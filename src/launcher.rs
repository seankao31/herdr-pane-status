//! The three subcommands. herdr runs `ensure` as the plugin's hook, so the
//! common case (daemon already running) is one lock probe and an exit.

use std::env;
use std::fs;
use std::io;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::HerdrClient;
use crate::config;
use crate::daemon::{self, Daemon};
use crate::files::FileLog;
use crate::lock;
use crate::paths;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const STOP_WAIT: Duration = Duration::from_secs(3);

/// Start the daemon unless one is running. Always exits 0: a failed start
/// is reported on stderr (herdr's plugin log) and retried by the next hook.
pub fn ensure() -> u8 {
    let state_dir = paths::state_dir();
    if let Err(e) = fs::create_dir_all(&state_dir) {
        eprintln!("cannot create {}: {e}", state_dir.display());
        return 0;
    }
    if lock::daemon_running(&state_dir) {
        return 0;
    }
    if let Err(e) = spawn_daemon() {
        eprintln!("could not start the daemon: {e}");
    }
    0
}

fn spawn_daemon() -> io::Result<Child> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("daemon")
        // Explicit, so a daemon started by hand uses herdr's folders too.
        .env(paths::STATE_DIR_VAR, paths::state_dir())
        .env(paths::CONFIG_DIR_VAR, paths::config_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach(&mut command);
    command.spawn()
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};

    // A child inherits every inheritable handle, and herdr waits for the
    // hook's output pipes to close. If the daemon held them, the hook would
    // look like it never ends.
    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        unsafe { SetHandleInformation(GetStdHandle(which), HANDLE_FLAG_INHERIT, 0) };
    }
    // No console (so no window), and outside the hook's Ctrl+C group.
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(windows))]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

/// Run the daemon in this process until it is stopped or herdr goes away.
pub fn run_daemon() -> u8 {
    let state_dir = paths::state_dir();
    if let Err(e) = fs::create_dir_all(&state_dir) {
        eprintln!("cannot create {}: {e}", state_dir.display());
        return 1;
    }
    let _lock = match lock::acquire(&state_dir) {
        Ok(Some(lock)) => lock,
        Ok(None) => return 0, // another daemon is running
        Err(e) => {
            eprintln!("cannot lock {}: {e}", state_dir.display());
            return 1;
        }
    };
    let log = FileLog::new(state_dir.join(daemon::LOG_FILE_NAME));
    let cfg = config::load(&paths::config_dir());
    let client = HerdrClient::new(paths::socket_path(), REQUEST_TIMEOUT);
    let mut daemon = Daemon::new(
        client,
        cfg,
        state_dir,
        Box::new(move |message| log.log(message)),
    );
    match daemon.run() {
        Ok(()) => 0,
        Err(_) => 1, // logged by the daemon
    }
}

/// Ask a running daemon to shut down cleanly, and wait for it. Signals
/// can't reach a detached daemon on Windows; the stop file works anywhere.
pub fn stop() -> u8 {
    let state_dir = paths::state_dir();
    let stop_file = state_dir.join(daemon::STOP_FILE_NAME);
    if !lock::daemon_running(&state_dir) {
        let _ = fs::remove_file(&stop_file); // left by a daemon that died first
        println!("not running (state dir: {})", state_dir.display());
        return 0;
    }
    if let Err(e) = fs::write(&stop_file, b"") {
        eprintln!("cannot create {}: {e}", stop_file.display());
        return 1;
    }
    let deadline = Instant::now() + STOP_WAIT;
    while Instant::now() < deadline {
        if !lock::daemon_running(&state_dir) {
            let _ = fs::remove_file(&stop_file);
            println!("stopped");
            return 0;
        }
        thread::sleep(Duration::from_millis(50));
    }
    eprintln!("still running after {}s", STOP_WAIT.as_secs());
    1
}
