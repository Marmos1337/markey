//! Keyboard layouts: which input source is active, switching it, and reading
//! what each key types on the user's installed layouts.

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{CFRelease, TCFType};
use core_foundation::data::CFData;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: CFStringRef) -> *mut c_void;
    fn TISCreateInputSourceList(properties: *const c_void, include_all: bool) -> CFArrayRef;
    fn TISSelectInputSource(source: *mut c_void) -> i32;
    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    fn LMGetKbdType() -> u8;
    fn UCKeyTranslate(
        layout: *const c_void,
        virtual_key_code: u16,
        key_action: u16,
        modifier_key_state: u32,
        keyboard_type: u32,
        key_translate_options: u32,
        dead_key_state: *mut u32,
        max_string_length: usize,
        actual_string_length: *mut usize,
        unicode_string: *mut u16,
    ) -> i32;
}

// -- context ----------------------------------------------------------------

/// macOS input source identifiers mapped to layout ids.
///
/// Matched as a suffix of the full identifier, so regional variants such as
/// `com.apple.keylayout.RussianWin` resolve correctly. The exact key positions
/// of whichever variant is installed are read from the system at startup (see
/// [`system_layouts`]), so `Russian` and `Russian – PC` both convert
/// correctly even though they put punctuation in different places.
const INPUT_SOURCES: &[(&str, &str)] = &[
    ("US", "us"),
    ("USExtended", "us"),
    ("USInternational-PC", "us"),
    ("ABC", "us"),
    ("ABC-India", "us"),
    ("British", "us"),
    ("British-PC", "us"),
    ("Australian", "us"),
    ("Canadian", "us"),
    ("Irish", "us"),
    ("Russian", "ru"),
    ("RussianWin", "ru"),
    ("Russian-Phonetic", "ru"),
    ("Ukrainian", "uk"),
    ("Ukrainian-PC", "uk"),
    ("Ukrainian-QWERTY", "uk"),
    ("Hebrew", "he"),
    ("Hebrew-PC", "he"),
    ("Hebrew-QWERTY", "he"),
    ("Arabic", "ar"),
    ("Arabic-AZERTY", "ar"),
    ("Arabic-PC", "ar"),
    ("Greek", "el"),
    ("GreekPolytonic", "el"),
    ("German", "de"),
    ("German-DIN-2137", "de"),
    ("French", "fr"),
    ("French-PC", "fr"),
    ("French-numerical", "fr"),
    ("Spanish", "es"),
    ("Spanish-ISO", "es"),
    ("Turkish", "tr"),
    ("Turkish-QWERTY-PC", "tr"),
];

/// Map a macOS input source id such as `com.apple.keylayout.Russian` to a
/// layout id.
pub fn layout_from_input_source(id: &str) -> Option<&'static str> {
    let tail = id.rsplit('.').next()?;
    INPUT_SOURCES
        .iter()
        .filter(|(name, _)| *name == tail)
        .map(|(_, layout)| *layout)
        .next()
        .or_else(|| {
            // Longest prefix first, so "Ukrainian-PC" beats "Ukrainian".
            INPUT_SOURCES
                .iter()
                .filter(|(name, _)| tail.starts_with(name))
                .max_by_key(|(name, _)| name.len())
                .map(|(_, layout)| *layout)
        })
}

fn source_id(source: *mut c_void) -> Option<String> {
    unsafe {
        let id_ref = TISGetInputSourceProperty(source, kTISPropertyInputSourceID);
        if id_ref.is_null() {
            return None;
        }
        Some(CFString::wrap_under_get_rule(id_ref as CFStringRef).to_string())
    }
}

/// The layout id for the keyboard the user is currently typing on.
///
/// # Must be called on the main thread
///
/// Text Input Services asserts the dispatch queue internally. Calling this
/// from any other thread — the event tap callback in particular — aborts the
/// process with SIGILL inside `dispatch_assert_queue`, on the very first
/// keystroke. Sample it on the main thread and share the result.
pub fn current_layout() -> Option<String> {
    unsafe {
        let source = TISCopyCurrentKeyboardLayoutInputSource();
        if source.is_null() {
            return None;
        }
        let result =
            source_id(source).and_then(|id| layout_from_input_source(&id).map(str::to_string));
        CFRelease(source as *const c_void);
        result
    }
}

/// Visit every input source the user has enabled.
fn for_each_enabled_source(mut f: impl FnMut(*mut c_void, &str) -> bool) {
    unsafe {
        // A null filter returns the input sources the user has actually
        // enabled, which is what we want: never touch a layout they have not
        // installed.
        let list = TISCreateInputSourceList(std::ptr::null(), false);
        if list.is_null() {
            return;
        }
        let count = CFArrayGetCount(list);
        for i in 0..count {
            let source = CFArrayGetValueAtIndex(list, i) as *mut c_void;
            if source.is_null() {
                continue;
            }
            let Some(id) = source_id(source) else {
                continue;
            };
            if !f(source, &id) {
                break;
            }
        }
        CFRelease(list as *const c_void);
    }
}

/// Switch the system keyboard to the layout MarKey just corrected into.
///
/// # Must be called on the main thread
///
/// Returns true when the layout was found among the user's enabled input
/// sources and selected.
pub fn select_layout(layout_id: &str) -> bool {
    let mut selected = false;
    for_each_enabled_source(|source, id| {
        if layout_from_input_source(id) == Some(layout_id) && id.contains("keylayout") {
            selected = unsafe { TISSelectInputSource(source) } == 0;
            return false;
        }
        true
    });
    selected
}

/// macOS virtual key codes for each of the 48 printable keys, in the same
/// physical order `rekey_core::layout` uses.
///
/// These are positions, not characters: key code 0 is the key labelled `a` on
/// a US keyboard, `ф` on a Russian one. That is exactly why replaying them
/// works — the active layout decides what they produce.
pub(crate) const KEYCODES: [u16; rekey_core::layout::KEY_COUNT] = [
    // ` 1 2 3 4 5 6 7 8 9 0 - =
    50, 18, 19, 20, 21, 23, 22, 26, 28, 25, 29, 27, 24, //
    // q w e r t y u i o p [ ] \
    12, 13, 14, 15, 17, 16, 32, 34, 31, 35, 33, 30, 42, //
    // a s d f g h j k l ; '
    0, 1, 2, 3, 5, 4, 38, 40, 37, 41, 39, //
    // z x c v b n m , . /
    6, 7, 8, 9, 11, 45, 46, 43, 47, 44, //
    // The ISO key (kVK_ISO_Section). On a Russian Mac keyboard this is where
    // some layouts put ё; without it, ё typed on the wrong layout broke the
    // word in two.
    10,
];

/// What one key produces on a keyboard layout, or `None` for a dead key or a
/// key that produces nothing printable.
unsafe fn translate(
    layout: *const c_void,
    keycode: u16,
    shift: bool,
    kbd_type: u32,
) -> Option<String> {
    const SHIFT_KEY: u32 = 0x0200;
    let mut dead_state: u32 = 0;
    let mut length: usize = 0;
    let mut buffer = [0u16; 8];
    let status = UCKeyTranslate(
        layout,
        keycode,
        0, // kUCKeyActionDown
        if shift { (SHIFT_KEY >> 8) & 0xFF } else { 0 },
        kbd_type,
        0,
        &mut dead_state,
        buffer.len(),
        &mut length,
        buffer.as_mut_ptr(),
    );
    if status != 0 || dead_state != 0 || length == 0 {
        return None;
    }
    let text = String::from_utf16_lossy(&buffer[..length.min(buffer.len())]);
    let printable = text.chars().all(|c| !c.is_control() && !c.is_whitespace());
    printable.then_some(text)
}

/// A layout id with what each of its 48 keys types, unshifted and shifted.
pub type KeyTable = (String, Vec<Option<String>>, Vec<Option<String>>);

/// The key tables of the user's own installed layouts, read from the system.
///
/// Returns `(layout id, lower, upper)` with 48 space-separated tokens each, in
/// the same physical order as [`rekey_core::layout`]; keys the system reports
/// nothing for are `None` and keep the built-in table's value. Feeding these
/// to [`rekey_core::layout::override_keys`] makes conversion follow the
/// layout actually in use — Apple's `Russian` puts `.` and `,` on Shift+7 and
/// Shift+6, `Russian – PC` on the `/` key — rather than a guess.
///
/// # Must be called on the main thread
pub fn system_layouts() -> Vec<KeyTable> {
    let mut out: Vec<KeyTable> = Vec::new();
    let kbd_type = unsafe { LMGetKbdType() } as u32;
    for_each_enabled_source(|source, id| {
        let Some(layout_id) = layout_from_input_source(id) else {
            return true;
        };
        if out.iter().any(|(l, _, _)| l == layout_id) {
            return true;
        }
        unsafe {
            let data = TISGetInputSourceProperty(source, kTISPropertyUnicodeKeyLayoutData);
            if data.is_null() {
                return true;
            }
            let data = CFData::wrap_under_get_rule(data as core_foundation::data::CFDataRef);
            let ptr = data.bytes().as_ptr() as *const c_void;
            let lower = KEYCODES
                .iter()
                .map(|k| translate(ptr, *k, false, kbd_type))
                .collect();
            let upper = KEYCODES
                .iter()
                .map(|k| translate(ptr, *k, true, kbd_type))
                .collect();
            out.push((layout_id.to_string(), lower, upper));
        }
        true
    });
    out
}

#[cfg(test)]
mod tests {
    use super::super::inject::{VK_RETURN, VK_SPACE, VK_TAB};
    use super::*;

    #[test]
    fn maps_apple_input_sources_to_layouts() {
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.US"),
            Some("us")
        );
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.ABC"),
            Some("us")
        );
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.Russian"),
            Some("ru")
        );
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.RussianWin"),
            Some("ru")
        );
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.Hebrew-PC"),
            Some("he")
        );
        assert_eq!(
            layout_from_input_source("com.apple.keylayout.Ukrainian-PC"),
            Some("uk")
        );
    }

    #[test]
    fn unknown_input_sources_are_not_guessed() {
        assert_eq!(
            layout_from_input_source("com.apple.inputmethod.Kotoeri"),
            None
        );
        assert_eq!(layout_from_input_source("com.apple.keylayout.Dvorak"), None);
    }

    #[test]
    fn keycodes_match_the_layout_table_positions() {
        use rekey_core::layout::KeyPress;
        let presses = rekey_core::layout::presses_for("привет ", "ru").unwrap();
        let codes: Vec<u16> = presses
            .iter()
            .map(|p| match *p {
                KeyPress::Key { index, .. } => KEYCODES[index],
                KeyPress::Space => VK_SPACE,
                KeyPress::Tab => VK_TAB,
                KeyPress::Enter => VK_RETURN,
            })
            .collect();
        assert_eq!(codes, vec![5, 4, 11, 2, 17, 45, VK_SPACE]);
    }

    #[test]
    fn every_keycode_is_distinct() {
        let mut seen = KEYCODES.to_vec();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "duplicate key code in the table");
    }
}
