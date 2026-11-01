//! Typing corrections with synthetic `CGEvent`s.

use super::layouts::KEYCODES;
use super::REKEY_SIGNATURE;
use crate::{Injector, Key, RawKey};
use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation, EventField};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use std::time::Duration;

// -- injection --------------------------------------------------------------

pub struct MacInjector {
    source: CGEventSource,
    /// Pause between keystrokes, in microseconds; see
    /// [`rekey_core::apps::keystroke_gap`].
    gap_micros: std::sync::atomic::AtomicU64,
}

// SAFETY: CGEventSource is a reference-counted Core Foundation object whose
// functions are thread-safe to call (Quartz Event Services documents event
// creation and posting as usable from any thread). The injector is created
// once and then used only from the markey-inject thread; the other field is
// an atomic. Moving it there (Send) and sharing &self (Sync) therefore never
// mutates the source concurrently.
unsafe impl Send for MacInjector {}
unsafe impl Sync for MacInjector {}

/// Pause between injected keystrokes.
///
/// Synthetic events posted back to back are not always all delivered: the
/// receiving application can drop or coalesce them, which shows up as a
/// replacement that deleted one character too few. A real keyboard never
/// produces keystrokes this close together, and a few milliseconds each is
/// imperceptible.
const KEYSTROKE_GAP: Duration = rekey_core::apps::DEFAULT_KEYSTROKE_GAP;

/// Virtual keycodes for the keys the layout tables do not cover.
const VK_DELETE: u16 = 51;
pub(crate) const VK_SPACE: u16 = 49;
pub(crate) const VK_TAB: u16 = 48;
pub(crate) const VK_RETURN: u16 = 36;
const VK_LEFT: u16 = 123;
const VK_RIGHT: u16 = 124;
const VK_C: u16 = 8;
const VK_V: u16 = 9;

impl MacInjector {
    pub fn new() -> Result<MacInjector, crate::HookError> {
        // `Private` deliberately, not `HIDSystemState`. A HID-state source
        // reflects the modifier keys physically held right now, so a
        // correction fired while the user still has a modifier down would
        // inherit it.
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .map_err(|_| crate::HookError::Os("cannot create a CGEventSource".into()))?;
        Ok(MacInjector {
            source,
            gap_micros: std::sync::atomic::AtomicU64::new(KEYSTROKE_GAP.as_micros() as u64),
        })
    }

    fn post(&self, event: &CGEvent, flags: CGEventFlags) {
        event.set_flags(flags);
        // The signature must be stamped *after* the flags: setting flags
        // clears the event's user-data field.
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, REKEY_SIGNATURE);
        event.post(CGEventTapLocation::HID);
    }

    fn pause(&self) {
        let micros = self.gap_micros.load(std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(Duration::from_micros(micros));
    }

    /// Press and release one key with exactly these modifiers.
    fn press(&self, keycode: u16, flags: CGEventFlags) {
        for down in [true, false] {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), keycode, down) {
                self.post(&event, flags);
            }
        }
        self.pause();
    }

    /// ⌘C.
    pub fn copy(&self) {
        self.press(VK_C, CGEventFlags::CGEventFlagCommand);
    }

    /// ⌘V.
    pub fn paste(&self) {
        self.press(VK_V, CGEventFlags::CGEventFlagCommand);
    }

    /// ⌥⇧←: select the word before the caret.
    pub fn select_word_left(&self) {
        self.press(
            VK_LEFT,
            CGEventFlags::CGEventFlagAlternate | CGEventFlags::CGEventFlagShift,
        );
    }

    /// →: collapse a selection to its end.
    pub fn collapse_right(&self) {
        self.press(VK_RIGHT, CGEventFlags::empty());
    }

    /// Send again a key the user pressed while a correction was being typed.
    pub fn replay(&self, raw: &RawKey) {
        let flags = CGEventFlags::from_bits_retain(raw.flags);
        for down in [true, false] {
            let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), raw.code, down) else {
                return;
            };
            if down && !raw.text.is_empty() {
                event.set_string(&raw.text);
            }
            self.post(&event, flags);
        }
        self.pause();
    }
}

impl Injector for MacInjector {
    fn set_keystroke_gap(&self, gap: Duration) {
        self.gap_micros
            .store(gap.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
    }

    fn backspace(&self, count: usize) {
        for _ in 0..count {
            self.press(VK_DELETE, CGEventFlags::empty());
        }
    }

    fn type_keys(&self, presses: &[rekey_core::layout::KeyPress]) -> bool {
        use rekey_core::layout::KeyPress;
        for press in presses {
            let (keycode, shift) = match *press {
                KeyPress::Key { index, shift } => match KEYCODES.get(index) {
                    Some(&code) => (code, shift),
                    None => return false,
                },
                KeyPress::Space => (VK_SPACE, false),
                KeyPress::Tab => (VK_TAB, false),
                KeyPress::Enter => (VK_RETURN, false),
            };
            self.press(
                keycode,
                if shift {
                    CGEventFlags::CGEventFlagShift
                } else {
                    CGEventFlags::empty()
                },
            );
        }
        true
    }

    fn type_text(&self, text: &str) {
        // One event per character, because a CGEvent *is* one keystroke.
        // Attaching a whole string to one event relies on the receiving app
        // inserting it verbatim, and many take only the first character.
        let mut encoded = String::with_capacity(4);
        for ch in text.chars() {
            match ch {
                ' ' => {
                    self.press(VK_SPACE, CGEventFlags::empty());
                    continue;
                }
                '\t' => {
                    self.press(VK_TAB, CGEventFlags::empty());
                    continue;
                }
                '\n' => {
                    self.press(VK_RETURN, CGEventFlags::empty());
                    continue;
                }
                _ => {}
            }
            encoded.clear();
            encoded.push(ch);
            for down in [true, false] {
                let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), 0, down) else {
                    return;
                };
                event.set_string(&encoded);
                self.post(&event, CGEventFlags::empty());
            }
            self.pause();
        }
    }

    fn boundary(&self, key: Key, shift: bool) -> bool {
        let code = match key {
            Key::Space => VK_SPACE,
            Key::Tab => VK_TAB,
            Key::Enter => VK_RETURN,
            _ => return false,
        };
        self.press(
            code,
            if shift {
                CGEventFlags::CGEventFlagShift
            } else {
                CGEventFlags::empty()
            },
        );
        true
    }
}
