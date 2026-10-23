//! Stand-in platform used on unsupported targets and in tests.
//!
//! It records what it was asked to do rather than touching a real keyboard,
//! which is what lets the end-to-end engine tests run in CI on Linux.

use crate::{Capabilities, Context, Disposition, HookError, Injector, KeyEvent, RawKey};
use std::sync::{Arc, Mutex};

#[derive(Debug, Default, Clone)]
pub struct RecordingInjector {
    pub actions: Arc<Mutex<Vec<Action>>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Backspace(usize),
    Type(String),
}

impl RecordingInjector {
    pub fn new() -> RecordingInjector {
        RecordingInjector::default()
    }

    pub fn taken(&self) -> Vec<Action> {
        std::mem::take(&mut self.actions.lock().unwrap())
    }
}

impl Injector for RecordingInjector {
    fn backspace(&self, count: usize) {
        self.actions.lock().unwrap().push(Action::Backspace(count));
    }

    fn type_text(&self, text: &str) {
        self.actions
            .lock()
            .unwrap()
            .push(Action::Type(text.to_string()));
    }
}

pub fn current_context() -> Context {
    Context::default()
}

pub fn has_permission() -> bool {
    false
}

/// No keyboard to switch on an unsupported platform.
pub fn select_layout(_layout_id: &str) -> bool {
    false
}

pub fn request_permission() -> bool {
    false
}

pub fn current_layout() -> Option<String> {
    None
}

pub fn frontmost_app() -> Option<String> {
    None
}

pub fn secure_input_active() -> bool {
    false
}

pub fn run<F, R>(_on_ready: R, _on_key: F) -> Result<(), HookError>
where
    F: FnMut(KeyEvent, Option<RawKey>) -> Disposition + Send + 'static,
    R: FnOnce(Capabilities),
{
    Err(HookError::Unsupported(
        "keyboard hooks are available on macOS and Windows",
    ))
}

impl RecordingInjector {
    pub fn copy(&self) {}
    pub fn paste(&self) {}
    pub fn select_word_left(&self) {}
    pub fn collapse_right(&self) {}
    pub fn replay(&self, raw: &crate::RawKey) {
        self.type_text(&raw.text);
    }
}

/// A layout id with what each of its keys types, unshifted and shifted.
pub type KeyTable = (String, Vec<Option<String>>, Vec<Option<String>>);

pub fn system_layouts() -> Vec<KeyTable> {
    Vec::new()
}

pub fn focused_selection_length() -> Option<usize> {
    None
}

pub fn play_sound() {}

pub struct PasteboardSnapshot;

pub fn pasteboard_change_count() -> isize {
    0
}

pub fn pasteboard_snapshot() -> PasteboardSnapshot {
    PasteboardSnapshot
}

pub fn pasteboard_restore(_snapshot: &PasteboardSnapshot) {}

pub fn pasteboard_string() -> Option<String> {
    None
}

pub fn pasteboard_set_string(_text: &str) {}

pub fn caps_lock_off() {}
