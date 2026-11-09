//! MarKey's report about itself in the shared `Mar` folder, which is how
//! MarHub knows what permissions it has and whether it is signed in.
//!
//! `Mar/apps/markey.json`: no tokens, no paths to personal files, nothing the
//! user typed. A failure to write only logs; MarKey does not depend on it.

use crate::account::spawn_logged;
use crate::private_files::{atomic_write, ensure_private_dir};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

/// macOS does not announce permission changes, so they are looked for.
const CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// Rewritten at least this often, so MarHub can tell a live report from one
/// left by a program that is no longer running.
const MAX_AGE_SECS: u64 = 5 * 60;

/// What the report says, apart from the time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub accessibility: bool,
    pub input_monitoring: bool,
    pub signed_in: bool,
}

/// `Mar/apps/markey.json` under the user's local data folder: Application
/// Support on macOS, `%LOCALAPPDATA%` on Windows.
pub fn report_path() -> Option<PathBuf> {
    Some(path_under(&dirs::data_local_dir()?))
}

fn path_under(root: &Path) -> PathBuf {
    root.join("Mar").join("apps").join("markey.json")
}

/// RFC 3339 in UTC.
fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

fn render(snapshot: Snapshot, version: &str, now: u64) -> Value {
    let accessibility = if snapshot.accessibility {
        "granted"
    } else {
        "denied"
    };
    // macOS only says "yes" here; "not yet" is indistinguishable from "no".
    let input_monitoring = if snapshot.input_monitoring {
        "granted"
    } else {
        "unknown"
    };
    json!({
        "schema": 1,
        "app": "markey",
        "version": version,
        "updatedAt": rfc3339(now),
        "permissions": {
            "accessibility": accessibility,
            "input-monitoring": input_monitoring,
        },
        "uses": { "models": [] },
        "account": { "signedIn": snapshot.signed_in },
        "deepLink": "markey://",
    })
}

fn write_report(path: &Path, snapshot: Snapshot, version: &str, now: u64) -> std::io::Result<()> {
    if let Some(apps) = path.parent() {
        ensure_private_dir(apps)?;
        if let Some(mar) = apps.parent() {
            ensure_private_dir(mar)?;
        }
    }
    let text = serde_json::to_vec_pretty(&render(snapshot, version, now))?;
    atomic_write(path, &text)
}

/// Writes the report when what it says changes.
struct Writer {
    path: PathBuf,
    version: String,
    last: Option<Snapshot>,
    failing: bool,
}

impl Writer {
    /// True when the file is gone, or old enough to be written again anyway.
    fn is_stale(&self, now: u64) -> bool {
        let written = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.duration_since(UNIX_EPOCH).ok());
        match written {
            Some(at) => now.saturating_sub(at.as_secs()) >= MAX_AGE_SECS,
            None => true,
        }
    }

    fn update(&mut self, snapshot: Snapshot, now: u64) {
        if self.last != Some(snapshot) || self.is_stale(now) {
            self.write(snapshot, now);
        }
    }

    fn write(&mut self, snapshot: Snapshot, now: u64) {
        match write_report(&self.path, snapshot, &self.version, now) {
            Ok(()) => {
                self.last = Some(snapshot);
                self.failing = false;
            }
            Err(e) => {
                // It will be tried again every check; once in the log is enough.
                if !self.failing {
                    log::warn!("cannot write the report for MarHub: {e}");
                }
                self.failing = true;
            }
        }
    }
}

/// Keeps the report current from a thread of its own.
pub struct Reporter {
    tx: Sender<()>,
    writer: Option<Arc<Mutex<Writer>>>,
}

impl Reporter {
    /// `snapshot` is `None` until the app is ready to say anything.
    pub fn spawn(snapshot: impl Fn() -> Option<Snapshot> + Send + 'static) -> Reporter {
        let (tx, rx) = mpsc::channel();
        let Some(path) = report_path() else {
            log::warn!("no folder for the report for MarHub");
            return Reporter { tx, writer: None };
        };
        let writer = Arc::new(Mutex::new(Writer {
            path,
            version: env!("CARGO_PKG_VERSION").to_string(),
            last: None,
            failing: false,
        }));
        let working = writer.clone();
        spawn_logged("markey-report", move || loop {
            if let Some(now) = snapshot() {
                working
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .update(now, crate::account::unix_now());
            }
            match rx.recv_timeout(CHECK_INTERVAL) {
                Ok(()) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
        });
        Reporter {
            tx,
            writer: Some(writer),
        }
    }

    /// Something the report mentions may have changed. Only posts.
    pub fn touch(&self) {
        let _ = self.tx.send(());
    }

    /// One last write as MarKey quits, on the calling thread: the worker is
    /// not going to get another turn.
    pub fn write_final(&self, snapshot: Snapshot) {
        // Only a real change: a fresh `updatedAt` from a program that is quitting
        // would look like a live one for minutes to MarHub.
        if let Some(writer) = &self.writer {
            let mut writer = writer.lock().unwrap_or_else(|e| e.into_inner());
            if writer.last != Some(snapshot) {
                writer.write(snapshot, crate::account::unix_now());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::temp_dir;

    const SIGNED_IN: Snapshot = Snapshot {
        accessibility: true,
        input_monitoring: true,
        signed_in: true,
    };

    #[test]
    fn dates_are_rfc_3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_794_088_800), "2026-11-07T22:00:00Z");
        assert_eq!(rfc3339(1_794_088_800 + 3661), "2026-11-07T23:01:01Z");
    }

    #[test]
    fn the_report_is_exactly_the_agreed_shape() {
        let value = render(SIGNED_IN, "0.2.0", 1_794_088_800);
        assert_eq!(
            value,
            json!({
                "schema": 1,
                "app": "markey",
                "version": "0.2.0",
                "updatedAt": "2026-11-07T22:00:00Z",
                "permissions": { "accessibility": "granted", "input-monitoring": "granted" },
                "uses": { "models": [] },
                "account": { "signedIn": true },
                "deepLink": "markey://",
            })
        );
    }

    #[test]
    fn what_is_not_granted_says_so() {
        let value = render(
            Snapshot {
                accessibility: false,
                input_monitoring: false,
                signed_in: false,
            },
            "0.2.0",
            0,
        );
        assert_eq!(value["permissions"]["accessibility"], "denied");
        assert_eq!(value["permissions"]["input-monitoring"], "unknown");
        assert_eq!(value["account"]["signedIn"], false);
    }

    #[test]
    fn it_lands_in_the_shared_folder_whole_and_private() {
        let root = temp_dir("report");
        let path = path_under(&root);
        write_report(&path, SIGNED_IN, "0.2.0", 5).unwrap();
        write_report(&path, SIGNED_IN, "0.2.1", 6).unwrap();
        assert!(path.ends_with("Mar/apps/markey.json"));
        let written: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["version"], "0.2.1");
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "no temp file left: {names:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&root.join("Mar")), 0o700);
            assert_eq!(mode(&root.join("Mar/apps")), 0o700);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    fn writer(root: &Path) -> Writer {
        Writer {
            path: path_under(root),
            version: "0.2.0".into(),
            last: None,
            failing: false,
        }
    }

    fn updated_at(writer: &Writer) -> String {
        let value: Value = serde_json::from_slice(&std::fs::read(&writer.path).unwrap()).unwrap();
        value["updatedAt"].as_str().unwrap().to_string()
    }

    #[test]
    fn it_is_written_only_when_something_besides_the_time_changes() {
        let root = temp_dir("report-changes");
        let mut w = writer(&root);
        w.update(SIGNED_IN, 100);
        let first = updated_at(&w);
        w.update(SIGNED_IN, 200);
        assert_eq!(updated_at(&w), first, "same content, not rewritten");
        w.update(
            Snapshot {
                signed_in: false,
                ..SIGNED_IN
            },
            300,
        );
        assert_ne!(updated_at(&w), first);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_or_old_report_is_written_again_with_nothing_changed() {
        let root = temp_dir("report-stale");
        let mut w = writer(&root);
        let now = crate::account::unix_now();
        w.update(SIGNED_IN, now);
        let first = updated_at(&w);

        std::fs::remove_file(&w.path).unwrap();
        w.update(SIGNED_IN, now + 1);
        assert_ne!(updated_at(&w), first, "written again when missing");

        let second = updated_at(&w);
        w.update(SIGNED_IN, now + MAX_AGE_SECS - 10);
        assert_eq!(updated_at(&w), second, "not yet");
        w.update(SIGNED_IN, now + MAX_AGE_SECS + 10);
        assert_ne!(updated_at(&w), second, "written again when old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_last_write_ignores_that_nothing_changed() {
        let root = temp_dir("report-final");
        let mut w = writer(&root);
        w.update(SIGNED_IN, 100);
        let first = updated_at(&w);
        w.write(SIGNED_IN, 200);
        assert_ne!(updated_at(&w), first);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_folder_that_cannot_be_written_only_logs() {
        let root = temp_dir("report-blocked");
        std::fs::create_dir_all(&root).unwrap();
        // A file where the folder should go.
        std::fs::write(root.join("Mar"), "in the way").unwrap();
        let mut w = writer(&root);
        w.update(SIGNED_IN, 100);
        assert!(w.failing);
        assert_eq!(w.last, None, "tried again next time");
        let _ = std::fs::remove_dir_all(&root);
    }
}
