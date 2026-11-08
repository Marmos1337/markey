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

mod inject;
mod layouts;
mod pasteboard;
mod tap;

pub use inject::MacInjector;
pub use layouts::{
    current_layout, layout_from_input_source, select_layout, system_layouts, KeyTable,
};
pub use pasteboard::{
    pasteboard_change_count, pasteboard_restore, pasteboard_set_string, pasteboard_snapshot,
    pasteboard_string, PasteboardSnapshot,
};
pub use tap::run;

use crate::Context;
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::{CFString, CFStringRef};
use std::ffi::c_void;

/// Marker written into the `USER_DATA` field of every event MarKey
/// synthesises, so the tap can recognise its own typing and ignore it. Without
/// this the app would react to its own corrections and loop.
pub(crate) const REKEY_SIGNATURE: i64 = 0x52_454B_4559; // "REKEY"

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
    fn CGPreflightListenEventAccess() -> bool;
}

#[link(name = "Carbon", kind = "framework")]
unsafe extern "C" {
    fn IsSecureEventInputEnabled() -> bool;
}

// -- permission -------------------------------------------------------------

/// True when the user has granted Accessibility access.
pub fn has_permission() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// True when macOS already allows this app to listen to input events. Only
/// looks: unlike `CGRequestListenEventAccess` it never shows a prompt.
pub fn has_input_monitoring() -> bool {
    // SAFETY: takes no arguments and only reads the process's TCC state.
    unsafe { CGPreflightListenEventAccess() }
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

// -- caps lock --------------------------------------------------------------

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const std::ffi::c_char) -> *mut c_void;
    fn IOServiceGetMatchingService(main_port: u32, matching: *mut c_void) -> u32;
    fn IOServiceOpen(service: u32, owning_task: u32, kind: u32, connect: *mut u32) -> i32;
    fn IOServiceClose(connect: u32) -> i32;
    fn IOObjectRelease(object: u32) -> i32;
    fn IOHIDSetModifierLockState(handle: u32, selector: i32, state: bool) -> i32;
}

unsafe extern "C" {
    static mach_task_self_: u32;
}

/// Switch Caps Lock off, after a word showed it was left on by mistake.
pub fn caps_lock_off() {
    const IO_HID_PARAM_CONNECT_TYPE: u32 = 1;
    const IO_HID_CAPS_LOCK_STATE: i32 = 1;
    unsafe {
        let matching = IOServiceMatching(c"IOHIDSystem".as_ptr());
        if matching.is_null() {
            return;
        }
        // The matching dictionary is consumed by the call.
        let service = IOServiceGetMatchingService(0, matching);
        if service == 0 {
            return;
        }
        let mut connect: u32 = 0;
        if IOServiceOpen(
            service,
            mach_task_self_,
            IO_HID_PARAM_CONNECT_TYPE,
            &mut connect,
        ) == 0
        {
            let status = IOHIDSetModifierLockState(connect, IO_HID_CAPS_LOCK_STATE, false);
            if status != 0 {
                log::debug!("could not switch Caps Lock off ({status})");
            }
            IOServiceClose(connect);
        }
        IOObjectRelease(service);
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
