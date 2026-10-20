use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use super::*;
use crate::files::test_dir;

type Handler = Box<dyn FnMut(&Value) -> Result<Value, HerdrError> + Send>;

#[derive(Default)]
struct FakeState {
    handlers: HashMap<String, Handler>,
    requests: Vec<(String, Value)>,
    subscribes: Vec<Value>,
    events: VecDeque<Option<Value>>,
}

/// herdr in memory: canned answers per method, and every request recorded.
#[derive(Clone, Default)]
struct Fake(Arc<Mutex<FakeState>>);

impl Fake {
    fn on(
        &self,
        method: &str,
        handler: impl FnMut(&Value) -> Result<Value, HerdrError> + Send + 'static,
    ) {
        self.0
            .lock()
            .unwrap()
            .handlers
            .insert(method.to_string(), Box::new(handler));
    }

    fn panes(&self, panes: Vec<Value>) {
        self.on("pane.list", move |_| Ok(json!({"panes": panes.clone()})));
    }

    fn requests(&self, method: &str) -> Vec<Value> {
        let state = self.0.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn reports(&self, pane_id: &str) -> Vec<Value> {
        self.requests("pane.report_metadata")
            .into_iter()
            .filter(|p| p["pane_id"] == pane_id)
            .collect()
    }

    /// The pane's last report: its title, or "<cleared>".
    fn last_title(&self, pane_id: &str) -> Option<String> {
        self.reports(pane_id).last().map(|p| {
            if p["clear_title"] == true {
                "<cleared>".to_string()
            } else {
                p["title"].as_str().unwrap().to_string()
            }
        })
    }
}

impl Api for Fake {
    type Events = FakeEvents;

    fn request(&mut self, method: &str, params: Value) -> Result<Value, HerdrError> {
        let mut state = self.0.lock().unwrap();
        state.requests.push((method.to_string(), params.clone()));
        match state.handlers.get_mut(method) {
            Some(handler) => handler(&params),
            None => Ok(json!({})),
        }
    }

    fn subscribe(&mut self, subscriptions: Value) -> Result<FakeEvents, HerdrError> {
        self.0.lock().unwrap().subscribes.push(subscriptions);
        Ok(FakeEvents(self.0.clone()))
    }
}

struct FakeEvents(Arc<Mutex<FakeState>>);

impl EventSource for FakeEvents {
    fn next_event(&mut self, _deadline: Instant) -> Result<Option<Value>, Closed> {
        match self.0.lock().unwrap().events.pop_front() {
            Some(Some(event)) => Ok(Some(event)),
            Some(None) => Err(Closed),
            None => Ok(None),
        }
    }
}

fn pane(pane_id: &str, tab_id: &str, status: &str) -> Value {
    json!({"pane_id": pane_id, "tab_id": tab_id, "workspace_id": "w1", "agent_status": status, "agent": "claude"})
}

fn with(mut info: Value, key: &str, value: Value) -> Value {
    info[key] = value;
    info
}

struct Harness {
    herdr: Fake,
    daemon: Daemon<Fake>,
    log: Arc<Mutex<Vec<String>>>,
    now: Instant,
}

impl Harness {
    fn new(panes: Vec<Value>) -> Self {
        Self::with_config(panes, Config::default(), test_dir("daemon"))
    }

    fn with_config(panes: Vec<Value>, cfg: Config, state_dir: PathBuf) -> Self {
        let herdr = Fake::default();
        herdr.panes(panes);
        let log = Arc::new(Mutex::new(Vec::new()));
        let sink = log.clone();
        let mut daemon = Daemon::new(
            herdr.clone(),
            cfg,
            state_dir,
            Box::new(move |m| sink.lock().unwrap().push(m.to_string())),
        );
        daemon.bootstrap().unwrap();
        let mut harness = Harness {
            herdr,
            daemon,
            log,
            now: Instant::now(),
        };
        harness.sync();
        harness
    }

    fn sync(&mut self) {
        self.daemon.sync(self.now);
    }

    fn event(&mut self, event: &str, data: Value) {
        self.daemon
            .handle_event(&json!({"event": event, "data": data}));
        self.sync();
    }

    fn status(&mut self, pane_id: &str, status: &str) {
        self.event(
            "pane.agent_status_changed",
            json!({"pane_id": pane_id, "agent_status": status}),
        );
    }

    fn focus(&mut self, pane_id: &str) {
        self.event("pane.focused", json!({"pane_id": pane_id}));
    }

    fn logged(&self, needle: &str) -> bool {
        self.log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(needle))
    }
}

/// reviewer (focused) and docs, both working.
fn two_working() -> Harness {
    Harness::new(vec![
        with(
            with(
                with(
                    pane("w1:p1", "w1:t1", "working"),
                    "label",
                    json!("reviewer"),
                ),
                "focused",
                json!(true),
            ),
            "terminal_title_stripped",
            json!("fix login"),
        ),
        with(
            pane("w1:p2", "w1:t1", "working"),
            "terminal_title_stripped",
            json!("write docs"),
        ),
    ])
}

// -- subscriptions --

#[test]
fn subscribes_to_lifecycle_focus_and_each_pane() {
    let h = two_working();
    let subscribes = h.herdr.0.lock().unwrap().subscribes.clone();
    let types: Vec<String> = subscribes[0]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            format!(
                "{} {}",
                s["type"].as_str().unwrap(),
                s["pane_id"].as_str().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(
        types,
        [
            "pane.created ",
            "pane.closed ",
            "pane.exited ",
            "pane.focused ",
            "pane.agent_status_changed w1:p1",
            "pane.agent_status_changed w1:p2"
        ]
    );
}

#[test]
fn a_new_pane_is_cached_and_needs_a_resubscribe() {
    let mut h = two_working();
    assert!(!h.daemon.topology_dirty());
    h.event(
        "pane.created",
        json!({"pane": pane("w1:p3", "w1:t1", "idle")}),
    );
    assert!(h.daemon.panes.contains_key("w1:p3"));
    assert!(h.daemon.topology_dirty());
    h.event("pane.closed", json!({"pane_id": "w1:p3"}));
    assert!(!h.daemon.panes.contains_key("w1:p3"));
}

// -- the done marker --

#[test]
fn a_pane_that_finishes_out_of_focus_shows_done_until_focused() {
    let mut h = two_working();
    h.status("w1:p2", "idle"); // active tab: herdr skips "done"
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("✓ write docs"));
    h.focus("w1:p2");
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("<cleared>"));
}

#[test]
fn the_focused_pane_finishing_in_view_gets_no_marker() {
    let mut h = two_working();
    h.status("w1:p1", "idle");
    assert_eq!(h.herdr.last_title("w1:p1").as_deref(), Some("<cleared>"));
}

#[test]
fn the_focused_pane_finishing_while_the_terminal_is_unfocused_is_marked_until_seen() {
    let mut h = two_working();
    h.status("w1:p1", "done");
    assert_eq!(
        h.herdr.last_title("w1:p1").as_deref(),
        Some("✓ reviewer · fix login")
    );
    h.status("w1:p1", "idle"); // the terminal got focus back
    assert_eq!(h.herdr.last_title("w1:p1").as_deref(), Some("<cleared>"));
}

#[test]
fn switching_to_the_tab_keeps_the_marker_on_panes_not_focused() {
    let mut h = two_working();
    h.status("w1:p2", "done");
    h.status("w1:p2", "idle"); // herdr marks the whole tab seen
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("✓ write docs"));
}

#[test]
fn new_work_replaces_the_marker_and_a_gone_agent_drops_it() {
    let mut h = two_working();
    h.status("w1:p2", "idle");
    h.status("w1:p2", "working");
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("◐ write docs"));
    h.status("w1:p2", "idle");
    h.status("w1:p2", "unknown");
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("<cleared>"));
}

#[test]
fn our_own_title_echo_changes_nothing() {
    let mut h = two_working();
    h.status("w1:p2", "idle");
    let before = h.herdr.reports("w1:p2").len();
    h.status("w1:p2", "idle"); // the echo of our report
    assert!(h.daemon.panes["w1:p2"].finished);
    assert_eq!(h.herdr.reports("w1:p2").len(), before);
}

#[test]
fn panes_herdr_reports_done_at_startup_are_marked_and_others_are_not() {
    let h = Harness::new(vec![
        pane("w1:p1", "w1:t1", "done"),
        pane("w1:p2", "w1:t1", "idle"),
        with(pane("w1:p3", "w1:t1", "unknown"), "agent", Value::Null),
    ]);
    assert_eq!(h.herdr.last_title("w1:p1").as_deref(), Some("✓"));
    assert_eq!(h.herdr.last_title("w1:p2"), None);
    assert_eq!(h.herdr.last_title("w1:p3"), None);
}

#[test]
fn markers_survive_a_restart() {
    let state_dir = test_dir("daemon-restart");
    let mut first = Harness::with_config(two_working_panes(), Config::default(), state_dir.clone());
    first.status("w1:p2", "idle");
    first.daemon.shutdown();

    let idle = vec![
        pane("w1:p1", "w1:t1", "idle"),
        pane("w1:p2", "w1:t1", "idle"),
    ];
    let second = Harness::with_config(idle, Config::default(), state_dir);
    assert!(second.daemon.panes["w1:p2"].finished);
    assert!(!second.daemon.panes["w1:p1"].finished);
    assert_eq!(second.herdr.last_title("w1:p2").as_deref(), Some("✓"));
}

fn two_working_panes() -> Vec<Value> {
    vec![
        with(pane("w1:p1", "w1:t1", "working"), "focused", json!(true)),
        pane("w1:p2", "w1:t1", "working"),
    ]
}

#[test]
fn a_refresh_applies_a_finish_the_events_missed() {
    let mut h = two_working();
    h.herdr.panes(vec![
        with(pane("w1:p1", "w1:t1", "working"), "focused", json!(true)),
        pane("w1:p2", "w1:t1", "idle"),
    ]);
    h.daemon.refresh();
    assert!(h.daemon.panes["w1:p2"].finished);
}

#[test]
fn a_refresh_that_sees_focus_move_clears_that_pane() {
    let mut h = two_working();
    h.status("w1:p2", "idle");
    h.herdr.panes(vec![
        pane("w1:p1", "w1:t1", "working"),
        with(pane("w1:p2", "w1:t1", "idle"), "focused", json!(true)),
    ]);
    h.daemon.refresh();
    assert!(!h.daemon.panes["w1:p2"].finished);
}

#[test]
fn a_refresh_keeps_a_focused_pane_herdr_still_calls_done() {
    let mut h = two_working();
    h.status("w1:p1", "done");
    h.herdr.panes(vec![
        with(pane("w1:p1", "w1:t1", "done"), "focused", json!(true)),
        pane("w1:p2", "w1:t1", "working"),
    ]);
    h.daemon.refresh();
    assert!(h.daemon.panes["w1:p1"].finished);
}

#[test]
fn a_refresh_picks_up_new_task_text_and_labels() {
    let mut h = two_working();
    h.herdr.panes(vec![
        with(pane("w1:p1", "w1:t1", "working"), "focused", json!(true)),
        with(
            with(pane("w1:p2", "w1:t1", "working"), "label", json!("docs")),
            "terminal_title_stripped",
            json!("v2"),
        ),
    ]);
    h.daemon.refresh();
    h.sync();
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("◐ docs · v2"));
}

// -- titles --

#[test]
fn titles_carry_source_agent_guard_ttl_and_a_wall_clock_seq() {
    let h = two_working();
    let reports = h.herdr.reports("w1:p1");
    let first = &reports[0];
    assert_eq!(first["source"], SOURCE);
    assert_eq!(first["agent"], "claude");
    assert_eq!(first["ttl_ms"], TITLE_TTL.as_millis() as u64);
    assert!(first["seq"].as_u64().unwrap() > 1_000_000_000_000);
    let seqs: Vec<u64> = h
        .herdr
        .requests("pane.report_metadata")
        .iter()
        .map(|p| p["seq"].as_u64().unwrap())
        .collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq must strictly increase: {seqs:?}"
    );
}

#[test]
fn an_unchanged_title_is_resent_only_before_it_expires() {
    let mut h = two_working();
    h.sync();
    h.sync();
    assert_eq!(h.herdr.reports("w1:p2").len(), 1);
    h.now += TITLE_REFRESH_AFTER;
    h.sync();
    assert_eq!(h.herdr.reports("w1:p2").len(), 2);
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("◐ write docs"));
}

#[test]
fn a_failed_report_is_retried_on_the_next_sync() {
    let herdr_fails = Arc::new(Mutex::new(true));
    let mut h = two_working();
    let flag = herdr_fails.clone();
    h.herdr.on("pane.report_metadata", move |_| {
        if *flag.lock().unwrap() {
            Err(HerdrError::Api {
                code: "busy".into(),
                message: "later".into(),
            })
        } else {
            Ok(json!({}))
        }
    });
    h.status("w1:p2", "idle");
    let tries = h.herdr.reports("w1:p2").len();
    *herdr_fails.lock().unwrap() = false;
    h.sync();
    assert_eq!(h.herdr.reports("w1:p2").len(), tries + 1);
    h.sync();
    assert_eq!(h.herdr.reports("w1:p2").len(), tries + 1);
    assert!(h.logged("request failed: method=pane.report_metadata"));
}

#[test]
fn show_working_off_marks_only_blocked_and_done() {
    let cfg = Config {
        show_working: false,
    };
    let mut h = Harness::with_config(two_working_panes(), cfg, test_dir("daemon-noworking"));
    assert_eq!(h.herdr.requests("pane.report_metadata").len(), 0);
    h.status("w1:p2", "blocked");
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("×"));
    h.status("w1:p2", "working");
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("<cleared>"));
}

// -- shutdown --

#[test]
fn shutdown_clears_our_titles() {
    let mut h = two_working();
    h.daemon.shutdown();
    assert_eq!(h.herdr.last_title("w1:p1").as_deref(), Some("<cleared>"));
    assert_eq!(h.herdr.last_title("w1:p2").as_deref(), Some("<cleared>"));
}

#[test]
fn an_error_line_from_herdr_is_logged() {
    let mut h = two_working();
    h.daemon.handle_event(
        &json!({"id": "sub_1", "error": {"code": "events_lost", "message": "resync"}}),
    );
    assert!(h.logged("events_lost"));
    h.sync();
}
