//! macOS keyboard observation via `CGEventTap`, and injection via synthetic
//! `CGEvent`s.
//!
//! The tap is installed as an *active* filter when the system allows it, which
//! is what lets MarKey hold back the space that ends a mistyped word until the
//! correction has been typed — so the correction can never race the space, and
//! a Return that would send a chat message is only sent once the word in it is
//! right. Everything else the user types passes straight through; keys pressed
//! while a correction is being typed are held for those few milliseconds and
//! replayed in order. If the system only grants a listen-only tap, MarKey falls
//! back to correcting after the fact.
//!
//! Three OS signals matter for not misbehaving:
//!
//! * **Accessibility** must be granted or the tap cannot be created at all.
//! * **Secure input** is on whenever the focused field is a password field.
//!   Injecting there is both futile and alarming, so MarKey stands down.
//! * **The active input source** tells us which layout the user is typing on,
//!   which is the other half of every decision the detector makes.

use crate::{
    Capabilities, Context, Disposition, HookError, Injector, Key, KeyEvent, Modifiers, RawKey,
};
use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement,
    CGEventType, CallbackResult, EventField,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::sys::CGEventRef;
use foreign_types::ForeignType;
use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Marker written into the `USER_DATA` field of every event MarKey
/// synthesises, so the tap can recognise its own typing and ignore it. Without
/// this the app would react to its own corrections and loop.
const REKEY_SIGNATURE: i64 = 0x52_454B_4559; // "REKEY"

// -- Carbon / ApplicationServices bindings ----------------------------------

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: CFTypeRef, seconds: f32) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: i32, out: *mut c_void) -> bool;
}

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

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn TISCopyCurrentKeyboardLayoutInputSource() -> *mut c_void;
    fn TISGetInputSourceProperty(source: *mut c_void, key: CFStringRef) -> *mut c_void;
    fn TISCreateInputSourceList(properties: *const c_void, include_all: bool) -> CFArrayRef;
    fn TISSelectInputSource(source: *mut c_void) -> i32;
    static kTISPropertyInputSourceID: CFStringRef;
    static kTISPropertyUnicodeKeyLayoutData: CFStringRef;
    fn IsSecureEventInputEnabled() -> bool;
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

// -- permission -------------------------------------------------------------

/// True when the user has granted Accessibility access.
pub fn has_permission() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// Ask macOS to show the Accessibility prompt. Returns the current state; the
/// grant itself happens asynchronously in System Settings, and the hook starts
/// by itself once it does.
pub fn request_permission() -> bool {
    unsafe {
        let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
        let value = core_foundation::boolean::CFBoolean::true_value();
        let options = CFDictionary::from_CFType_pairs(&[(key, value)]);
        AXIsProcessTrustedWithOptions(options.as_CFTypeRef())
    }
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

/// macOS virtual key codes for each of the 47 printable keys, in the same
/// physical order `rekey_core::layout` uses.
///
/// These are positions, not characters: key code 0 is the key labelled `a` on
/// a US keyboard, `ф` on a Russian one. That is exactly why replaying them
/// works — the active layout decides what they produce.
const KEYCODES: [u16; rekey_core::layout::KEY_COUNT] = [
    // ` 1 2 3 4 5 6 7 8 9 0 - =
    50, 18, 19, 20, 21, 23, 22, 26, 28, 25, 29, 27, 24, //
    // q w e r t y u i o p [ ] \
    12, 13, 14, 15, 17, 16, 32, 34, 31, 35, 33, 30, 42, //
    // a s d f g h j k l ; '
    0, 1, 2, 3, 5, 4, 38, 40, 37, 41, 39, //
    // z x c v b n m , . /
    6, 7, 8, 9, 11, 45, 46, 43, 47, 44,
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

/// A layout id with what each of its 47 keys types, unshifted and shifted.
pub type KeyTable = (String, Vec<Option<String>>, Vec<Option<String>>);

/// The key tables of the user's own installed layouts, read from the system.
///
/// Returns `(layout id, lower, upper)` with 47 space-separated tokens each, in
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

/// Bundle identifier of the frontmost application.
///
/// # Must be called on the main thread
pub fn frontmost_app() -> Option<String> {
    use objc2_app_kit::NSWorkspace;
    let workspace = NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    app.bundleIdentifier().map(|id| id.to_string())
}

/// Names and bundle identifiers of running apps, for the exclusion picker.
/// Main-thread metadata only; no window titles or document contents.
pub fn running_applications() -> Vec<(String, String)> {
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let mut out = Vec::new();
    for app in workspace.runningApplications().iter() {
        // Background agents have no windows to type into.
        if app.activationPolicy() != objc2_app_kit::NSApplicationActivationPolicy::Regular {
            continue;
        }
        if let Some(id) = app.bundleIdentifier() {
            let id = id.to_string();
            let name = app
                .localizedName()
                .map(|s| s.to_string())
                .unwrap_or_else(|| id.clone());
            out.push((name, id));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An application installed on this Mac.
#[derive(Debug, Clone)]
pub struct InstalledApp {
    pub name: String,
    pub bundle_id: String,
    pub path: String,
}

/// Every application in the usual places, for the exclusion picker.
///
/// Reads bundle metadata only.
pub fn installed_applications() -> Vec<InstalledApp> {
    use objc2_foundation::{NSBundle, NSFileManager, NSString};
    let mut dirs = vec![
        std::path::PathBuf::from("/Applications"),
        std::path::PathBuf::from("/Applications/Utilities"),
        std::path::PathBuf::from("/System/Applications"),
        std::path::PathBuf::from("/System/Applications/Utilities"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::Path::new(&home).join("Applications"));
    }
    let mut bundles = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "app") {
                bundles.push(path);
            } else if path.is_dir() && !path.to_string_lossy().ends_with("Utilities") {
                // One level of folders, as in /Applications/Microsoft Office.
                if let Ok(inner) = std::fs::read_dir(&path) {
                    for entry in inner.flatten() {
                        let p = entry.path();
                        if p.extension().is_some_and(|e| e == "app") {
                            bundles.push(p);
                        }
                    }
                }
            }
        }
    }
    let files = NSFileManager::defaultManager();
    let mut out: Vec<InstalledApp> = Vec::new();
    for path in bundles {
        let path_str = path.to_string_lossy().to_string();
        let ns_path = NSString::from_str(&path_str);
        let Some(bundle) = NSBundle::bundleWithPath(&ns_path) else {
            continue;
        };
        let Some(id) = bundle.bundleIdentifier() else {
            continue;
        };
        let name = files.displayNameAtPath(&ns_path).to_string();
        let name = name.strip_suffix(".app").unwrap_or(&name).to_string();
        let bundle_id = id.to_string();
        if out
            .iter()
            .any(|a| a.bundle_id.eq_ignore_ascii_case(&bundle_id))
        {
            continue;
        }
        out.push(InstalledApp {
            name,
            bundle_id,
            path: path_str,
        });
    }
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

/// Where an application with this bundle id is installed.
pub fn application_path(bundle_id: &str) -> Option<String> {
    use objc2_foundation::NSString;
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let url = workspace.URLForApplicationWithBundleIdentifier(&NSString::from_str(bundle_id))?;
    url.path().map(|p| p.to_string())
}

/// An application's icon as PNG data, `size` points square.
///
/// # Must be called on the main thread
pub fn application_icon_png(path: &str, size: f64) -> Option<Vec<u8>> {
    use objc2::AllocAnyThread;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSCompositingOperation, NSImage};
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let icon = workspace.iconForFile(&NSString::from_str(path));
    let target = NSSize::new(size, size);
    let small = NSImage::initWithSize(NSImage::alloc(), target);
    #[allow(deprecated)]
    {
        small.lockFocus();
        icon.drawInRect_fromRect_operation_fraction(
            NSRect::new(NSPoint::new(0.0, 0.0), target),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
            NSCompositingOperation::SourceOver,
            1.0,
        );
        small.unlockFocus();
    }
    let tiff = small.TIFFRepresentation()?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)?;
    let png = unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }?;
    Some(png.to_vec())
}

/// True when a password field has taken over keyboard input.
pub fn secure_input_active() -> bool {
    unsafe { IsSecureEventInputEnabled() }
}

/// Sample everything the engine needs to know about the current moment.
///
/// # Must be called on the main thread
pub fn current_context() -> Context {
    Context {
        app: frontmost_app(),
        layout: current_layout(),
        secure_input: secure_input_active(),
    }
}

/// Play the system's quiet confirmation sound.
///
/// # Must be called on the main thread
pub fn play_sound() {
    use objc2_foundation::NSString;
    if let Some(sound) = objc2_app_kit::NSSound::soundNamed(&NSString::from_str("Tink")) {
        sound.play();
    }
}

// -- accessibility ----------------------------------------------------------

/// Length of the selection in the focused text field, when the application
/// reports one through Accessibility.
///
/// `Some(0)` is a definite "nothing is selected". `None` means the app does
/// not say — common in Electron and web views — and the caller has to decide
/// without it.
pub fn focused_selection_length() -> Option<usize> {
    const AX_VALUE_CF_RANGE: i32 = 4;
    #[repr(C)]
    struct CFRange {
        location: isize,
        length: isize,
    }
    unsafe {
        let system = AXUIElementCreateSystemWide();
        if system.is_null() {
            return None;
        }
        // Never let a hung application stall the shortcut.
        AXUIElementSetMessagingTimeout(system, 0.15);
        let focused_attr = CFString::from_static_string("AXFocusedUIElement");
        let mut focused: CFTypeRef = std::ptr::null();
        let status =
            AXUIElementCopyAttributeValue(system, focused_attr.as_concrete_TypeRef(), &mut focused);
        CFRelease(system);
        if status != 0 || focused.is_null() {
            return None;
        }
        AXUIElementSetMessagingTimeout(focused, 0.15);
        let range_attr = CFString::from_static_string("AXSelectedTextRange");
        let mut value: CFTypeRef = std::ptr::null();
        let status =
            AXUIElementCopyAttributeValue(focused, range_attr.as_concrete_TypeRef(), &mut value);
        CFRelease(focused);
        if status != 0 || value.is_null() {
            return None;
        }
        let mut range = CFRange {
            location: 0,
            length: 0,
        };
        let ok = AXValueGetValue(
            value,
            AX_VALUE_CF_RANGE,
            &mut range as *mut CFRange as *mut c_void,
        );
        CFRelease(value);
        (ok && range.length >= 0).then_some(range.length as usize)
    }
}

// -- clipboard --------------------------------------------------------------

/// Everything on the general pasteboard, so it can be put back after MarKey
/// borrows it to read or replace a selection.
pub struct PasteboardSnapshot {
    items: Vec<Vec<(String, Vec<u8>)>>,
}

/// The pasteboard's change counter. It moves whenever anything writes to it,
/// which is how a copy is known to have happened.
pub fn pasteboard_change_count() -> isize {
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount()
}

pub fn pasteboard_snapshot() -> PasteboardSnapshot {
    let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
    let mut items = Vec::new();
    if let Some(list) = pasteboard.pasteboardItems() {
        for item in list.iter() {
            let mut entries = Vec::new();
            for kind in item.types().iter() {
                if let Some(data) = item.dataForType(&kind) {
                    entries.push((kind.to_string(), data.to_vec()));
                }
            }
            items.push(entries);
        }
    }
    PasteboardSnapshot { items }
}

pub fn pasteboard_restore(snapshot: &PasteboardSnapshot) {
    use objc2::runtime::ProtocolObject;
    use objc2_app_kit::{NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSData, NSString};
    let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    if snapshot.items.is_empty() {
        return;
    }
    let mut objects = Vec::new();
    for entries in &snapshot.items {
        let item = NSPasteboardItem::new();
        for (kind, bytes) in entries {
            let data = NSData::with_bytes(bytes);
            item.setData_forType(&data, &NSString::from_str(kind));
        }
        objects.push(ProtocolObject::<dyn NSPasteboardWriting>::from_retained(
            item,
        ));
    }
    let array = NSArray::from_retained_slice(&objects);
    pasteboard.writeObjects(&array);
}

/// The plain text on the pasteboard.
pub fn pasteboard_string() -> Option<String> {
    let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
    let kind = unsafe { objc2_app_kit::NSPasteboardTypeString };
    pasteboard.stringForType(kind).map(|s| s.to_string())
}

/// Put plain text on the pasteboard, marked transient so clipboard managers
/// do not record MarKey's temporary use of it.
pub fn pasteboard_set_string(text: &str) {
    use objc2_foundation::NSString;
    let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
    pasteboard.clearContents();
    let kind = unsafe { objc2_app_kit::NSPasteboardTypeString };
    pasteboard.setString_forType(&NSString::from_str(text), kind);
    // The nspasteboard.org conventions every clipboard manager honours.
    for marker in [
        "org.nspasteboard.TransientType",
        "org.nspasteboard.AutoGeneratedType",
    ] {
        pasteboard.setString_forType(&NSString::from_str(""), &NSString::from_str(marker));
    }
}

// -- injection --------------------------------------------------------------

pub struct MacInjector {
    source: CGEventSource,
}

// CGEventSource is a Core Foundation object; MarKey only injects from its one
// injector thread.
unsafe impl Send for MacInjector {}
unsafe impl Sync for MacInjector {}

/// Pause between injected keystrokes.
///
/// Synthetic events posted back to back are not always all delivered: the
/// receiving application can drop or coalesce them, which shows up as a
/// replacement that deleted one character too few. A real keyboard never
/// produces keystrokes this close together, and a few milliseconds each is
/// imperceptible.
const KEYSTROKE_GAP: Duration = Duration::from_millis(3);

/// Virtual keycodes for the keys the layout tables do not cover.
const VK_DELETE: u16 = 51;
const VK_SPACE: u16 = 49;
const VK_TAB: u16 = 48;
const VK_RETURN: u16 = 36;
const VK_LEFT: u16 = 123;
const VK_RIGHT: u16 = 124;
const VK_C: u16 = 8;
const VK_V: u16 = 9;

impl MacInjector {
    pub fn new() -> Result<MacInjector, HookError> {
        // `Private` deliberately, not `HIDSystemState`. A HID-state source
        // reflects the modifier keys physically held right now, so a
        // correction fired while the user still has a modifier down would
        // inherit it.
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .map_err(|_| HookError::Os("cannot create a CGEventSource".into()))?;
        Ok(MacInjector { source })
    }

    fn post(&self, event: &CGEvent, flags: CGEventFlags) {
        event.set_flags(flags);
        // The signature must be stamped *after* the flags: setting flags
        // clears the event's user-data field.
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, REKEY_SIGNATURE);
        event.post(CGEventTapLocation::HID);
    }

    /// Press and release one key with exactly these modifiers.
    fn press(&self, keycode: u16, flags: CGEventFlags) {
        for down in [true, false] {
            if let Ok(event) = CGEvent::new_keyboard_event(self.source.clone(), keycode, down) {
                self.post(&event, flags);
            }
        }
        std::thread::sleep(KEYSTROKE_GAP);
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
        std::thread::sleep(KEYSTROKE_GAP);
    }
}

impl Injector for MacInjector {
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
            std::thread::sleep(KEYSTROKE_GAP);
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
    if !has_permission() {
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
