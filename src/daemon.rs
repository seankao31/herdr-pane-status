//! The daemon: follows herdr's pane statuses and focus, and shows a marker
//! on each pane's border.
//!
//! herdr (verified against v0.9.5 src/api) gives no "every pane" filter for
//! `pane.agent_status_changed`, and a connection's subscriptions are fixed
//! for its lifetime, so the daemon subscribes per known pane and
//! resubscribes when the set of panes changes. herdr also closes a
//! subscription that falls more than 512 events behind (`events_lost`)
//! while it keeps running; the daemon then resyncs from `pane.list` and
//! resubscribes, and shuts down only once herdr stops answering.
//!
//! Every new title herdr accepts fires `pane.agent_status_changed` for
//! every listener, other plugins' hooks included, so titles change only
//! when what they say changes. An unchanged title fires nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::client::{Api, Closed, EventSource, HerdrError};
use crate::config::Config;
use crate::core::{self, Marker};
use crate::files;

#[cfg(test)]
mod tests;

pub const SOURCE: &str = "herdr-pane-status";
pub const STOP_FILE_NAME: &str = "stop";
pub const FINISHED_FILE_NAME: &str = "finished.json";
pub const LOG_FILE_NAME: &str = "daemon.log";

/// A reported title expires after this, so a dead daemon's titles clear.
pub const TITLE_TTL: Duration = Duration::from_secs(4);
/// An unchanged title is sent again once it is this old.
pub const TITLE_REFRESH_AFTER: Duration = Duration::from_secs(2);
/// How often `pane.list` is read again: task text, labels, missed events.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(1);
pub const RESUBSCRIBE_MIN_INTERVAL: Duration = Duration::from_secs(1);

pub type Log = Box<dyn FnMut(&str) + Send>;

/// Lifecycle and focus events (no pane id needed), plus one status filter
/// per known pane.
pub fn build_subscriptions<'a, I: IntoIterator<Item = &'a String>>(pane_ids: I) -> Value {
    let mut subscriptions: Vec<Value> =
        ["pane.created", "pane.closed", "pane.exited", "pane.focused"]
            .into_iter()
            .map(|kind| json!({"type": kind}))
            .collect();
    for pane_id in pane_ids {
        subscriptions.push(json!({"type": "pane.agent_status_changed", "pane_id": pane_id}));
    }
    Value::Array(subscriptions)
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

#[derive(Clone, Debug)]
pub struct PaneRecord {
    pub pane_id: String,
    pub status: String,
    pub agent: Option<String>,
    /// The pane's terminal title without its leading glyph.
    pub task_text: Option<String>,
    /// The pane's own label; herdr shows our title in place of it.
    pub label: Option<String>,
    /// The agent finished and nobody has focused the pane since.
    pub finished: bool,
    /// We own a title on this pane that must be cleared eventually.
    reporting: bool,
    /// The last title herdr accepted, and when.
    reported: Option<(String, Instant)>,
}

impl PaneRecord {
    fn from_info(info: &Value) -> Option<Self> {
        let pane_id = text(info, "pane_id")?;
        let status = text(info, "agent_status").unwrap_or_else(|| "unknown".to_string());
        Some(PaneRecord {
            finished: status == "done",
            pane_id,
            status,
            agent: text(info, "agent"),
            task_text: text(info, "terminal_title_stripped"),
            label: text(info, "label"),
            reporting: false,
            reported: None,
        })
    }

    pub fn marker(&self) -> Option<Marker> {
        core::marker(&self.status, self.finished)
    }
}

fn read_finished(path: &Path) -> BTreeSet<String> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub struct Daemon<A: Api> {
    api: A,
    cfg: Config,
    state_dir: PathBuf,
    log: Log,
    pub panes: BTreeMap<String, PaneRecord>,
    pub focused: Option<String>,
    synced: bool,
    subscription: Option<A::Events>,
    subscribed: BTreeSet<String>,
    saved_finished: Option<BTreeSet<String>>,
    last_seq: u64,
    last_recovery: Option<Instant>,
    herdr_gone: bool,
}

impl<A: Api> Daemon<A> {
    pub fn new(api: A, cfg: Config, state_dir: PathBuf, log: Log) -> Self {
        Daemon {
            api,
            cfg,
            state_dir,
            log,
            panes: BTreeMap::new(),
            focused: None,
            synced: false,
            subscription: None,
            subscribed: BTreeSet::new(),
            saved_finished: None,
            last_seq: 0,
            last_recovery: None,
            herdr_gone: false,
        }
    }

    fn log(&mut self, message: &str) {
        (self.log)(message)
    }

    // -- lifecycle --

    pub fn run(&mut self) -> Result<(), HerdrError> {
        let started = format!(
            "started pid={} show_working={}",
            std::process::id(),
            self.cfg.show_working
        );
        self.log(&started);
        if let Err(e) = self.bootstrap() {
            self.log(&format!("could not reach herdr ({e}); exiting"));
            return Err(e);
        }
        self.sync(Instant::now());
        let mut next_refresh = Instant::now() + REFRESH_INTERVAL;
        loop {
            if self.stop_requested() {
                self.log("stop file found; shutting down");
                break;
            }
            let polled = match self.subscription.as_mut() {
                Some(subscription) => subscription.next_event(next_refresh),
                None => Err(Closed),
            };
            match polled {
                Ok(Some(event)) => {
                    self.handle_event(&event);
                    self.sync(Instant::now());
                }
                Ok(None) => {}
                Err(Closed) => {
                    self.subscription = None;
                    if !self.recover() {
                        break;
                    }
                    self.sync(Instant::now());
                }
            }
            let now = Instant::now();
            if now >= next_refresh {
                self.refresh();
                self.sync(now);
                next_refresh = now + REFRESH_INTERVAL;
            }
            if self.topology_dirty()
                && let Err(e) = self.resubscribe()
            {
                self.log(&format!("resubscribe failed ({e}); retrying"));
            }
        }
        self.shutdown();
        Ok(())
    }

    pub fn bootstrap(&mut self) -> Result<(), HerdrError> {
        let list = self.api.request("pane.list", json!({}))?;
        self.resync(&list);
        let saved = read_finished(&self.state_dir.join(FINISHED_FILE_NAME));
        for record in self.panes.values_mut() {
            if saved.contains(&record.pane_id) && matches!(record.status.as_str(), "idle" | "done")
            {
                record.finished = true;
            }
        }
        self.resubscribe()
    }

    /// After herdr closed the subscription. False when herdr no longer
    /// answers, i.e. the daemon should shut down.
    fn recover(&mut self) -> bool {
        if let Some(last) = self.last_recovery {
            let since = last.elapsed();
            if since < RESUBSCRIBE_MIN_INTERVAL {
                // herdr may keep closing it straight away; don't spin.
                thread::sleep(RESUBSCRIBE_MIN_INTERVAL - since);
            }
        }
        self.last_recovery = Some(Instant::now());
        match self.api.request("pane.list", json!({})) {
            Ok(list) => self.resync(&list),
            Err(e) => {
                self.herdr_gone = true;
                self.log(&format!(
                    "subscription closed and herdr no longer answers ({e}); shutting down"
                ));
                return false;
            }
        }
        match self.resubscribe() {
            Ok(()) => self.log("subscription closed by herdr; resynced and resubscribed"),
            Err(e) => self.log(&format!(
                "subscription closed by herdr; resubscribe failed ({e})"
            )),
        }
        true
    }

    fn stop_requested(&self) -> bool {
        let path = self.state_dir.join(STOP_FILE_NAME);
        if !path.exists() {
            return false;
        }
        let _ = fs::remove_file(&path);
        true
    }

    /// Clear our titles.
    pub fn shutdown(&mut self) {
        self.subscription = None;
        if !self.herdr_gone {
            let reporting: Vec<String> = self
                .panes
                .values()
                .filter(|r| r.reporting)
                .map(|r| r.pane_id.clone())
                .collect();
            for pane_id in reporting {
                self.clear_title(&pane_id);
            }
        }
        self.save_finished();
    }

    // -- herdr state --

    fn try_request(&mut self, method: &str, params: Value) -> Option<Value> {
        match self.api.request(method, params.clone()) {
            Ok(result) => Some(result),
            Err(e) => {
                self.log(&format!(
                    "request failed: method={method} params={params} error={e}"
                ));
                None
            }
        }
    }

    pub fn refresh(&mut self) {
        if let Some(list) = self.try_request("pane.list", json!({})) {
            self.resync(&list);
        }
    }

    /// Replace the pane cache with a fresh `pane.list` result, carrying our
    /// own per-pane state over and applying any status change since.
    pub fn resync(&mut self, list: &Value) {
        let infos = list
            .get("panes")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut fresh = BTreeMap::new();
        let mut focused = None;
        for info in infos {
            let Some(record) = PaneRecord::from_info(info) else {
                continue;
            };
            if info.get("focused").and_then(Value::as_bool) == Some(true) {
                focused = Some(record.pane_id.clone());
            }
            fresh.insert(record.pane_id.clone(), record);
        }
        // Focus moving to a pane counts as looking at it; a pane that stays
        // focused while herdr reports it done does not (the terminal itself
        // had no focus).
        let focus_moved = self.synced && focused != self.focused;
        self.focused = focused;
        self.synced = true;
        for record in fresh.values_mut() {
            let is_focused = self.focused.as_deref() == Some(record.pane_id.as_str());
            if let Some(previous) = self.panes.get(&record.pane_id) {
                record.reporting = previous.reporting;
                record.reported = previous.reported.clone();
                record.finished = core::finished_after(
                    previous.finished,
                    &previous.status,
                    &record.status,
                    is_focused,
                );
            }
            if focus_moved && is_focused {
                record.finished = false;
            }
        }
        self.panes = fresh;
    }

    pub fn resubscribe(&mut self) -> Result<(), HerdrError> {
        self.subscription = None; // close the old connection first
        let pane_ids: BTreeSet<String> = self.panes.keys().cloned().collect();
        self.subscription = Some(self.api.subscribe(build_subscriptions(&pane_ids))?);
        self.subscribed = pane_ids;
        Ok(())
    }

    pub fn topology_dirty(&self) -> bool {
        !self.panes.keys().eq(self.subscribed.iter())
    }

    pub fn handle_event(&mut self, envelope: &Value) {
        if let Some(error) = envelope.get("error") {
            // herdr's last line before it closes a subscription, e.g.
            // events_lost when we fell behind its event history.
            self.log(&format!("subscription error from herdr: {error}"));
            return;
        }
        let data = envelope.get("data").unwrap_or(&Value::Null);
        match envelope.get("event").and_then(Value::as_str) {
            Some("pane.agent_status_changed") => self.status_changed(data),
            Some("pane.created") => {
                if let Some(record) = data.get("pane").and_then(PaneRecord::from_info) {
                    self.panes.entry(record.pane_id.clone()).or_insert(record);
                }
            }
            Some("pane.closed" | "pane.exited") => {
                if let Some(pane_id) = data.get("pane_id").and_then(Value::as_str) {
                    self.panes.remove(pane_id);
                }
            }
            Some("pane.focused") => {
                let pane_id = text(data, "pane_id");
                if let Some(record) = pane_id.as_ref().and_then(|id| self.panes.get_mut(id)) {
                    record.finished = false;
                }
                self.focused = pane_id;
            }
            _ => {}
        }
    }

    fn status_changed(&mut self, data: &Value) {
        let (Some(pane_id), Some(new)) = (
            data.get("pane_id").and_then(Value::as_str),
            data.get("agent_status").and_then(Value::as_str),
        ) else {
            return;
        };
        let focused = self.focused.as_deref() == Some(pane_id);
        let Some(record) = self.panes.get_mut(pane_id) else {
            return;
        };
        if new == record.status {
            // Our own title reports re-fire this event; only a real status
            // change matters.
            return;
        }
        record.finished = core::finished_after(record.finished, &record.status, new, focused);
        record.status = new.to_string();
        if let Some(agent) = data.get("agent") {
            record.agent = agent.as_str().map(str::to_string);
        }
    }

    // -- what herdr shows --

    /// Bring titles and `finished.json` in line with the pane cache.
    /// Requests only what changed (or is about to expire).
    pub fn sync(&mut self, now: Instant) {
        self.report_titles(now);
        self.save_finished();
    }

    fn next_seq(&mut self) -> u64 {
        // herdr ignores a report whose seq isn't above the last one it took
        // for this (pane, source), and keeps that mark across our restarts,
        // so the seq is wall-clock milliseconds.
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_seq = wall.max(self.last_seq + 1);
        self.last_seq
    }

    fn wanted_title(&self, record: &PaneRecord) -> Option<String> {
        let marker = record.marker()?;
        if marker == Marker::Working && !self.cfg.show_working {
            return None;
        }
        Some(core::border_title(
            marker,
            record.label.as_deref(),
            record.task_text.as_deref(),
        ))
    }

    fn report_titles(&mut self, now: Instant) {
        let wanted: Vec<(String, Option<String>, bool)> = self
            .panes
            .values()
            .map(|r| (r.pane_id.clone(), self.wanted_title(r), r.reporting))
            .collect();
        for (pane_id, title, reporting) in wanted {
            match title {
                Some(title) => self.report_title(&pane_id, title, now),
                None if reporting => self.clear_title(&pane_id),
                None => {}
            }
        }
    }

    fn report_title(&mut self, pane_id: &str, title: String, now: Instant) {
        let Some(record) = self.panes.get(pane_id) else {
            return;
        };
        let fresh = record.reported.as_ref().is_some_and(|(sent, at)| {
            *sent == title && now.saturating_duration_since(*at) < TITLE_REFRESH_AFTER
        });
        if fresh {
            return;
        }
        let agent = record.agent.clone();
        let params = json!({
            "pane_id": pane_id,
            "source": SOURCE,
            "agent": agent,
            "title": title,
            "ttl_ms": TITLE_TTL.as_millis() as u64,
            "seq": self.next_seq(),
        });
        let accepted = self.try_request("pane.report_metadata", params).is_some();
        if let Some(record) = self.panes.get_mut(pane_id) {
            record.reporting = true;
            if accepted {
                record.reported = Some((title, now));
            }
        }
    }

    fn clear_title(&mut self, pane_id: &str) {
        let params = json!({
            "pane_id": pane_id,
            "source": SOURCE,
            "clear_title": true,
            "seq": self.next_seq(),
        });
        self.try_request("pane.report_metadata", params);
        if let Some(record) = self.panes.get_mut(pane_id) {
            record.reporting = false;
            record.reported = None;
        }
    }

    fn save_finished(&mut self) {
        let finished: BTreeSet<String> = self
            .panes
            .values()
            .filter(|r| r.finished)
            .map(|r| r.pane_id.clone())
            .collect();
        if self.saved_finished.as_ref() == Some(&finished) {
            return;
        }
        let body = serde_json::to_vec(&finished).expect("a string set always serializes");
        match files::write_atomic(&self.state_dir.join(FINISHED_FILE_NAME), &body) {
            Ok(()) => self.saved_finished = Some(finished),
            Err(e) => self.log(&format!("could not save finished panes: {e}")),
        }
    }
}
