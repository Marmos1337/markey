//! The keyboard event tap: what the user types, as it happens.

use super::REKEY_SIGNATURE;
use crate::{Capabilities, Disposition, HookError, Key, KeyEvent, Modifiers, RawKey};
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use core_graphics::sys::CGEventRef;
use foreign_types::ForeignType;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Re-arms an event tap that the system has switched off.
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);

    /// Reads the characters a key event resolves to on the active layout.
    /// `core-graphics` exposes the setter but not the getter.
    fn CGEventKeyboardGetUnicodeString(
        event: CGEventRef,
        max_length: usize,
        actual_length: *mut usize,
        buffer: *mut u16,
    );
}

// -- the tap ----------------------------------------------------------------

/// Read the character a key event produced, as macOS resolved it for the
/// active layout.
fn event_text(event: &CGEvent) -> String {
    const MAX: usize = 8;
    let mut buffer = [0u16; MAX];
    let mut actual: usize = 0;
    unsafe {
        CGEventKeyboardGetUnicodeString(
            event.as_ptr(),
            MAX,
            &mut actual as *mut usize,
            buffer.as_mut_ptr(),
        );
    }
    String::from_utf16_lossy(&buffer[..actual.min(MAX)])
}

/// Device-dependent flag bits that say which side a modifier is on.
const NX_DEVICERSHIFTKEYMASK: u64 = 0x0000_0004;
const NX_DEVICERCMDKEYMASK: u64 = 0x0000_0010;
const NX_DEVICERALTKEYMASK: u64 = 0x0000_0040;
const NX_DEVICERCTLKEYMASK: u64 = 0x0000_2000;

fn modifiers_of(event: &CGEvent) -> Modifiers {
    let flags = event.get_flags();
    let bits = flags.bits();
    Modifiers {
        shift: flags.contains(CGEventFlags::CGEventFlagShift),
        control: flags.contains(CGEventFlags::CGEventFlagControl),
        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
        meta: flags.contains(CGEventFlags::CGEventFlagCommand),
        right_shift: bits & NX_DEVICERSHIFTKEYMASK != 0,
        right_control: bits & NX_DEVICERCTLKEYMASK != 0,
        right_alt: bits & NX_DEVICERALTKEYMASK != 0,
        right_meta: bits & NX_DEVICERCMDKEYMASK != 0,
    }
}

/// macOS virtual keycodes the engine needs to recognise by identity rather
/// than by the character they produce.
fn classify(keycode: u16, character: Option<char>, modifiers: Modifiers) -> Key {
    match keycode {
        51 => Key::Backspace,
        49 => Key::Space,
        36 | 76 => Key::Enter,
        48 => Key::Tab,
        53 => Key::Escape,
        // Arrows, and the Home/End/PageUp/PageDown cluster.
        123..=126 | 115 | 116 | 119 | 121 => Key::Navigation,
        // Modifier keys: command, shift, caps lock, option, control, fn.
        // Named explicitly: when one arrives as a key event its reported
        // character is a space, which must not end a word.
        54..=63 => Key::ModifiersChanged,
        // ⌘A selects everything.
        0 if modifiers.meta && !modifiers.control => Key::Selection,
        _ => match character {
            // Whitespace is identified by key code above, because that is
            // what a keyboard sends.
            Some(c) if c.is_whitespace() => Key::Other,
            Some(c) if !c.is_control() => Key::Character,
            _ => Key::Other,
        },
    }
}

/// Install the tap and run the event loop on the calling thread.
///
/// `on_ready` is told what the installed tap can do before the first event.
/// This never returns while the hook is running, so callers put it on a
/// dedicated thread.
pub fn run<F, R>(on_ready: R, on_key: F) -> Result<(), HookError>
where
    F: FnMut(KeyEvent, Option<RawKey>) -> Disposition + Send + 'static,
    R: FnOnce(Capabilities),
{
    if !super::has_permission() {
        return Err(HookError::PermissionDenied);
    }

    // The tap callback is `Fn`, but the handler needs to mutate state. The tap
    // only ever fires on this one thread, so the lock is uncontended.
    let handler = Arc::new(Mutex::new(on_key));

    let events = vec![
        CGEventType::KeyDown,
        CGEventType::FlagsChanged,
        CGEventType::LeftMouseDown,
        CGEventType::RightMouseDown,
        CGEventType::OtherMouseDown,
        CGEventType::LeftMouseDragged,
    ];

    let make_callback = |active: bool, port: Arc<AtomicUsize>| {
        let handler = handler.clone();
        move |_proxy: core_graphics::event::CGEventTapProxy,
              event_type: CGEventType,
              event: &CGEvent|
              -> CallbackResult {
            // macOS switches a tap off if its callback is ever too slow, or if
            // the user's own input trips the watchdog, and it stays off until
            // explicitly re-armed. Without this the app goes quietly deaf.
            if matches!(
                event_type,
                CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
            ) {
                let port = port.load(Ordering::Relaxed);
                if port != 0 {
                    // Whatever was tracked may have changed while deaf.
                    if let Ok(mut handler) = handler.lock() {
                        let _ = handler(
                            KeyEvent {
                                character: None,
                                key: Key::Click,
                                modifiers: Modifiers::default(),
                                synthetic: false,
                            },
                            None,
                        );
                    }
                    log::warn!("keyboard tap was disabled by the system; re-arming");
                    unsafe { CGEventTapEnable(port as *mut c_void, true) };
                }
                return CallbackResult::Keep;
            }

            let synthetic = event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
                == REKEY_SIGNATURE;
            let modifiers = modifiers_of(event);

            let (key_event, raw) = match event_type {
                CGEventType::KeyDown => {
                    let keycode =
                        event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
                    let text = event_text(event);
                    let character = text.chars().next();
                    (
                        KeyEvent {
                            character,
                            key: classify(keycode, character, modifiers),
                            modifiers,
                            synthetic,
                        },
                        Some(RawKey {
                            code: keycode,
                            flags: event.get_flags().bits(),
                            text,
                        }),
                    )
                }
                // A modifier on its own arrives as a flags change. Tapping one
                // is how the manual shortcut is triggered.
                CGEventType::FlagsChanged => (
                    KeyEvent {
                        character: None,
                        key: Key::ModifiersChanged,
                        modifiers,
                        synthetic,
                    },
                    None,
                ),
                CGEventType::LeftMouseDown => {
                    // A double or triple click selects a word or a line.
                    let clicks = event.get_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE);
                    (
                        KeyEvent {
                            character: None,
                            key: if clicks >= 2 {
                                Key::Selection
                            } else {
                                Key::Click
                            },
                            modifiers,
                            synthetic,
                        },
                        None,
                    )
                }
                CGEventType::LeftMouseDragged => (
                    KeyEvent {
                        character: None,
                        key: Key::Selection,
                        modifiers,
                        synthetic,
                    },
                    None,
                ),
                CGEventType::RightMouseDown | CGEventType::OtherMouseDown => (
                    KeyEvent {
                        character: None,
                        key: Key::Click,
                        modifiers,
                        synthetic,
                    },
                    None,
                ),
                _ => return CallbackResult::Keep,
            };

            let disposition = match handler.lock() {
                Ok(mut handler) => handler(key_event, raw),
                Err(_) => Disposition::Pass,
            };
            if active && disposition == Disposition::Drop {
                CallbackResult::Drop
            } else {
                CallbackResult::Keep
            }
        }
    };

    // The active filter first; a listen-only tap if the system refuses it.
    let active_port = Arc::new(AtomicUsize::new(0));
    let (tap, active, port) = match CGEventTap::new(
        CGEventTapLocation::Session,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        events.clone(),
        make_callback(true, active_port.clone()),
    ) {
        Ok(tap) => (tap, true, active_port),
        Err(_) => {
            log::warn!("an active keyboard filter was refused; observing only");
            let port = Arc::new(AtomicUsize::new(0));
            let tap = CGEventTap::new(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::ListenOnly,
                events,
                make_callback(false, port.clone()),
            )
            .map_err(|_| HookError::PermissionDenied)?;
            (tap, false, port)
        }
    };

    port.store(
        tap.mach_port().as_concrete_TypeRef() as usize,
        Ordering::Relaxed,
    );

    let loop_source = tap
        .mach_port()
        .create_runloop_source(0)
        .map_err(|_| HookError::Os("cannot create a run loop source".into()))?;

    let run_loop = CFRunLoop::get_current();
    unsafe {
        run_loop.add_source(&loop_source, kCFRunLoopCommonModes);
    }
    tap.enable();
    log::info!(
        "keyboard tap installed ({})",
        if active { "active" } else { "listen-only" }
    );
    on_ready(Capabilities { can_hold: active });
    CFRunLoop::run_current();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_editing_keys() {
        let none = Modifiers::default();
        assert_eq!(classify(51, None, none), Key::Backspace);
        assert_eq!(classify(49, Some(' '), none), Key::Space);
        assert_eq!(classify(123, None, none), Key::Navigation);
        assert_eq!(classify(0, Some('a'), none), Key::Character);
        let cmd = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert_eq!(classify(0, Some('a'), cmd), Key::Selection);
    }

    #[test]
    fn modifier_keys_are_never_mistaken_for_whitespace() {
        for keycode in 54..=63 {
            assert_eq!(
                classify(keycode, Some(' '), Modifiers::default()),
                Key::ModifiersChanged
            );
        }
    }

    #[test]
    fn whitespace_is_recognised_by_key_not_by_character() {
        let none = Modifiers::default();
        assert_eq!(classify(49, Some(' '), none), Key::Space);
        assert_eq!(classify(48, Some('\t'), none), Key::Tab);
        assert_eq!(classify(36, Some('\r'), none), Key::Enter);
        assert_eq!(classify(0, Some(' '), none), Key::Other);
    }
}
