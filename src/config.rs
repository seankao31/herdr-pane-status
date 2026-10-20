//! Optional `config.json` in the plugin's config directory. A missing file,
//! missing keys or bad JSON fall back to the defaults.

use std::fs;
use std::path::Path;

use serde_json::Value;

pub const CONFIG_FILE_NAME: &str = "config.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// Show `◐` on working panes' borders, not only blocked and done ones.
    pub show_working: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { show_working: true }
    }
}

pub fn load(dir: &Path) -> Config {
    let data = fs::read(dir.join(CONFIG_FILE_NAME))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .unwrap_or(Value::Null);
    let defaults = Config::default();
    let flag = |key: &str, default: bool| data.get(key).and_then(Value::as_bool).unwrap_or(default);
    Config {
        show_working: flag("show_working", defaults.show_working),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(contents: Option<&str>) -> std::path::PathBuf {
        let dir = crate::files::test_dir("config");
        if let Some(contents) = contents {
            fs::write(dir.join(CONFIG_FILE_NAME), contents).unwrap();
        }
        dir
    }

    #[test]
    fn missing_file_gives_defaults() {
        assert_eq!(load(&dir_with(None)), Config::default());
    }

    #[test]
    fn reads_show_working() {
        let cfg = load(&dir_with(Some(r#"{"show_working": false}"#)));
        assert_eq!(
            cfg,
            Config {
                show_working: false
            }
        );
    }

    #[test]
    fn mistyped_or_unknown_keys_fall_back() {
        let cfg = load(&dir_with(Some(
            r#"{"show_working": "no", "tab_icons": false}"#,
        )));
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn bad_json_falls_back() {
        assert_eq!(load(&dir_with(Some("{not json"))), Config::default());
        assert_eq!(load(&dir_with(Some("[1, 2]"))), Config::default());
    }
}
