//! herdr-pane-status: marks the panes whose agent finished or needs you,
//! on the pane border and the tab label.
//!
//! One binary: `ensure` is the herdr hook that starts the daemon if it isn't
//! running, `daemon` is the daemon, `stop` asks a running daemon to shut down.

pub mod client;
pub mod config;
pub mod core;
pub mod daemon;
pub mod files;
pub mod launcher;
pub mod lock;
pub mod paths;
pub mod transport;
