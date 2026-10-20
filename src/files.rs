//! Small files in the state directory: atomic replaces and the log.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REPLACE_ATTEMPTS: u32 = 10;
const REPLACE_RETRY_DELAY: Duration = Duration::from_millis(20);
const LOG_MAX_BYTES: u64 = 256 * 1024;

/// Write `bytes` to a temp file, then rename it over `path`. On Windows the
/// rename fails while another process (the tab bar's `type`) has `path`
/// open, so retry for a moment.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, bytes)?;
    let mut attempt = 1;
    loop {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied && attempt < REPLACE_ATTEMPTS => {
                attempt += 1;
                thread::sleep(REPLACE_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Appends timestamped lines to `daemon.log`, keeping one rotated copy.
pub struct FileLog {
    path: PathBuf,
}

impl FileLog {
    pub fn new(path: PathBuf) -> Self {
        FileLog { path }
    }

    pub fn log(&self, message: &str) {
        if fs::metadata(&self.path).is_ok_and(|m| m.len() > LOG_MAX_BYTES) {
            let mut old = self.path.as_os_str().to_owned();
            old.push(".1");
            let _ = fs::rename(&self.path, PathBuf::from(old));
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{} {message}", utc_timestamp(SystemTime::now()));
        }
    }
}

/// `2026-10-20T17:46:16Z`.
pub fn utc_timestamp(at: SystemTime) -> String {
    let secs = at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
pub fn test_dir(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "hps-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_timestamps() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01T00:00:00Z");
        let at = UNIX_EPOCH + Duration::from_secs(1_792_518_376);
        assert_eq!(utc_timestamp(at), "2026-10-20T17:46:16Z");
        let leap = UNIX_EPOCH + Duration::from_secs(951_782_400); // 2000-02-29
        assert_eq!(utc_timestamp(leap), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn write_atomic_replaces_and_leaves_no_temp_file() {
        let dir = test_dir("atomic");
        let path = dir.join("summary.txt");
        write_atomic(&path, b"one\n").unwrap();
        write_atomic(&path, "✓ 1\n".as_bytes()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), "✓ 1\n".as_bytes());
        assert!(!dir.join("summary.txt.tmp").exists());
    }

    #[cfg(windows)]
    #[test]
    fn write_atomic_waits_for_a_reader_to_close_the_target() {
        let dir = test_dir("atomic-reader");
        let path = dir.join("summary.txt");
        write_atomic(&path, b"old\n").unwrap();
        // Like cmd's `type`: no FILE_SHARE_DELETE, so a rename over it fails.
        use std::os::windows::fs::OpenOptionsExt;
        let reader = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let closer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(60));
            drop(reader);
        });
        write_atomic(&path, b"new\n").unwrap();
        closer.join().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new\n");
    }

    #[test]
    fn log_appends_lines_and_rotates() {
        let dir = test_dir("log");
        let log = FileLog::new(dir.join("daemon.log"));
        log.log("first");
        log.log("second");
        let text = fs::read_to_string(dir.join("daemon.log")).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().next().unwrap().ends_with("Z first"));

        fs::write(
            dir.join("daemon.log"),
            vec![b'x'; LOG_MAX_BYTES as usize + 1],
        )
        .unwrap();
        log.log("after rotation");
        assert!(dir.join("daemon.log.1").exists());
        let text = fs::read_to_string(dir.join("daemon.log")).unwrap();
        assert_eq!(text.lines().count(), 1);
    }
}
