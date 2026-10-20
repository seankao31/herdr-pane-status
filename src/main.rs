use std::process::ExitCode;

use herdr_pane_status::launcher;

const USAGE: &str = "usage: herdr-pane-status <ensure|daemon|stop>

  ensure   start the daemon unless it is running (the herdr hook)
  daemon   run the daemon in this process
  stop     ask the running daemon to shut down cleanly";

fn main() -> ExitCode {
    let code = match std::env::args().nth(1).as_deref() {
        Some("ensure") => launcher::ensure(),
        Some("daemon") => launcher::run_daemon(),
        Some("stop") => launcher::stop(),
        Some("--version" | "-V") => {
            println!("herdr-pane-status {}", env!("CARGO_PKG_VERSION"));
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    ExitCode::from(code)
}
