//! Watching the keyboard and putting corrected text back, per platform.
//!
//! Rekey observes keystrokes but never swallows them: the character the user
//! typed always lands. A correction is applied afterwards by sending backspaces
//! followed by the replacement, which is what makes the app work in every text
//! field on the system without knowing anything about the app it is typing in.
//!
//! Everything here is a thin, honest wrapper over OS APIs. Anything resembling
//! a decision belongs in `rekey-core`.

use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub use rekey_core::input::{Context, Key, KeyEvent, Modifiers, TextWriter as Injector};

pub mod noop;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos as platform;

#[cfg(target_os = "windows")]
pub mod windows;
#[cfg(target_os = "windows")]
pub use windows as platform;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub use noop as platform;

/// What the hook should do with the keystroke it just reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Let it through to the application.
    Pass,
    /// Swallow it. Only possible when the hook was able to install itself as
    /// an active filter; see [`Capabilities::can_hold`].
    Drop,
    /// The release of a modifier that was just used as MarKey's shortcut:
    /// let it through, but so that the system does not act on the tap itself.
    /// On Windows a lone tap of Alt focuses the menu bar and a tap of the
    /// Windows key opens Start.
    Mask,
}

/// A keystroke as the OS described it, kept so it can be sent again later.
///
/// When a correction is being typed, keys the user presses in the meantime are
/// held back and replayed afterwards, so they land after the correction
/// instead of in the middle of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawKey {
    /// Virtual key code.
    pub code: u16,
    /// Platform modifier flags, verbatim.
    pub flags: u64,
    /// The text the key produced.
    pub text: String,
}

/// What the installed hook turned out to be able to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Capabilities {
    /// Keystrokes can be swallowed and replayed.
    pub can_hold: bool,
}

/// A callback slower than this is counted as slow. macOS disables a tap whose
/// callback takes about a second; anything near 5 ms already shows up as lag.
pub const SLOW_CALLBACK: Duration = Duration::from_millis(5);

/// How many recent "disabled by the system" moments are remembered.
const DISABLE_RING: usize = 8;

/// Why the system switched the keyboard hook off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableKind {
    /// The callback was too slow (macOS `TapDisabledByTimeout`).
    Timeout,
    /// The user's own input tripped the watchdog (`TapDisabledByUserInput`).
    UserInput,
}

/// Counters the hook callback updates without locks, allocation or I/O.
///
/// The callback runs on the keyboard's critical path: anything that can block
/// there — a file write, a contended mutex — is what gets the tap disabled.
/// So the callback only bumps atomics, and a separate thread reads them and
/// writes the log.
#[derive(Debug)]
pub struct TapHealth {
    started: Instant,
    events: AtomicU64,
    slow: AtomicU64,
    slowest_micros: AtomicU64,
    last_slow_micros: AtomicU64,
    skipped_busy: AtomicU64,
    disabled_timeout: AtomicU64,
    disabled_user: AtomicU64,
    /// Milliseconds since `started` of the last few disables, plus one, so
    /// zero means an empty slot.
    disables: [AtomicU64; DISABLE_RING],
    next_disable: AtomicUsize,
}

impl Default for TapHealth {
    fn default() -> Self {
        TapHealth {
            started: Instant::now(),
            events: AtomicU64::new(0),
            slow: AtomicU64::new(0),
            slowest_micros: AtomicU64::new(0),
            last_slow_micros: AtomicU64::new(0),
            skipped_busy: AtomicU64::new(0),
            disabled_timeout: AtomicU64::new(0),
            disabled_user: AtomicU64::new(0),
            disables: Default::default(),
            next_disable: AtomicUsize::new(0),
        }
    }
}

/// A copy of the counters at one moment, for logging and the settings window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TapHealthSnapshot {
    pub events: u64,
    pub slow: u64,
    pub slowest_micros: u64,
    pub last_slow_micros: u64,
    /// Events passed through untouched because the engine was busy.
    pub skipped_busy: u64,
    pub disabled_timeout: u64,
    pub disabled_user: u64,
}

impl TapHealth {
    pub fn new() -> TapHealth {
        TapHealth::default()
    }

    /// One callback finished after `took`.
    pub fn record_event(&self, took: Duration) {
        self.events.fetch_add(1, Ordering::Relaxed);
        if took >= SLOW_CALLBACK {
            let micros = took.as_micros().min(u64::MAX as u128) as u64;
            self.slow.fetch_add(1, Ordering::Relaxed);
            self.last_slow_micros.store(micros, Ordering::Relaxed);
            self.slowest_micros.fetch_max(micros, Ordering::Relaxed);
        }
    }

    /// An event went through without the engine seeing it, because the
    /// engine was busy and waiting for it would have stalled the keyboard.
    pub fn record_skipped(&self) {
        self.skipped_busy.fetch_add(1, Ordering::Relaxed);
    }

    /// The system switched the hook off.
    pub fn record_disabled(&self, kind: DisableKind) {
        match kind {
            DisableKind::Timeout => self.disabled_timeout.fetch_add(1, Ordering::Relaxed),
            DisableKind::UserInput => self.disabled_user.fetch_add(1, Ordering::Relaxed),
        };
        let at = self.started.elapsed().as_millis() as u64 + 1;
        let slot = self.next_disable.fetch_add(1, Ordering::Relaxed) % DISABLE_RING;
        self.disables[slot].store(at, Ordering::Relaxed);
    }

    /// How many disables happened in the last `window` (at most 8 are kept).
    pub fn disables_within(&self, window: Duration) -> usize {
        let now = self.started.elapsed().as_millis() as u64 + 1;
        let window = window.as_millis() as u64;
        self.disables
            .iter()
            .map(|slot| slot.load(Ordering::Relaxed))
            .filter(|&at| at != 0 && now.saturating_sub(at) <= window)
            .count()
    }

    pub fn snapshot(&self) -> TapHealthSnapshot {
        TapHealthSnapshot {
            events: self.events.load(Ordering::Relaxed),
            slow: self.slow.load(Ordering::Relaxed),
            slowest_micros: self.slowest_micros.load(Ordering::Relaxed),
            last_slow_micros: self.last_slow_micros.load(Ordering::Relaxed),
            skipped_busy: self.skipped_busy.load(Ordering::Relaxed),
            disabled_timeout: self.disabled_timeout.load(Ordering::Relaxed),
            disabled_user: self.disabled_user.load(Ordering::Relaxed),
        }
    }
}

/// Lets through one drag report per mouse press, and after that at most one
/// per [`DragGate::INTERVAL`].
///
/// A drag arrives 60–120 times a second, and to the engine every one of them
/// means the same thing: "a selection is being made". Sending each through
/// the whole handler was most of the tap's load while selecting text.
#[derive(Debug, Default)]
pub struct DragGate {
    /// Milliseconds (plus one) on the caller's clock of the last drag let
    /// through; zero after a press.
    last: AtomicU64,
}

impl DragGate {
    pub const INTERVAL: Duration = Duration::from_millis(50);

    /// A mouse button went down: the next drag starts a new selection.
    pub fn reset(&self) {
        self.last.store(0, Ordering::Relaxed);
    }

    /// Whether a drag at `now_ms` should reach the engine.
    pub fn admit(&self, now_ms: u64) -> bool {
        let now = now_ms + 1;
        let last = self.last.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < Self::INTERVAL.as_millis() as u64 {
            return false;
        }
        self.last.store(now, Ordering::Relaxed);
        true
    }
}

/// The most the clipboard snapshot may hold. Converting a selection borrows
/// the clipboard and puts the user's contents back afterwards; a clipboard
/// holding a large image or file is left untouched instead of being copied
/// into memory on every press of the shortcut.
pub const MAX_PASTEBOARD_SNAPSHOT: usize = 20 * 1024 * 1024;

/// The clipboard holds more than [`MAX_PASTEBOARD_SNAPSHOT`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotTooLarge;

/// Counts bytes against [`MAX_PASTEBOARD_SNAPSHOT`] while a snapshot is read.
#[derive(Debug)]
pub struct SnapshotBudget {
    left: usize,
}

impl Default for SnapshotBudget {
    fn default() -> Self {
        SnapshotBudget::new(MAX_PASTEBOARD_SNAPSHOT)
    }
}

impl SnapshotBudget {
    pub fn new(limit: usize) -> SnapshotBudget {
        SnapshotBudget { left: limit }
    }

    /// Spend `bytes`; an error once the total goes over the limit.
    pub fn spend(&mut self, bytes: usize) -> Result<(), SnapshotTooLarge> {
        self.left = self.left.checked_sub(bytes).ok_or(SnapshotTooLarge)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum HookError {
    /// The OS refused to install the hook. On macOS this is almost always a
    /// missing Accessibility grant.
    PermissionDenied,
    Unsupported(&'static str),
    Os(String),
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookError::PermissionDenied => write!(
                f,
                "Rekey needs Accessibility permission to watch the keyboard"
            ),
            HookError::Unsupported(s) => write!(f, "not supported on this platform: {s}"),
            HookError::Os(s) => write!(f, "operating system error: {s}"),
        }
    }
}

impl std::error::Error for HookError {}

/// Convenience re-export for examples: the key presses that type `text` on
/// `layout`. See [`rekey_core::layout::presses_for`].
pub fn rekey_core_presses(text: &str, layout: &str) -> Option<Vec<rekey_core::layout::KeyPress>> {
    rekey_core::layout::presses_for(text, layout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_callbacks_are_counted_with_their_duration() {
        let health = TapHealth::new();
        health.record_event(Duration::from_micros(300));
        health.record_event(Duration::from_millis(7));
        health.record_event(Duration::from_millis(12));
        health.record_event(Duration::from_millis(6));
        let s = health.snapshot();
        assert_eq!(s.events, 4);
        assert_eq!(s.slow, 3);
        assert_eq!(s.slowest_micros, 12_000);
        assert_eq!(s.last_slow_micros, 6_000);
    }

    #[test]
    fn disables_are_told_apart_and_windowed() {
        let health = TapHealth::new();
        health.record_disabled(DisableKind::Timeout);
        health.record_disabled(DisableKind::UserInput);
        health.record_disabled(DisableKind::Timeout);
        let s = health.snapshot();
        assert_eq!((s.disabled_timeout, s.disabled_user), (2, 1));
        assert_eq!(health.disables_within(Duration::from_secs(60)), 3);
        // The ring keeps the most recent eight.
        for _ in 0..20 {
            health.record_disabled(DisableKind::Timeout);
        }
        assert_eq!(
            health.disables_within(Duration::from_secs(60)),
            DISABLE_RING
        );
    }

    #[test]
    fn the_snapshot_budget_refuses_past_the_limit() {
        let mut budget = SnapshotBudget::new(10);
        assert!(budget.spend(4).is_ok());
        assert!(budget.spend(6).is_ok());
        assert_eq!(budget.spend(1), Err(SnapshotTooLarge));
        let mut real = SnapshotBudget::default();
        assert!(real.spend(MAX_PASTEBOARD_SNAPSHOT).is_ok());
        assert!(real.spend(1).is_err());
    }

    #[test]
    fn drag_reports_are_throttled_and_reset_by_a_press() {
        let gate = DragGate::default();
        assert!(
            gate.admit(1000),
            "the first drag after a press goes through"
        );
        // A burst at 120 Hz for half a second: one report per 50 ms at most.
        let admitted = (1..60).filter(|i| gate.admit(1000 + i * 8)).count();
        assert!(admitted <= 10, "{admitted} drags admitted out of 59");
        gate.reset();
        assert!(gate.admit(1490), "a new press starts a new selection");
    }
}
