//! Where things are. herdr passes `HERDR_PLUGIN_STATE_DIR`,
//! `HERDR_PLUGIN_CONFIG_DIR` and `HERDR_SOCKET_PATH` to plugin commands;
//! the defaults below are herdr's own, for running a command by hand.

use std::env;
use std::path::PathBuf;

pub const PLUGIN_ID: &str = "seankao31.herdr-pane-status";

pub const STATE_DIR_VAR: &str = "HERDR_PLUGIN_STATE_DIR";
pub const CONFIG_DIR_VAR: &str = "HERDR_PLUGIN_CONFIG_DIR";
pub const SOCKET_PATH_VAR: &str = "HERDR_SOCKET_PATH";

fn var(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn home() -> PathBuf {
    var("HOME")
        .or_else(|| var("USERPROFILE"))
        .unwrap_or_default()
}

pub fn state_dir() -> PathBuf {
    var(STATE_DIR_VAR).unwrap_or_else(default_state_dir)
}

pub fn config_dir() -> PathBuf {
    var(CONFIG_DIR_VAR).unwrap_or_else(default_config_dir)
}

pub fn socket_path() -> String {
    if let Some(path) = env::var_os(SOCKET_PATH_VAR).filter(|v| !v.is_empty()) {
        return path.to_string_lossy().into_owned();
    }
    default_socket_path().to_string_lossy().into_owned()
}

#[cfg(windows)]
fn default_state_dir() -> PathBuf {
    var("LOCALAPPDATA")
        .unwrap_or_else(|| home().join("AppData").join("Local"))
        .join("herdr")
        .join("plugins")
        .join(PLUGIN_ID)
}

#[cfg(not(windows))]
fn default_state_dir() -> PathBuf {
    home().join(".local/state/herdr/plugins").join(PLUGIN_ID)
}

#[cfg(windows)]
fn default_config_dir() -> PathBuf {
    appdata()
        .join("herdr")
        .join("plugins")
        .join("config")
        .join(PLUGIN_ID)
}

#[cfg(not(windows))]
fn default_config_dir() -> PathBuf {
    home().join(".config/herdr/plugins/config").join(PLUGIN_ID)
}

#[cfg(windows)]
fn default_socket_path() -> PathBuf {
    appdata().join("herdr").join("herdr.sock")
}

#[cfg(not(windows))]
fn default_socket_path() -> PathBuf {
    home().join(".config/herdr/herdr.sock")
}

#[cfg(windows)]
fn appdata() -> PathBuf {
    var("APPDATA").unwrap_or_else(|| home().join("AppData").join("Roaming"))
}
