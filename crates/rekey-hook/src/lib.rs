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
