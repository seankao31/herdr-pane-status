//! The binary as herdr runs it: `ensure` as a hook, `stop` by hand.

mod common;

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{FakeHerdr, temp_dir, wait_until};
use herdr_pane_status::{daemon, lock, paths};
use serde_json::json;

const EXE: &str = env!("CARGO_BIN_EXE_herdr-pane-status");

struct Plugin {
    server: FakeHerdr,
    state_dir: PathBuf,
    config_dir: PathBuf,
}

impl Plugin {
    fn new() -> Self {
        let server = FakeHerdr::start();
        server.panes(json!([{"pane_id": "w1:p1", "tab_id": "w1:t1", "agent_status": "idle"}]));
        Plugin {
            server,
            state_dir: temp_dir("state"),
            config_dir: temp_dir("config"),
        }
    }

    fn command(&self, arg: &str) -> Command {
        let mut command = Command::new(EXE);
        command
            .arg(arg)
            .env(paths::SOCKET_PATH_VAR, &self.server.path)
            .env(paths::STATE_DIR_VAR, &self.state_dir)
            .env(paths::CONFIG_DIR_VAR, &self.config_dir);
        command
    }

    /// Run like herdr runs a hook: output captured through pipes. None if
    /// it hasn't finished (pipes and all) within the timeout.
    fn run(&self, arg: &str, timeout: Duration) -> Option<Output> {
        let child = self
            .command(arg)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(child.wait_with_output());
        });
        rx.recv_timeout(timeout).ok().map(Result::unwrap)
    }

    fn running(&self) -> bool {
        lock::daemon_running(&self.state_dir)
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        if self.running() {
            let _ = self.run("stop", Duration::from_secs(5));
        }
    }
}

#[test]
fn ensure_returns_at_once_and_the_daemon_keeps_running() {
    let plugin = Plugin::new();
    // herdr waits for the hook's output pipes to close; the daemon must not
    // hold them.
    let output = plugin
        .run("ensure", Duration::from_secs(5))
        .expect("ensure never finished");
    assert!(output.status.success(), "{output:?}");
    assert!(
        plugin
            .server
            .wait_for_subscriptions(1, Duration::from_secs(5)),
        "the daemon never connected"
    );
    assert!(plugin.running());

    let stop = plugin
        .run("stop", Duration::from_secs(5))
        .expect("stop never finished");
    assert!(stop.status.success(), "{stop:?}");
    assert!(!plugin.running());
    assert!(!plugin.state_dir.join(daemon::STOP_FILE_NAME).exists());
    let log = std::fs::read_to_string(plugin.state_dir.join(daemon::LOG_FILE_NAME)).unwrap();
    assert!(log.contains("stop file found"), "{log}");
}

#[test]
fn ensure_leaves_a_running_daemon_alone() {
    let plugin = Plugin::new();
    plugin.run("ensure", Duration::from_secs(5)).unwrap();
    assert!(
        plugin
            .server
            .wait_for_subscriptions(1, Duration::from_secs(5))
    );
    for _ in 0..3 {
        assert!(
            plugin
                .run("ensure", Duration::from_secs(5))
                .unwrap()
                .status
                .success()
        );
    }
    thread::sleep(Duration::from_millis(300));
    assert_eq!(plugin.server.subscription_count(), 1);
}

#[test]
fn concurrent_hooks_start_exactly_one_daemon() {
    let plugin = Plugin::new();
    thread::scope(|scope| {
        for _ in 0..16 {
            scope.spawn(|| {
                plugin
                    .run("ensure", Duration::from_secs(10))
                    .expect("ensure never finished")
            });
        }
    });
    assert!(
        plugin
            .server
            .wait_for_subscriptions(1, Duration::from_secs(5))
    );
    thread::sleep(Duration::from_millis(500));
    assert_eq!(plugin.server.subscription_count(), 1);
}

#[test]
fn stop_without_a_daemon_clears_a_stale_stop_file() {
    let plugin = Plugin::new();
    let stop_file = plugin.state_dir.join(daemon::STOP_FILE_NAME);
    std::fs::write(&stop_file, b"").unwrap();
    let output = plugin.run("stop", Duration::from_secs(5)).unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("not running"));
    assert!(!stop_file.exists());
}

#[test]
fn a_daemon_without_herdr_logs_and_exits() {
    let mut plugin = Plugin::new();
    plugin.server.stop();
    plugin.run("ensure", Duration::from_secs(5)).unwrap();
    let log_path = plugin.state_dir.join(daemon::LOG_FILE_NAME);
    assert!(wait_until(Duration::from_secs(5), || {
        std::fs::read_to_string(&log_path).is_ok_and(|log| log.contains("could not reach herdr"))
    }));
    assert!(wait_until(Duration::from_secs(2), || !plugin.running()));
}

#[test]
fn unknown_arguments_print_usage() {
    let output = Command::new(EXE).arg("bogus").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("usage"));
}
