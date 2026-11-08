//! The log, kept off the keyboard's path.
//!
//! A menu bar app launched from Finder has nowhere to write stderr, so the log
//! goes to a file beside the settings. It never records what is typed: entries
//! note decisions and lengths, never the words.
//!
//! Writing it used to happen synchronously — a `writeln!` and a `flush()` to
//! disk under a mutex — from whatever thread logged, the keyboard hook
//! included. A slow disk then made the hook's callback slow, and macOS
//! switched the keyboard filter off. Now a log call only formats the line and
//! hands it to a bounded channel; one background thread does all the writing.
//! When the channel is full the line is dropped and counted, never waited for.
//!
//! The file rotates: `markey.log` up to 1 MB, then `markey.log.1` and
//! `markey.log.2`. Starting the app rotates too, so the previous session's
//! log survives a relaunch — that is the one a user reporting a problem needs.

use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

/// Rotate once the current file passes this size.
pub const MAX_FILE: u64 = 1024 * 1024;
/// Files kept in all: `markey.log`, `.1`, `.2`.
pub const KEEP: usize = 3;
/// Lines waiting for the writer. Past this, new lines are dropped.
const BACKLOG: usize = 4096;
/// The filter when neither `MARKEY_LOG` nor `RUST_LOG` is set. Debug output
/// (one line per word) is opt-in.
const DEFAULT_FILTER: &str = "info";

/// A file that moves itself aside at [`MAX_FILE`], keeping [`KEEP`] files.
pub struct RotatingFile {
    path: PathBuf,
    out: Option<BufWriter<File>>,
    written: u64,
}

impl RotatingFile {
    /// Opens `path`, first moving any existing log aside.
    pub fn open(path: PathBuf) -> RotatingFile {
        let mut file = RotatingFile {
            path,
            out: None,
            written: 0,
        };
        file.rotate();
        file
    }

    fn rotated(&self, n: usize) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".{n}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) {
        if let Some(mut out) = self.out.take() {
            let _ = out.flush();
        }
        let _ = std::fs::remove_file(self.rotated(KEEP - 1));
        for n in (1..KEEP - 1).rev() {
            let _ = std::fs::rename(self.rotated(n), self.rotated(n + 1));
        }
        let _ = std::fs::rename(&self.path, self.rotated(1));
        self.out = create_private(&self.path).ok().map(BufWriter::new);
        self.written = 0;
    }

    pub fn write_line(&mut self, line: &str) {
        let len = line.len() as u64 + 1;
        if self.written > 0 && self.written + len > MAX_FILE {
            self.rotate();
        }
        if let Some(out) = self.out.as_mut() {
            if writeln!(out, "{line}").is_ok() {
                self.written += len;
            }
        }
    }

    pub fn flush(&mut self) {
        if let Some(out) = self.out.as_mut() {
            let _ = out.flush();
        }
    }
}

/// Creates (truncating) a file only the user can read.
fn create_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Below info these print what must never reach a file: ureq and rustls the
/// bytes on the wire, tokens included, and the other two whole `markey://`
/// links with their codes.
const NEVER_VERBOSE: [&str; 5] = [
    "ureq",
    "ureq_proto",
    "rustls",
    "tao",
    "tauri_plugin_deep_link",
];

/// Checked in the logger itself, so no `MARKEY_LOG` can turn these back on.
fn must_stay_quiet(target: &str, level: log::Level) -> bool {
    level > log::Level::Info
        && NEVER_VERBOSE.iter().any(|name| {
            target
                .strip_prefix(name)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
        })
}

struct ChannelLogger {
    filter: env_filter::Filter,
    lines: SyncSender<String>,
    dropped: Arc<AtomicU64>,
}

impl log::Log for ChannelLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        self.filter.enabled(metadata) && !must_stay_quiet(metadata.target(), metadata.level())
    }

    fn log(&self, record: &log::Record<'_>) {
        if !self.filter.matches(record) || must_stay_quiet(record.target(), record.level()) {
            return;
        }
        let line = format!(
            "[{} {} {}] {}",
            timestamp(),
            record.level(),
            record.target(),
            record.args()
        );
        match self.lines.try_send(line) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    fn flush(&self) {}
}

/// Writes queued lines; flushes whenever the queue runs dry, so a line is on
/// disk within moments without a flush per line.
fn writer(lines: Receiver<String>, mut file: RotatingFile, dropped: Arc<AtomicU64>) {
    let echo = cfg!(debug_assertions);
    let mut reported = 0;
    loop {
        let line = match lines.recv_timeout(Duration::from_millis(500)) {
            Ok(line) => line,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                file.flush();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let lost = dropped.load(Ordering::Relaxed);
        if lost > reported {
            file.write_line(&format!(
                "[{} WARN markey] {} log line(s) dropped: the log could not keep up",
                timestamp(),
                lost - reported
            ));
            reported = lost;
        }
        file.write_line(&line);
        if echo {
            eprintln!("{line}");
        }
        while let Ok(line) = lines.try_recv() {
            file.write_line(&line);
            if echo {
                eprintln!("{line}");
            }
        }
        file.flush();
    }
    file.flush();
}

/// The filter: `MARKEY_LOG`, else `RUST_LOG`, else [`DEFAULT_FILTER`].
fn filter_spec() -> String {
    std::env::var("MARKEY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .unwrap_or_else(|_| DEFAULT_FILTER.to_string())
}

/// Install the logger. Lines go to `path`, rotated, from a background thread.
pub fn init(path: PathBuf) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let filter = env_filter::Builder::new().parse(&filter_spec()).build();
    let level = filter.filter();
    let (tx, rx) = mpsc::sync_channel(BACKLOG);
    let dropped = Arc::new(AtomicU64::new(0));
    let file = RotatingFile::open(path.clone());
    let writer_dropped = dropped.clone();
    let spawned = std::thread::Builder::new()
        .name("markey-log".into())
        .spawn(move || writer(rx, file, writer_dropped));
    if spawned.is_err() {
        return;
    }
    let logger = ChannelLogger {
        filter,
        lines: tx,
        dropped,
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(level);
    }
    log::info!("logging to {} (level {level})", path.display());
}

/// Seconds since the Unix epoch, with milliseconds.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("markey-log-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rotates_at_one_megabyte_and_keeps_three_files() {
        let dir = temp_dir("rotate");
        let path = dir.join("markey.log");
        let mut file = RotatingFile::open(path.clone());
        let line = "x".repeat(1000);
        for _ in 0..3500 {
            file.write_line(&line);
        }
        file.flush();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        names.sort();
        assert_eq!(names, ["markey.log", "markey.log.1", "markey.log.2"]);
        for name in &names {
            let size = std::fs::metadata(dir.join(name)).unwrap().len();
            assert!(size <= MAX_FILE, "{name} is {size} bytes");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_relaunch_keeps_the_previous_log() {
        let dir = temp_dir("relaunch");
        let path = dir.join("markey.log");
        let mut first = RotatingFile::open(path.clone());
        first.write_line("first session");
        first.flush();
        drop(first);
        let mut second = RotatingFile::open(path.clone());
        second.write_line("second session");
        second.flush();
        let previous = std::fs::read_to_string(dir.join("markey.log.1")).unwrap();
        assert_eq!(previous.trim(), "first session");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "second session"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_filter_can_bring_back_wire_or_link_dumps() {
        let (tx, rx) = mpsc::sync_channel(64);
        let logger = ChannelLogger {
            filter: env_filter::Builder::new().parse("trace").build(),
            lines: tx,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        let send = |target: &str, level: log::Level| {
            log::Log::log(
                &logger,
                &log::Record::builder()
                    .level(level)
                    .target(target)
                    .args(format_args!("line"))
                    .build(),
            );
        };
        for target in [
            "ureq",
            "ureq::run",
            "ureq_proto::util",
            "rustls::client::hs",
            "tao::platform_impl",
            "tauri_plugin_deep_link",
        ] {
            send(target, log::Level::Trace);
            send(target, log::Level::Debug);
            assert!(rx.try_recv().is_err(), "{target}");
            // Warnings still arrive: they say that something went wrong.
            send(target, log::Level::Warn);
            assert!(rx.try_recv().is_ok(), "{target}");
        }
        // Only those crates, and not names that merely start the same.
        send("markey", log::Level::Trace);
        assert!(rx.try_recv().is_ok());
        send("ureqish::x", log::Level::Trace);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn a_full_backlog_drops_lines_instead_of_waiting() {
        let (tx, _rx) = mpsc::sync_channel(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let logger = ChannelLogger {
            filter: env_filter::Builder::new().parse("info").build(),
            lines: tx,
            dropped: dropped.clone(),
        };
        let started = std::time::Instant::now();
        for _ in 0..10 {
            log::Log::log(
                &logger,
                &log::Record::builder()
                    .level(log::Level::Info)
                    .target("markey")
                    .args(format_args!("line"))
                    .build(),
            );
        }
        assert!(started.elapsed() < Duration::from_millis(50));
        assert_eq!(dropped.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn debug_is_off_unless_asked_for() {
        let filter = env_filter::Builder::new().parse(DEFAULT_FILTER).build();
        assert_eq!(filter.filter(), log::LevelFilter::Info);
    }
}
