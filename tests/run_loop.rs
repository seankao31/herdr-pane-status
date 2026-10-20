//! The daemon's run loop, with the real client, against a fake herdr.

mod common;

use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use common::{FakeHerdr, temp_dir, wait_until};
use herdr_pane_status::client::HerdrClient;
use herdr_pane_status::config::Config;
use herdr_pane_status::daemon::{self, Daemon};
use serde_json::{Value, json};

struct Running {
    server: FakeHerdr,
    state_dir: std::path::PathBuf,
    log: Arc<Mutex<Vec<String>>>,
    thread: Option<JoinHandle<()>>,
}

fn start(panes: Value) -> Running {
    let server = FakeHerdr::start();
    server.panes(panes);
    let state_dir = temp_dir("run-loop");
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = log.clone();
    let client = HerdrClient::new(server.path.clone(), Duration::from_secs(2));
    let mut daemon = Daemon::new(
        client,
        Config::default(),
        state_dir.clone(),
        Box::new(move |m| sink.lock().unwrap().push(m.into())),
    );
    let thread = thread::spawn(move || {
        let _ = daemon.run();
    });
    assert!(
        server.wait_for_subscriptions(1, Duration::from_secs(5)),
        "the daemon never subscribed"
    );
    Running {
        server,
        state_dir,
        log,
        thread: Some(thread),
    }
}

impl Running {
    fn titles(&self, pane_id: &str) -> Vec<String> {
        self.server
            .requests("pane.report_metadata")
            .iter()
            .filter(|p| p["pane_id"] == pane_id)
            .map(|p| {
                if p["clear_title"] == true {
                    "<cleared>".into()
                } else {
                    p["title"].as_str().unwrap().into()
                }
            })
            .collect()
    }

    fn logged(&self, needle: &str) -> bool {
        self.log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
    }

    /// Wait for the daemon thread to end; false if it is still running.
    fn ended(&mut self, timeout: Duration) -> bool {
        let thread = self.thread.as_ref().unwrap();
        if !wait_until(timeout, || thread.is_finished()) {
            return false;
        }
        self.thread.take().unwrap().join().unwrap();
        true
    }

    fn herdr_stops_answering(&self) {
        self.server.on(
            "pane.list",
            |_| json!({"__error__": {"code": "server_unavailable", "message": "gone"}}),
        );
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if self.thread.is_some() {
            let _ = std::fs::write(self.state_dir.join(daemon::STOP_FILE_NAME), b"");
            self.ended(Duration::from_secs(3));
        }
    }
}

fn working() -> Value {
    json!([{"pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "working", "agent": "claude"}])
}

#[test]
fn a_pushed_status_change_shows_at_once() {
    let running = start(working());
    let start = Instant::now();
    running.server.push(0, json!({"event": "pane.agent_status_changed", "data": {"pane_id": "w1:p1", "agent_status": "idle"}}));
    assert!(
        wait_until(Duration::from_secs(1), || running
            .titles("w1:p1")
            .last()
            .map(String::as_str)
            == Some("✓")),
        "{:?}",
        running.titles("w1:p1")
    );
    // Not on the next once-a-second refresh: as soon as the event arrives.
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "{:?}",
        start.elapsed()
    );
}

#[test]
fn the_stop_file_shuts_it_down_cleanly() {
    let mut running = start(working());
    assert!(wait_until(Duration::from_secs(2), || !running
        .titles("w1:p1")
        .is_empty()));
    let stop_file = running.state_dir.join(daemon::STOP_FILE_NAME);
    std::fs::write(&stop_file, b"").unwrap();
    assert!(running.ended(Duration::from_secs(3)), "still running");
    assert!(!stop_file.exists());
    assert_eq!(running.titles("w1:p1").last().unwrap(), "<cleared>");
    assert!(running.logged("stop file"));
}

#[test]
fn it_resyncs_and_resubscribes_when_herdr_drops_the_subscription() {
    let mut running = start(working());
    running.server.panes(json!([
        {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "working", "agent": "claude"},
        {"pane_id": "w1:p2", "tab_id": "w1:t1", "agent_status": "blocked", "agent": "claude"},
    ]));
    // herdr keeps 512 events of history; a subscriber that falls behind gets
    // this line and is disconnected while herdr keeps running.
    running.server.push(
        0,
        json!({"id": "sub_1", "error": {"code": "events_lost", "message": "resubscribe"}}),
    );
    running.server.close_subscription(0);
    assert!(
        running
            .server
            .wait_for_subscriptions(2, Duration::from_secs(3))
    );
    let last = running
        .server
        .requests("events.subscribe")
        .last()
        .unwrap()
        .clone();
    let panes: Vec<&str> = last["subscriptions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["pane_id"].as_str())
        .collect();
    assert_eq!(panes, ["w1:p1", "w1:p2"]);
    assert!(wait_until(Duration::from_secs(1), || running
        .titles("w1:p2")
        .last()
        .map(String::as_str)
        == Some("×")));
    assert!(running.logged("events_lost"));

    running.herdr_stops_answering();
    running.server.close_subscription(1);
    assert!(running.ended(Duration::from_secs(3)), "still running");
    assert!(running.logged("no longer answers"));
}

#[test]
fn it_resubscribes_at_most_once_a_second() {
    let mut running = start(working());
    running.server.close_subscription(0);
    assert!(
        running
            .server
            .wait_for_subscriptions(2, Duration::from_secs(3))
    );
    let first_retry = Instant::now();
    running.server.close_subscription(1);
    assert!(
        running
            .server
            .wait_for_subscriptions(3, Duration::from_secs(3))
    );
    assert!(
        first_retry.elapsed() >= Duration::from_millis(800),
        "{:?}",
        first_retry.elapsed()
    );
    running.herdr_stops_answering();
    running.server.close_subscription(2);
    assert!(running.ended(Duration::from_secs(3)));
}

#[test]
fn it_reads_pane_list_about_once_a_second_while_idle() {
    let running = start(working());
    let before = running.server.requests("pane.list").len();
    thread::sleep(Duration::from_millis(2500));
    let during = running.server.requests("pane.list").len() - before;
    assert!(
        (2..=3).contains(&during),
        "{during} pane.list requests in 2.5s"
    );
}
