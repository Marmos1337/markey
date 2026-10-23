//! Windows keyboard and mouse observation via low-level hooks, and injection
//! via `SendInput`.
//!
//! The shape mirrors the macOS implementation. `WH_KEYBOARD_LL` sees every
//! keystroke and can hold one back — so the space that ends a mistyped word
//! is sent only after the correction — and `WH_MOUSE_LL` reports clicks and
//! drags, because after a click nothing behind the caret is known any more.
//!
//! Low-level hooks run on the thread that installed them, inside its message
//! loop, and the system only waits a few hundred milliseconds for them. All
//! state therefore lives in thread-local storage and nothing here blocks.
//!
//! Modifier state is tracked from the hook's own events. `GetKeyState` reads
//! the *calling thread's* input state, which for this thread never changes —
//! the original version relied on it and saw Shift as never held.

use crate::{
    Capabilities, Context, Disposition, HookError, Injector, Key, KeyEvent, Modifiers, RawKey,
};
use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;
use windows::core::PWSTR;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, GetKeyboardLayout, GetKeyboardLayoutList, MapVirtualKeyExW,
    MapVirtualKeyW, SendInput, ToUnicodeEx, HKL, INPUT, INPUT_KEYBOARD, KEYBDINPUT,
    KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC,
    MAPVK_VSC_TO_VK, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE,
    VK_HOME, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR,
    VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DispatchMessageW, EnumWindows, GetForegroundWindow,
    GetMessageW, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowThreadProcessId,
    IsWindowVisible, PostMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
    GWL_EXSTYLE, GW_OWNER, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG,
    MSLLHOOKSTRUCT, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_INPUTLANGCHANGEREQUEST, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MOUSEMOVE, WM_RBUTTONDOWN, WM_SYSKEYDOWN, WM_SYSKEYUP, WS_EX_TOOLWINDOW,
};

/// Marker stamped into the `dwExtraInfo` of every synthetic event MarKey
/// sends, so the hook can recognise its own typing. `LLKHF_INJECTED` alone is
/// not enough: other tools inject too, and their input is real input.
const REKEY_SIGNATURE: usize = 0x5245_4B45;

/// An unassigned virtual key. Pressing it between a modifier going down and
/// coming up turns a "tap" into a "chord" as far as Windows is concerned,
/// which keeps Start and the menu bar from opening on MarKey's shortcut.
const VK_MASK: u16 = 0xE8;

type Handler = Box<dyn FnMut(KeyEvent, Option<RawKey>) -> Disposition>;

/// Modifiers as the hook has seen them go down and up.
#[derive(Debug, Clone, Copy, Default)]
struct ModState {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lwin: bool,
    rwin: bool,
    caps: bool,
}

impl ModState {
    fn modifiers(&self) -> Modifiers {
        Modifiers {
            shift: self.lshift || self.rshift,
            control: self.lctrl || self.rctrl,
            alt: self.lalt || self.ralt,
            meta: self.lwin || self.rwin,
            right_shift: self.rshift,
            right_control: self.rctrl,
            right_alt: self.ralt,
            right_meta: self.rwin,
        }
    }

    /// Record a modifier key going down or up. Returns true when it was a
    /// modifier and the state actually changed (auto-repeat does not).
    fn update(&mut self, vk: VIRTUAL_KEY, down: bool) -> Option<bool> {
        let slot = match vk {
            VK_LSHIFT | VK_SHIFT => &mut self.lshift,
            VK_RSHIFT => &mut self.rshift,
            VK_LCONTROL | VK_CONTROL => &mut self.lctrl,
            VK_RCONTROL => &mut self.rctrl,
            VK_LMENU | VK_MENU => &mut self.lalt,
            VK_RMENU => &mut self.ralt,
            VK_LWIN => &mut self.lwin,
            VK_RWIN => &mut self.rwin,
            _ => return None,
        };
        let changed = *slot != down;
        *slot = down;
        Some(changed)
    }

    /// A keyboard state array for `ToUnicodeEx`, matching what is held.
    fn key_state(&self) -> [u8; 256] {
        let mut state = [0u8; 256];
        let set = |state: &mut [u8; 256], vk: VIRTUAL_KEY, on: bool| {
            if on {
                state[vk.0 as usize] = 0x80;
            }
        };
        let m = self.modifiers();
        set(&mut state, VK_SHIFT, m.shift);
        set(&mut state, VK_LSHIFT, self.lshift);
        set(&mut state, VK_RSHIFT, self.rshift);
        set(&mut state, VK_CONTROL, m.control);
        set(&mut state, VK_LCONTROL, self.lctrl);
        set(&mut state, VK_RCONTROL, self.rctrl);
        set(&mut state, VK_MENU, m.alt);
        set(&mut state, VK_LMENU, self.lalt);
        set(&mut state, VK_RMENU, self.ralt);
        if self.caps {
            state[VK_CAPITAL.0 as usize] |= 0x01;
        }
        state
    }
}

/// Mouse state for telling clicks, double clicks and drags apart.
#[derive(Debug, Default)]
struct MouseState {
    left_down: bool,
    drag_reported: bool,
    down_at: (i32, i32),
    last_click_time: u32,
    last_click_at: (i32, i32),
}

struct HookState {
    handler: Handler,
    mods: ModState,
    mouse: MouseState,
}

thread_local! {
    /// The hook callbacks are bare `extern "system" fn`s with no user pointer,
    /// so their state is reached through thread-local storage. The hooks and
    /// their callbacks always run on the thread that installed them.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
}

/// A message-only window on the hook thread, which owns the clipboard while
/// MarKey borrows it. It has to live on a thread that pumps messages: the
/// system *sends* the clipboard owner messages, and a sender waits for them.
static CLIPBOARD_WINDOW: AtomicIsize = AtomicIsize::new(0);

// -- keyboard layout --------------------------------------------------------

/// Windows language identifiers (the low word of an HKL) mapped to layout ids.
const LAYOUT_IDS: &[(u16, &str)] = &[
    (0x0409, "us"), // English (United States)
    (0x0809, "us"), // English (United Kingdom)
    (0x1009, "us"), // English (Canada)
    (0x0C09, "us"), // English (Australia)
    (0x0419, "ru"), // Russian
    (0x0422, "uk"), // Ukrainian
    (0x040D, "he"), // Hebrew
    (0x0401, "ar"), // Arabic (Saudi Arabia)
    (0x0C01, "ar"), // Arabic (Egypt)
    (0x0408, "el"), // Greek
    (0x0407, "de"), // German
    (0x0C07, "de"), // German (Austria)
    (0x0807, "de"), // German (Switzerland)
    (0x040C, "fr"), // French
    (0x080C, "fr"), // French (Belgium)
    (0x0C0C, "fr"), // French (Canada)
    (0x0C0A, "es"), // Spanish (Spain)
    (0x080A, "es"), // Spanish (Mexico)
    (0x041F, "tr"), // Turkish
];

/// Map the language identifier in an HKL to a layout id.
pub fn layout_from_langid(langid: u16) -> Option<&'static str> {
    LAYOUT_IDS
        .iter()
        .find(|(id, _)| *id == langid)
        .map(|(_, layout)| *layout)
}

fn langid(hkl: HKL) -> u16 {
    (hkl.0 as usize & 0xFFFF) as u16
}

fn foreground_hkl() -> Option<HKL> {
    unsafe {
        let hwnd = GetForegroundWindow();
        let thread = if hwnd.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(hwnd, None)
        };
        let hkl = GetKeyboardLayout(thread);
        (!hkl.0.is_null()).then_some(hkl)
    }
}

/// The layout of the thread that owns the foreground window — which is the one
/// the user is actually typing into, not necessarily our own.
pub fn current_layout() -> Option<String> {
    layout_from_langid(langid(foreground_hkl()?)).map(str::to_string)
}

/// Every keyboard layout the user has installed.
fn installed_layouts() -> Vec<HKL> {
    unsafe {
        let count = GetKeyboardLayoutList(None);
        if count <= 0 {
            return Vec::new();
        }
        let mut list = vec![HKL::default(); count as usize];
        let got = GetKeyboardLayoutList(Some(&mut list));
        list.truncate(got.max(0) as usize);
        list
    }
}

/// Switch the foreground window's keyboard to `layout_id`.
///
/// Windows keeps the layout per thread, so the change is requested from the
/// window that has focus — exactly what the language bar does. Only layouts
/// the user has installed are considered.
pub fn select_layout(layout_id: &str) -> bool {
    let Some(hkl) = installed_layouts()
        .into_iter()
        .find(|h| layout_from_langid(langid(*h)) == Some(layout_id))
    else {
        return false;
    };
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        PostMessageW(
            Some(hwnd),
            WM_INPUTLANGCHANGEREQUEST,
            WPARAM(0),
            LPARAM(hkl.0 as isize),
        )
        .is_ok()
    }
}

/// Scan codes of the 48 printable keys, in the physical order of
/// `rekey_core::layout`. Scan codes are positions; virtual-key codes are not —
/// on AZERTY the key in the Q position reports `VK_A`.
const SCANCODES: [u32; rekey_core::layout::KEY_COUNT] = [
    0x29, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, //
    0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x2B, //
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, //
    0x2C, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, //
    // The ISO key beside the left Shift.
    0x56,
];

/// A layout id with what each of its 48 keys types, unshifted and shifted.
pub type KeyTable = (String, Vec<Option<String>>, Vec<Option<String>>);

/// The key tables of the user's own installed layouts, read from the system,
/// so conversion follows the exact variant they use.
pub fn system_layouts() -> Vec<KeyTable> {
    let mut out: Vec<KeyTable> = Vec::new();
    for hkl in installed_layouts() {
        let Some(id) = layout_from_langid(langid(hkl)) else {
            continue;
        };
        if out.iter().any(|(l, _, _)| l == id) {
            continue;
        }
        let read = |shift: bool| -> Vec<Option<String>> {
            let mut state = [0u8; 256];
            if shift {
                state[VK_SHIFT.0 as usize] = 0x80;
                state[VK_LSHIFT.0 as usize] = 0x80;
            }
            SCANCODES
                .iter()
                .map(|&scan| unsafe {
                    let vk = MapVirtualKeyExW(scan, MAPVK_VSC_TO_VK, Some(hkl));
                    if vk == 0 {
                        return None;
                    }
                    let mut buf = [0u16; 8];
                    // Bit 2: leave the keyboard's dead-key state alone.
                    let n = ToUnicodeEx(vk, scan, &state, &mut buf, 1 << 2, Some(hkl));
                    if n != 1 {
                        return None;
                    }
                    let text = String::from_utf16_lossy(&buf[..1]);
                    text.chars()
                        .all(|c| !c.is_control() && !c.is_whitespace())
                        .then_some(text)
                })
                .collect()
        };
        out.push((id.to_string(), read(false), read(true)));
    }
    out
}

// -- applications -----------------------------------------------------------

fn process_exe(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        if !ok || len == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().map(str::to_string)
    }
}

/// Executable name of the foreground application, e.g. `chrome.exe`.
pub fn frontmost_app() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        process_exe(pid)
    }
}

/// Applications with a visible window, as `(name, executable)`, for the
/// exclusion picker.
pub fn running_applications() -> Vec<(String, String)> {
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        unsafe {
            let list = &mut *(lparam.0 as *mut Vec<u32>);
            let visible = IsWindowVisible(hwnd).as_bool();
            let owned = GetWindow(hwnd, GW_OWNER).is_ok_and(|o| !o.0.is_null());
            let tool = WINDOW_EX_STYLE(GetWindowLongW(hwnd, GWL_EXSTYLE) as u32)
                .contains(WS_EX_TOOLWINDOW);
            if visible && !owned && !tool && GetWindowTextLengthW(hwnd) > 0 {
                let mut pid = 0u32;
                GetWindowThreadProcessId(hwnd, Some(&mut pid));
                if !list.contains(&pid) {
                    list.push(pid);
                }
            }
        }
        true.into()
    }
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut pids as *mut Vec<u32> as isize));
    }
    let mut out: Vec<(String, String)> = pids
        .into_iter()
        .filter_map(process_exe)
        .filter(|exe| !exe.eq_ignore_ascii_case("markey.exe"))
        .map(|exe| {
            let stem = exe
                .strip_suffix(".exe")
                .or_else(|| exe.strip_suffix(".EXE"))
                .unwrap_or(&exe);
            let mut name: Vec<char> = stem.chars().collect();
            if let Some(first) = name.first_mut() {
                *first = first.to_ascii_uppercase();
            }
            (name.into_iter().collect(), exe)
        })
        .collect();
    out.sort_by_key(|(n, _)| n.to_lowercase());
    out.dedup_by(|a, b| a.1.eq_ignore_ascii_case(&b.1));
    out
}

/// Whether a password field has focus.
///
/// Windows has no global secure-input flag like macOS, so this asks UI
/// Automation whether the focused element is a password field — which covers
/// browsers and modern apps as well as classic edit controls. The question is
/// asked on a thread of its own, a few times a second: a hung application can
/// make it slow, and the keyboard hook must never wait for it.
pub fn secure_input_active() -> bool {
    static WATCHER: std::sync::Once = std::sync::Once::new();
    WATCHER.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("markey-password-watch".into())
            .spawn(watch_password_fields);
    });
    PASSWORD_FOCUSED.load(Ordering::Relaxed)
}

static PASSWORD_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn watch_password_fields() {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    let automation: IUIAutomation = unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("UI Automation is unavailable, password fields cannot be detected: {e}");
                return;
            }
        }
    };
    loop {
        let password = unsafe {
            automation
                .GetFocusedElement()
                .and_then(|e| e.CurrentIsPassword())
                .map(|b| b.as_bool())
                .unwrap_or(false)
        };
        PASSWORD_FOCUSED.store(password, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(150));
    }
}

pub fn current_context() -> Context {
    Context {
        app: frontmost_app(),
        layout: current_layout(),
        secure_input: secure_input_active(),
    }
}

/// Windows grants low-level hooks without a permission prompt.
pub fn has_permission() -> bool {
    true
}

pub fn request_permission() -> bool {
    true
}

/// Windows cannot tell cheaply whether the focused field has a selection.
pub fn focused_selection_length() -> Option<usize> {
    None
}

pub fn play_sound() {
    use windows::Win32::System::Diagnostics::Debug::MessageBeep;
    use windows::Win32::UI::WindowsAndMessaging::MB_OK;
    unsafe {
        let _ = MessageBeep(MB_OK);
    }
}

/// Switch Caps Lock off, after a word showed it was left on by mistake.
pub fn caps_lock_off() {
    WindowsInjector::chord(&[], VK_CAPITAL);
}

// -- clipboard --------------------------------------------------------------

const CF_UNICODETEXT: u32 = 13;

/// Formats that are GDI handles rather than memory, which cannot be copied
/// byte for byte. Their memory-based equivalents (CF_DIB for bitmaps) are on
/// the clipboard as well and carry the same content.
const HANDLE_FORMATS: &[u32] = &[2, 3, 9, 14, 0x80, 0x82, 0x83, 0x8E];

/// Everything on the clipboard, so it can be put back after MarKey borrows it.
pub struct PasteboardSnapshot {
    items: Vec<(u32, Vec<u8>)>,
}

struct OpenedClipboard;

impl OpenedClipboard {
    /// Open the clipboard, retrying briefly: another app may be holding it.
    fn open() -> Option<OpenedClipboard> {
        let owner = HWND(CLIPBOARD_WINDOW.load(Ordering::SeqCst) as *mut _);
        for _ in 0..20 {
            let opened = unsafe { OpenClipboard((!owner.0.is_null()).then_some(owner)) };
            if opened.is_ok() {
                return Some(OpenedClipboard);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }
}

impl Drop for OpenedClipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn global_bytes(handle: HANDLE) -> Option<Vec<u8>> {
    unsafe {
        let global = HGLOBAL(handle.0);
        let size = GlobalSize(global);
        if size == 0 || size > 64 * 1024 * 1024 {
            return None;
        }
        let ptr = GlobalLock(global) as *const u8;
        if ptr.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
        let _ = GlobalUnlock(global);
        Some(bytes)
    }
}

fn set_bytes(format: u32, bytes: &[u8]) {
    unsafe {
        let Ok(global) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
            return;
        };
        let ptr = GlobalLock(global) as *mut u8;
        if ptr.is_null() {
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        let _ = GlobalUnlock(global);
        // On success the system owns the memory.
        let _ = SetClipboardData(format, Some(HANDLE(global.0)));
    }
}

fn register_format(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { RegisterClipboardFormatW(windows::core::PCWSTR(wide.as_ptr())) }
}

/// The clipboard's change counter. It moves whenever anything writes to it,
/// which is how a copy is known to have happened.
pub fn pasteboard_change_count() -> isize {
    unsafe { GetClipboardSequenceNumber() as isize }
}

pub fn pasteboard_snapshot() -> PasteboardSnapshot {
    let mut items = Vec::new();
    let Some(_open) = OpenedClipboard::open() else {
        return PasteboardSnapshot { items };
    };
    let mut format = 0u32;
    loop {
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            break;
        }
        if HANDLE_FORMATS.contains(&format) {
            continue;
        }
        if let Ok(handle) = unsafe { GetClipboardData(format) } {
            if let Some(bytes) = global_bytes(handle) {
                items.push((format, bytes));
            }
        }
    }
    PasteboardSnapshot { items }
}

pub fn pasteboard_restore(snapshot: &PasteboardSnapshot) {
    let Some(_open) = OpenedClipboard::open() else {
        return;
    };
    unsafe {
        let _ = EmptyClipboard();
    }
    for (format, bytes) in &snapshot.items {
        set_bytes(*format, bytes);
    }
}

/// The plain text on the clipboard.
pub fn pasteboard_string() -> Option<String> {
    let _open = OpenedClipboard::open()?;
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT) }.ok()?;
    let bytes = global_bytes(handle)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Put plain text on the clipboard, marked so that Windows' clipboard history
/// and cloud clipboard do not keep MarKey's temporary use of it.
pub fn pasteboard_set_string(text: &str) {
    let Some(_open) = OpenedClipboard::open() else {
        return;
    };
    unsafe {
        let _ = EmptyClipboard();
    }
    let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    bytes.extend_from_slice(&[0, 0]);
    set_bytes(CF_UNICODETEXT, &bytes);
    set_bytes(
        register_format("ExcludeClipboardContentFromMonitorProcessing"),
        &[0],
    );
    set_bytes(
        register_format("CanIncludeInClipboardHistory"),
        &0u32.to_le_bytes(),
    );
    set_bytes(
        register_format("CanUploadToCloudClipboard"),
        &0u32.to_le_bytes(),
    );
}

// -- injection --------------------------------------------------------------

#[derive(Default)]
pub struct WindowsInjector;

impl WindowsInjector {
    pub fn new() -> Result<WindowsInjector, HookError> {
        Ok(WindowsInjector)
    }

    fn send(inputs: &[INPUT]) {
        unsafe {
            SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
        }
    }

    fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) } as u16;
        let extended = matches!(
            vk,
            VK_LEFT
                | VK_RIGHT
                | VK_UP
                | VK_DOWN
                | VK_HOME
                | VK_END
                | VK_PRIOR
                | VK_NEXT
                | VK_RCONTROL
                | VK_RMENU
                | VK_LWIN
                | VK_RWIN
        );
        let mut flags = if up {
            KEYEVENTF_KEYUP
        } else {
            KEYBD_EVENT_FLAGS(0)
        };
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: REKEY_SIGNATURE,
                },
            },
        }
    }

    /// One UTF-16 code unit as a synthetic keypress. Characters outside the
    /// basic plane are sent as their two surrogates, which Windows reassembles.
    fn unicode_input(unit: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: unit,
                    dwFlags: if up {
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_UNICODE
                    },
                    time: 0,
                    dwExtraInfo: REKEY_SIGNATURE,
                },
            },
        }
    }

    /// Press `key` with `modifiers` held, then release everything.
    fn chord(modifiers: &[VIRTUAL_KEY], key: VIRTUAL_KEY) {
        let mut inputs = Vec::new();
        for m in modifiers {
            inputs.push(Self::key_input(*m, false));
        }
        inputs.push(Self::key_input(key, false));
        inputs.push(Self::key_input(key, true));
        for m in modifiers.iter().rev() {
            inputs.push(Self::key_input(*m, true));
        }
        Self::send(&inputs);
        std::thread::sleep(Duration::from_millis(5));
    }

    /// Ctrl+C.
    pub fn copy(&self) {
        Self::chord(&[VK_LCONTROL], VIRTUAL_KEY(b'C' as u16));
    }

    /// Ctrl+V.
    pub fn paste(&self) {
        Self::chord(&[VK_LCONTROL], VIRTUAL_KEY(b'V' as u16));
    }

    /// Ctrl+Shift+←: select the word before the caret.
    pub fn select_word_left(&self) {
        Self::chord(&[VK_LCONTROL, VK_LSHIFT], VK_LEFT);
    }

    /// →: collapse a selection to its end.
    pub fn collapse_right(&self) {
        Self::chord(&[], VK_RIGHT);
    }

    /// Send again a key the user pressed while a correction was being typed.
    pub fn replay(&self, raw: &RawKey) {
        let flags = raw.flags;
        let held_command = flags & 0x3 != 0; // ctrl or alt bits, see `raw_flags`
                                             // Typed characters go back as text; Return, Tab and the like as keys,
                                             // which is what applications act on.
        let printable = !raw.text.is_empty() && raw.text.chars().all(|c| !c.is_control());
        if printable && !held_command && raw.code != VK_SPACE.0 {
            self.type_text(&raw.text);
            return;
        }
        let scan = ((flags >> 16) & 0xFF) as u16;
        let extended = flags & 0x100 != 0;
        let mut inputs = Vec::new();
        for up in [false, true] {
            let mut f = if up {
                KEYEVENTF_KEYUP
            } else {
                KEYBD_EVENT_FLAGS(0)
            };
            if extended {
                f |= KEYEVENTF_EXTENDEDKEY;
            }
            inputs.push(INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(raw.code),
                        wScan: scan,
                        dwFlags: f,
                        time: 0,
                        dwExtraInfo: REKEY_SIGNATURE,
                    },
                },
            });
        }
        Self::send(&inputs);
    }
}

impl Injector for WindowsInjector {
    fn backspace(&self, count: usize) {
        let mut inputs = Vec::with_capacity(count * 2);
        for _ in 0..count {
            inputs.push(WindowsInjector::key_input(VK_BACK, false));
            inputs.push(WindowsInjector::key_input(VK_BACK, true));
        }
        WindowsInjector::send(&inputs);
    }

    fn type_text(&self, text: &str) {
        // KEYEVENTF_UNICODE bypasses the active layout entirely, so the
        // corrected text arrives verbatim no matter which layout is selected.
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(WindowsInjector::unicode_input(unit, false));
            inputs.push(WindowsInjector::unicode_input(unit, true));
        }
        if !inputs.is_empty() {
            WindowsInjector::send(&inputs);
        }
    }

    fn boundary(&self, key: Key, shift: bool) -> bool {
        let vk = match key {
            Key::Space => VK_SPACE,
            Key::Tab => VK_TAB,
            Key::Enter => VK_RETURN,
            _ => return false,
        };
        if shift {
            WindowsInjector::chord(&[VK_LSHIFT], vk);
        } else {
            WindowsInjector::chord(&[], vk);
        }
        true
    }
}

// -- the hooks --------------------------------------------------------------

fn classify(vk: VIRTUAL_KEY, character: Option<char>, modifiers: Modifiers) -> Key {
    match vk {
        VK_BACK => Key::Backspace,
        VK_SPACE => Key::Space,
        VK_RETURN => Key::Enter,
        VK_TAB => Key::Tab,
        VK_ESCAPE => Key::Escape,
        VK_LEFT | VK_RIGHT | VK_UP | VK_DOWN | VK_HOME | VK_END | VK_PRIOR | VK_NEXT => {
            Key::Navigation
        }
        // Ctrl+A selects everything.
        VIRTUAL_KEY(0x41) if modifiers.control && !modifiers.alt => Key::Selection,
        _ => {
            if character.is_some_and(|c| !c.is_control() && !c.is_whitespace()) {
                Key::Character
            } else {
                Key::Other
            }
        }
    }
}

/// Resolve the character a key produces under the foreground window's layout
/// and the modifiers the hook has seen held.
fn character_for(vk: u32, scan: u32, mods: &ModState) -> String {
    let Some(hkl) = foreground_hkl() else {
        return String::new();
    };
    let state = mods.key_state();
    let mut buf = [0u16; 8];
    // Bit 2 keeps ToUnicodeEx from disturbing the kernel's dead-key state,
    // which would corrupt the user's next keystroke.
    let n = unsafe { ToUnicodeEx(vk, scan, &state, &mut buf, 1 << 2, Some(hkl)) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// Flags kept with a held key so it can be replayed: bit 0 Ctrl, bit 1 Alt,
/// bit 8 extended, bits 16–23 the scan code.
fn raw_flags(info: &KBDLLHOOKSTRUCT, mods: Modifiers) -> u64 {
    let mut flags = ((info.scanCode & 0xFF) as u64) << 16;
    if info.flags.contains(LLKHF_EXTENDED) {
        flags |= 0x100;
    }
    if mods.control {
        flags |= 0x1;
    }
    if mods.alt {
        flags |= 0x2;
    }
    flags
}

fn dispatch(event: KeyEvent, raw: Option<RawKey>) -> Disposition {
    STATE.with(|s| match s.borrow_mut().as_mut() {
        Some(state) => (state.handler)(event, raw),
        None => Disposition::Pass,
    })
}

fn current_mods() -> ModState {
    STATE.with(|s| s.borrow().as_ref().map(|st| st.mods).unwrap_or_default())
}

/// Mask a modifier tap: a dummy key between its press and release, then the
/// release itself, all synthetic.
fn mask_tap(vk: VIRTUAL_KEY) {
    let inputs = [
        WindowsInjector::key_input(VIRTUAL_KEY(VK_MASK), false),
        WindowsInjector::key_input(VIRTUAL_KEY(VK_MASK), true),
        WindowsInjector::key_input(vk, true),
    ];
    WindowsInjector::send(&inputs);
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let message = wparam.0 as u32;
    let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
    let up = message == WM_KEYUP || message == WM_SYSKEYUP;
    if !(down || up) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let vk = VIRTUAL_KEY(info.vkCode as u16);

    // Caps Lock is tracked whoever presses it — MarKey switches it off itself
    // after a word typed with it left on.
    if vk == VK_CAPITAL {
        if down {
            STATE.with(|s| {
                if let Some(st) = s.borrow_mut().as_mut() {
                    st.mods.caps = !st.mods.caps;
                }
            });
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // Only our own signature counts as our typing.
    if info.dwExtraInfo == REKEY_SIGNATURE {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // Modifiers: tracked on press and release; a change is reported, because
    // a tap of one is the manual shortcut.
    let changed = STATE.with(|s| {
        s.borrow_mut()
            .as_mut()
            .and_then(|st| st.mods.update(vk, down))
    });
    if let Some(changed) = changed {
        if changed {
            let event = KeyEvent {
                character: None,
                key: Key::ModifiersChanged,
                modifiers: current_mods().modifiers(),
                synthetic: false,
            };
            if dispatch(event, None) == Disposition::Mask
                && up
                && matches!(vk, VK_LMENU | VK_RMENU | VK_LWIN | VK_RWIN | VK_MENU)
            {
                mask_tap(vk);
                return LRESULT(1);
            }
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    if up {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let mods = current_mods();
    let modifiers = mods.modifiers();
    let text = character_for(info.vkCode, info.scanCode, &mods);
    let character = text.chars().next();
    let event = KeyEvent {
        character,
        key: classify(vk, character, modifiers),
        modifiers,
        synthetic: false,
    };
    let raw = RawKey {
        code: vk.0,
        flags: raw_flags(info, modifiers),
        text,
    };
    if dispatch(event, Some(raw)) == Disposition::Drop {
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let pt = (info.pt.x, info.pt.y);
        let near =
            |a: (i32, i32), b: (i32, i32), d: i32| (a.0 - b.0).abs() <= d && (a.1 - b.1).abs() <= d;
        let key = STATE.with(|s| {
            let mut s = s.borrow_mut();
            let st = s.as_mut()?;
            let m = &mut st.mouse;
            match wparam.0 as u32 {
                WM_LBUTTONDOWN => {
                    let double = info.time.wrapping_sub(m.last_click_time)
                        <= unsafe { GetDoubleClickTime() }
                        && near(pt, m.last_click_at, 4);
                    m.left_down = true;
                    m.drag_reported = false;
                    m.down_at = pt;
                    m.last_click_time = info.time;
                    m.last_click_at = pt;
                    Some(if double { Key::Selection } else { Key::Click })
                }
                WM_LBUTTONUP => {
                    m.left_down = false;
                    None
                }
                WM_MOUSEMOVE if m.left_down && !m.drag_reported && !near(pt, m.down_at, 4) => {
                    m.drag_reported = true;
                    Some(Key::Selection)
                }
                WM_RBUTTONDOWN | WM_MBUTTONDOWN => Some(Key::Click),
                _ => None,
            }
        });
        if let Some(key) = key {
            let event = KeyEvent {
                character: None,
                key,
                modifiers: current_mods().modifiers(),
                synthetic: false,
            };
            let _ = dispatch(event, None);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Install the hooks and pump messages on the calling thread.
///
/// Low-level hooks only deliver events to a thread with a message loop, so
/// this blocks and callers put it on a dedicated thread.
pub fn run<F, R>(on_ready: R, on_key: F) -> Result<(), HookError>
where
    F: FnMut(KeyEvent, Option<RawKey>) -> Disposition + Send + 'static,
    R: FnOnce(Capabilities),
{
    let mods = ModState {
        caps: unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0,
        ..ModState::default()
    };
    STATE.with(|s| {
        *s.borrow_mut() = Some(HookState {
            handler: Box::new(on_key),
            mods,
            mouse: MouseState::default(),
        })
    });

    let (keyboard, mouse) = unsafe {
        let module = GetModuleHandleW(None).map_err(|e| HookError::Os(e.to_string()))?;
        let keyboard =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(module.into()), 0)
                .map_err(|_| HookError::PermissionDenied)?;
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), Some(module.into()), 0).ok();

        // The clipboard owner window lives here, where messages are pumped.
        let class: Vec<u16> = "STATIC".encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            windows::core::PCWSTR(class.as_ptr()),
            windows::core::PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(module.into()),
            None,
        ) {
            CLIPBOARD_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        }
        (keyboard, mouse)
    };
    on_ready(Capabilities { can_hold: true });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(keyboard);
        if let Some(mouse) = mouse {
            let _ = UnhookWindowsHookEx(mouse);
        }
    }
    STATE.with(|s| *s.borrow_mut() = None);
    let _: Option<HHOOK> = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_windows_langids_to_layouts() {
        assert_eq!(layout_from_langid(0x0409), Some("us"));
        assert_eq!(layout_from_langid(0x0419), Some("ru"));
        assert_eq!(layout_from_langid(0x0422), Some("uk"));
        assert_eq!(layout_from_langid(0x040D), Some("he"));
        assert_eq!(layout_from_langid(0x0408), Some("el"));
    }

    #[test]
    fn unknown_langids_are_not_guessed() {
        assert_eq!(layout_from_langid(0x0411), None); // Japanese
        assert_eq!(layout_from_langid(0x0404), None); // Chinese (Traditional)
    }

    #[test]
    fn left_and_right_modifiers_are_tracked_separately() {
        let mut m = ModState::default();
        assert_eq!(m.update(VK_RCONTROL, true), Some(true));
        assert!(m.modifiers().control && m.modifiers().right_control);
        // Auto-repeat of a held modifier is not a change.
        assert_eq!(m.update(VK_RCONTROL, true), Some(false));
        assert_eq!(m.update(VK_LSHIFT, true), Some(true));
        assert!(m.modifiers().shift && !m.modifiers().right_shift);
        assert_eq!(m.update(VK_RCONTROL, false), Some(true));
        assert!(!m.modifiers().control);
        assert_eq!(m.update(VIRTUAL_KEY(0x41), true), None);
    }

    #[test]
    fn classifies_editing_keys() {
        let none = Modifiers::default();
        assert_eq!(classify(VK_BACK, None, none), Key::Backspace);
        assert_eq!(classify(VK_SPACE, Some(' '), none), Key::Space);
        assert_eq!(classify(VK_LEFT, None, none), Key::Navigation);
        assert_eq!(classify(VIRTUAL_KEY(0x41), Some('a'), none), Key::Character);
        let ctrl = Modifiers {
            control: true,
            ..Default::default()
        };
        assert_eq!(classify(VIRTUAL_KEY(0x41), None, ctrl), Key::Selection);
    }

    #[test]
    fn scancodes_are_distinct() {
        let mut seen = SCANCODES.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), SCANCODES.len());
    }
}
