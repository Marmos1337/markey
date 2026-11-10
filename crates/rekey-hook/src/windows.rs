//! Windows keyboard and mouse observation via low-level hooks, and injection
//! via `SendInput`.
//!
//! The shape mirrors the macOS implementation. `WH_KEYBOARD_LL` sees every
//! keystroke and can hold one back — so the space that ends a mistyped word
//! is sent only after the correction. Mouse clicks and drags arrive through
//! asynchronous Raw Input, so a high-polling-rate gaming mouse never waits for
//! this process before delivering movement to the foreground application.
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
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Duration;
use windows::core::PWSTR;
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, POINT, WPARAM};
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
    GetAsyncKeyState, GetDoubleClickTime, GetKeyState, GetKeyboardLayout, GetKeyboardLayoutList,
    MapVirtualKeyExW, MapVirtualKeyW, SendInput, ToUnicodeEx, HKL, INPUT, INPUT_KEYBOARD,
    KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
    MAPVK_VK_TO_VSC, MAPVK_VSC_TO_VK, VIRTUAL_KEY, VK_BACK, VK_CAPITAL, VK_CONTROL, VK_DOWN,
    VK_END, VK_ESCAPE, VK_HOME, VK_LCONTROL, VK_LEFT, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_NEXT, VK_PRIOR, VK_RCONTROL, VK_RETURN, VK_RIGHT, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
    VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::{
    GetRawInputData, GetRegisteredRawInputDevices, RegisterRawInputDevices, HRAWINPUT, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RIDEV_REMOVE, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, EnumWindows, GetCursorPos,
    GetForegroundWindow, GetMessageW, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, PostMessageW, SendMessageTimeoutW,
    SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, GWL_EXSTYLE, GW_OWNER, HHOOK,
    HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, RI_MOUSE_LEFT_BUTTON_DOWN,
    RI_MOUSE_LEFT_BUTTON_UP, RI_MOUSE_MIDDLE_BUTTON_DOWN, RI_MOUSE_RIGHT_BUTTON_DOWN,
    SMTO_ABORTIFHUNG, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_INPUT,
    WM_INPUTLANGCHANGEREQUEST, WM_KEYDOWN, WM_KEYUP, WM_NULL, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WS_EX_TOOLWINDOW,
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
    fn sample() -> Self {
        let held = |vk: VIRTUAL_KEY| unsafe { GetAsyncKeyState(vk.0 as i32) < 0 };
        Self {
            lshift: held(VK_LSHIFT),
            rshift: held(VK_RSHIFT),
            lctrl: held(VK_LCONTROL),
            rctrl: held(VK_RCONTROL),
            lalt: held(VK_LMENU),
            ralt: held(VK_RMENU),
            lwin: held(VK_LWIN),
            rwin: held(VK_RWIN),
            caps: unsafe { GetKeyState(VK_CAPITAL.0 as i32) } & 1 != 0,
        }
    }

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
            // Fn is handled inside the keyboard; Windows never sees it.
            function: false,
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
    health: std::sync::Arc<crate::TapHealth>,
    mods: ModState,
    suspended: bool,
}

/// The service publishes its enabled/exclusion state off the input path.
/// Even modifier taps pass untouched in an excluded application.
static INPUT_SUSPENDED: AtomicBool = AtomicBool::new(false);

pub fn set_input_suspended(suspended: bool) {
    INPUT_SUSPENDED.store(suspended, Ordering::Relaxed);
}

thread_local! {
    /// The hook callbacks are bare `extern "system" fn`s with no user pointer,
    /// so their state is reached through thread-local storage. The hooks and
    /// their callbacks always run on the thread that installed them.
    static STATE: RefCell<Option<HookState>> = const { RefCell::new(None) };
    static INJECTION_TARGET: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
}

pub fn foreground_window() -> isize {
    unsafe { GetForegroundWindow().0 as isize }
}

/// Bind a worker's pending correction to the window that requested it.
pub fn set_injection_target(window: isize) {
    INJECTION_TARGET.with(|target| target.set(window));
}

pub fn injection_target() -> isize {
    INJECTION_TARGET.with(|target| target.get())
}

/// Carry a worker's target across dispatch to another thread (notably Tauri's
/// main-thread layout operations), restoring that thread's previous scope.
pub fn with_injection_target<T>(window: isize, action: impl FnOnce() -> T) -> Option<T> {
    struct Restore(isize);
    impl Drop for Restore {
        fn drop(&mut self) {
            set_injection_target(self.0);
        }
    }
    let _restore = Restore(injection_target());
    set_injection_target(window);
    injection_target_active().then(action)
}

pub fn injection_target_active() -> bool {
    INJECTION_TARGET.with(|target| target.get() == 0 || target.get() == foreground_window())
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
    if !injection_target_active() {
        return false;
    }
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

/// Wait, for at most `limit`, until the foreground application has worked
/// through its input. A pasted keypress is only read from the clipboard when
/// the application gets to it.
///
/// A message that is sent, unlike a keypress, returns only once the window's
/// thread is pumping messages again. It overtakes keys still in the queue, so
/// one round proves little: each further round lets one more of them through.
pub fn wait_for_foreground_input(limit: Duration) {
    const ROUNDS: u32 = 5;
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.0.is_null() {
        return;
    }
    let started = std::time::Instant::now();
    for _ in 0..ROUNDS {
        let left = limit.saturating_sub(started.elapsed());
        if left.is_zero() {
            return;
        }
        // SAFETY: WM_NULL carries nothing, and the result pointer is optional.
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_NULL,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                left.as_millis() as u32,
                None,
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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
        if INPUT_SUSPENDED.load(Ordering::Relaxed) {
            PASSWORD_FOCUSED.store(false, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(150));
            continue;
        }
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

/// Windows has no separate grant for watching input.
pub fn has_input_monitoring() -> bool {
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

/// Copy everything on the clipboard, or refuse past
/// [`crate::MAX_PASTEBOARD_SNAPSHOT`]; see the macOS twin.
pub fn pasteboard_snapshot() -> Result<PasteboardSnapshot, crate::SnapshotTooLarge> {
    let mut items = Vec::new();
    let mut budget = crate::SnapshotBudget::default();
    let Some(_open) = OpenedClipboard::open() else {
        return Ok(PasteboardSnapshot { items });
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
            // Sized before reading: a format too big to copy must stop the
            // snapshot, not be skipped and then lost on restore.
            budget.spend(unsafe { GlobalSize(HGLOBAL(handle.0)) })?;
            if let Some(bytes) = global_bytes(handle) {
                items.push((format, bytes));
            }
        }
    }
    Ok(PasteboardSnapshot { items })
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
    let units: Vec<u16> = (0..bytes.len() / 2)
        .map(|i| u16::from_le_bytes([bytes[2 * i], bytes[2 * i + 1]]))
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
        if !injection_target_active() {
            return;
        }
        unsafe {
            let sent = SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
            if sent != inputs.len() as u32 {
                log::warn!("Windows accepted {sent}/{} injected input events; the target may run at a higher integrity level", inputs.len());
            }
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

    /// Only release modifiers that this chord pressed. Releasing a physical
    /// Ctrl/Shift here would leave the foreground app's state out of sync.
    fn chord_inputs(
        modifiers: &[VIRTUAL_KEY],
        key: VIRTUAL_KEY,
        held: impl Fn(VIRTUAL_KEY) -> bool,
    ) -> Vec<INPUT> {
        let pressed: Vec<_> = modifiers.iter().copied().filter(|m| !held(*m)).collect();
        let mut inputs = Vec::new();
        for m in &pressed {
            inputs.push(Self::key_input(*m, false));
        }
        inputs.push(Self::key_input(key, false));
        inputs.push(Self::key_input(key, true));
        for m in pressed.iter().rev() {
            inputs.push(Self::key_input(*m, true));
        }
        inputs
    }

    fn chord(modifiers: &[VIRTUAL_KEY], key: VIRTUAL_KEY) {
        let inputs = Self::chord_inputs(modifiers, key, |vk| unsafe {
            GetAsyncKeyState(vk.0 as i32) < 0
        });
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
    let started = std::time::Instant::now();
    STATE.with(|s| match s.borrow_mut().as_mut() {
        Some(state) => {
            let disposition = (state.handler)(event, raw);
            // Windows silently drops a low-level hook that keeps answering
            // late, so the same timings are kept as on the Mac.
            state.health.record_event(started.elapsed());
            disposition
        }
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

    let suspended = INPUT_SUSPENDED.load(Ordering::Relaxed);
    STATE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            if st.suspended && !suspended {
                st.mods = ModState::sample();
            }
            st.suspended = suspended;
        }
    });
    if suspended {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

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

const WM_MOUSE_OBSERVED: u32 = WM_APP + 17;
const WM_STOP_MOUSE: u32 = WM_APP + 18;

fn registered_mouse() -> windows::core::Result<Option<RAWINPUTDEVICE>> {
    unsafe {
        let mut count = 0;
        let size = std::mem::size_of::<RAWINPUTDEVICE>() as u32;
        if GetRegisteredRawInputDevices(None, &mut count, size) == u32::MAX {
            return Err(windows::core::Error::from_thread());
        }
        if count == 0 {
            return Ok(None);
        }
        let mut devices = vec![RAWINPUTDEVICE::default(); count as usize];
        let read = GetRegisteredRawInputDevices(Some(devices.as_mut_ptr()), &mut count, size);
        if read == u32::MAX {
            return Err(windows::core::Error::from_thread());
        }
        Ok(devices
            .into_iter()
            .take(read as usize)
            .find(|device| device.usUsagePage == 1 && device.usUsage == 2))
    }
}

fn restore_mouse_registration(ours: HWND, previous: Option<RAWINPUTDEVICE>) {
    // Raw Input registration belongs to the whole process. Tao may already
    // own the mouse, and another component can replace us later.
    if registered_mouse()
        .ok()
        .flatten()
        .is_some_and(|current| current.hwndTarget == ours)
    {
        let restore = previous.unwrap_or(RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_REMOVE,
            hwndTarget: HWND::default(),
        });
        unsafe {
            let _ =
                RegisterRawInputDevices(&[restore], std::mem::size_of::<RAWINPUTDEVICE>() as u32);
        }
    }
}

impl MouseState {
    fn observe(
        &mut self,
        buttons: u32,
        pt: (i32, i32),
        time: u32,
        double_time: u32,
    ) -> Option<Key> {
        let near = |a: (i32, i32), b: (i32, i32)| {
            (a.0 as i64 - b.0 as i64).abs() <= 4 && (a.1 as i64 - b.1 as i64).abs() <= 4
        };
        let mut key = None;
        if buttons & RI_MOUSE_LEFT_BUTTON_DOWN != 0 {
            let double = self.last_click_time != 0
                && time.wrapping_sub(self.last_click_time) <= double_time
                && near(pt, self.last_click_at);
            self.left_down = true;
            self.drag_reported = false;
            self.down_at = pt;
            self.last_click_time = time;
            self.last_click_at = pt;
            key = Some(if double { Key::Selection } else { Key::Click });
        } else if self.left_down && !self.drag_reported && !near(pt, self.down_at) {
            self.drag_reported = true;
            key = Some(Key::Selection);
        }
        if buttons & RI_MOUSE_LEFT_BUTTON_UP != 0 {
            self.left_down = false;
        }
        if buttons & (RI_MOUSE_RIGHT_BUTTON_DOWN | RI_MOUSE_MIDDLE_BUTTON_DOWN) != 0 {
            key = Some(Key::Click);
        }
        key
    }
}

/// Receive mouse packets on a separate message loop. Only clicks/selections
/// are posted to the keyboard thread; mouse movement can neither stall a game
/// waiting for a low-level hook nor flood the keyboard thread's message queue.
fn spawn_mouse_observer(target: HWND) -> Result<(isize, std::thread::JoinHandle<()>), HookError> {
    let target = target.0 as isize;
    let (ready, receiver) = std::sync::mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("markey-mouse".into())
        .spawn(move || unsafe {
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("STATIC"),
                windows::core::w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            );
            let hwnd = match window {
                Ok(hwnd) => hwnd,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    return;
                }
            };
            let device = RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2,
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            };
            let previous = match registered_mouse() {
                Ok(previous) => previous,
                Err(error) => {
                    let _ = ready.send(Err(error.to_string()));
                    let _ = DestroyWindow(hwnd);
                    return;
                }
            };
            if let Err(error) =
                RegisterRawInputDevices(&[device], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            {
                let _ = ready.send(Err(error.to_string()));
                let _ = DestroyWindow(hwnd);
                return;
            }
            let _ = ready.send(Ok(hwnd.0 as isize));
            let mut mouse = MouseState::default();
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                if msg.message == WM_STOP_MOUSE {
                    break;
                }
                if msg.message == WM_INPUT {
                    let mut input = RAWINPUT::default();
                    let mut size = std::mem::size_of::<RAWINPUT>() as u32;
                    let read = GetRawInputData(
                        HRAWINPUT(msg.lParam.0 as *mut _),
                        RID_INPUT,
                        Some((&mut input as *mut RAWINPUT).cast()),
                        &mut size,
                        std::mem::size_of::<RAWINPUTHEADER>() as u32,
                    );
                    if INPUT_SUSPENDED.load(Ordering::Relaxed) {
                        mouse = MouseState::default();
                    } else if read != u32::MAX && input.header.dwType == RIM_TYPEMOUSE.0 {
                        let buttons = input.data.mouse.Anonymous.Anonymous.usButtonFlags as u32;
                        // Idle movement needs no cursor lookup or engine callback.
                        if buttons != 0 || (mouse.left_down && !mouse.drag_reported) {
                            let mut point = POINT::default();
                            if GetCursorPos(&mut point).is_ok() {
                                if let Some(key) = mouse.observe(
                                    buttons,
                                    (point.x, point.y),
                                    msg.time,
                                    GetDoubleClickTime(),
                                ) {
                                    let _ = PostMessageW(
                                        Some(HWND(target as *mut _)),
                                        WM_MOUSE_OBSERVED,
                                        WPARAM(usize::from(key == Key::Selection)),
                                        LPARAM(0),
                                    );
                                }
                            }
                        }
                    }
                }
                // The default procedure releases foreground Raw Input packets.
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            restore_mouse_registration(hwnd, previous);
            let _ = DestroyWindow(hwnd);
        })
        .map_err(|e| HookError::Os(e.to_string()))?;
    match receiver.recv() {
        Ok(Ok(hwnd)) => Ok((hwnd, thread)),
        result => {
            let _ = thread.join();
            Err(HookError::Os(match result {
                Ok(Err(error)) => error,
                _ => "mouse observer stopped before startup".into(),
            }))
        }
    }
}

/// Install the hooks and pump messages on the calling thread.
///
/// Low-level hooks only deliver events to a thread with a message loop, so
/// this blocks and callers put it on a dedicated thread.
pub fn run<F, R>(
    on_ready: R,
    on_key: F,
    health: std::sync::Arc<crate::TapHealth>,
) -> Result<(), HookError>
where
    F: FnMut(KeyEvent, Option<RawKey>) -> Disposition + Send + 'static,
    R: FnOnce(Capabilities),
{
    let mods = ModState::sample();
    STATE.with(|s| {
        *s.borrow_mut() = Some(HookState {
            handler: Box::new(on_key),
            health,
            mods,
            suspended: INPUT_SUSPENDED.load(Ordering::Relaxed),
        })
    });

    let keyboard = unsafe {
        let module = GetModuleHandleW(None).map_err(|e| HookError::Os(e.to_string()))?;
        let keyboard =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), Some(module.into()), 0)
                .map_err(|_| HookError::PermissionDenied)?;

        // The clipboard owner window lives here, where messages are pumped.
        let class: Vec<u16> = "STATIC".encode_utf16().chain(std::iter::once(0)).collect();
        let hwnd = CreateWindowExW(
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
        )
        .map_err(|error| {
            let _ = UnhookWindowsHookEx(keyboard);
            HookError::Os(error.to_string())
        })?;
        CLIPBOARD_WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);
        keyboard
    };
    let mouse = spawn_mouse_observer(HWND(CLIPBOARD_WINDOW.load(Ordering::SeqCst) as *mut _))
        .inspect_err(|_| unsafe {
            let _ = UnhookWindowsHookEx(keyboard);
            let hwnd = CLIPBOARD_WINDOW.swap(0, Ordering::SeqCst);
            if hwnd != 0 {
                let _ = DestroyWindow(HWND(hwnd as *mut _));
            }
        })?;
    on_ready(Capabilities { can_hold: true });

    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            if msg.message == WM_MOUSE_OBSERVED && !INPUT_SUSPENDED.load(Ordering::Relaxed) {
                let _ = dispatch(
                    KeyEvent {
                        character: None,
                        key: if msg.wParam.0 == 1 {
                            Key::Selection
                        } else {
                            Key::Click
                        },
                        modifiers: current_mods().modifiers(),
                        synthetic: false,
                    },
                    None,
                );
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(keyboard);
        let _ = PostMessageW(
            Some(HWND(mouse.0 as *mut _)),
            WM_STOP_MOUSE,
            WPARAM(0),
            LPARAM(0),
        );
        let _ = mouse.1.join();
        let hwnd = CLIPBOARD_WINDOW.swap(0, Ordering::SeqCst);
        if hwnd != 0 {
            let _ = DestroyWindow(HWND(hwnd as *mut _));
        }
    }
    STATE.with(|s| *s.borrow_mut() = None);
    let _: Option<HHOOK> = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    static MOUSE_REGISTRATION_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn correction_chords_balance_only_the_modifiers_they_press() {
        for held_ctrl in [false, true] {
            let inputs = WindowsInjector::chord_inputs(&[VK_LCONTROL, VK_LSHIFT], VK_LEFT, |key| {
                held_ctrl && key == VK_LCONTROL
            });
            let mut down = std::collections::HashSet::new();
            for input in inputs {
                let key = unsafe { input.Anonymous.ki };
                assert_eq!(key.dwExtraInfo, REKEY_SIGNATURE);
                assert!(
                    !held_ctrl || key.wVk != VK_LCONTROL,
                    "physical Ctrl must stay held"
                );
                if key.dwFlags.contains(KEYEVENTF_KEYUP) {
                    assert!(down.remove(&key.wVk.0), "release without an injected press");
                } else {
                    assert!(down.insert(key.wVk.0), "duplicate press");
                }
            }
            assert!(down.is_empty(), "no injected key may stay down");
        }
    }

    #[test]
    fn a_dispatched_layout_action_keeps_its_originating_window_guard() {
        set_injection_target(-1);
        let target = injection_target();
        let ran = std::sync::Arc::new(AtomicBool::new(false));
        let called = ran.clone();
        std::thread::spawn(move || {
            assert_eq!(injection_target(), 0, "dispatch thread starts unbound");
            assert!(
                with_injection_target(target, || called.store(true, Ordering::Relaxed)).is_none()
            );
            assert_eq!(injection_target(), 0, "temporary scope must be restored");
        })
        .join()
        .unwrap();
        assert!(!ran.load(Ordering::Relaxed));
        set_injection_target(0);
    }

    #[test]
    fn mouse_move_load_does_not_generate_keyboard_thread_work() {
        let mut mouse = MouseState::default();
        let start = std::time::Instant::now();
        for time in 1..80_001 {
            assert_eq!(mouse.observe(0, (time, time / 2), time as u32, 500), None);
        }
        eprintln!(
            "80,000 idle mouse packets: {:?}, 0 engine notifications",
            start.elapsed()
        );
        assert_eq!(
            mouse.observe(RI_MOUSE_LEFT_BUTTON_DOWN, (10, 10), 90_000, 500),
            Some(Key::Click)
        );
        assert_eq!(mouse.observe(0, (11, 11), 90_001, 500), None);
        assert_eq!(
            mouse.observe(0, (20, 20), 90_002, 500),
            Some(Key::Selection)
        );
        for time in 90_003..100_000 {
            assert_eq!(mouse.observe(0, (time, time), time as u32, 500), None);
        }
        assert_eq!(
            mouse.observe(RI_MOUSE_LEFT_BUTTON_UP, (20, 20), 100_000, 500),
            None
        );
        assert!(!mouse.left_down);
    }

    #[test]
    fn a_combined_mouse_press_release_never_leaves_a_drag_armed() {
        let mut mouse = MouseState::default();
        assert_eq!(
            mouse.observe(
                RI_MOUSE_LEFT_BUTTON_DOWN | RI_MOUSE_LEFT_BUTTON_UP,
                (10, 10),
                1000,
                500
            ),
            Some(Key::Click)
        );
        assert!(!mouse.left_down);
        assert_eq!(mouse.observe(0, (100, 100), 1001, 500), None);
    }

    #[test]
    fn double_click_still_selects_a_word() {
        let mut mouse = MouseState::default();
        assert_eq!(
            mouse.observe(RI_MOUSE_LEFT_BUTTON_DOWN, (10, 10), 1000, 500),
            Some(Key::Click)
        );
        mouse.observe(RI_MOUSE_LEFT_BUTTON_UP, (10, 10), 1001, 500);
        assert_eq!(
            mouse.observe(RI_MOUSE_LEFT_BUTTON_DOWN, (11, 10), 1100, 500),
            Some(Key::Selection)
        );
    }

    #[test]
    fn native_mouse_observer_starts_and_stops_without_a_global_mouse_hook() {
        let _guard = MOUSE_REGISTRATION_TEST.lock().unwrap();
        // No keyboard observer, no generated input, no owner settings.
        let (hwnd, thread) = spawn_mouse_observer(HWND::default()).unwrap();
        unsafe {
            PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_STOP_MOUSE,
                WPARAM(0),
                LPARAM(0),
            )
            .unwrap();
        }
        thread.join().unwrap();
    }

    #[test]
    fn native_mouse_registration_restores_previous_and_respects_a_new_owner() {
        let _guard = MOUSE_REGISTRATION_TEST.lock().unwrap();
        unsafe {
            let previous = registered_mouse().unwrap();
            let make_window = || {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    windows::core::w!("STATIC"),
                    windows::core::w!(""),
                    WINDOW_STYLE(0),
                    0,
                    0,
                    0,
                    0,
                    Some(HWND_MESSAGE),
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            let first = make_window();
            let later = make_window();
            let register = |hwnd| {
                RegisterRawInputDevices(
                    &[RAWINPUTDEVICE {
                        usUsagePage: 1,
                        usUsage: 2,
                        dwFlags: RIDEV_INPUTSINK,
                        hwndTarget: hwnd,
                    }],
                    std::mem::size_of::<RAWINPUTDEVICE>() as u32,
                )
                .unwrap()
            };
            let stop = |observer: (isize, std::thread::JoinHandle<()>)| {
                PostMessageW(
                    Some(HWND(observer.0 as *mut _)),
                    WM_STOP_MOUSE,
                    WPARAM(0),
                    LPARAM(0),
                )
                .unwrap();
                observer.1.join().unwrap();
            };
            register(first);
            let observer = spawn_mouse_observer(HWND::default()).unwrap();
            assert_eq!(
                registered_mouse().unwrap().unwrap().hwndTarget.0 as isize,
                observer.0
            );
            stop(observer);
            assert_eq!(registered_mouse().unwrap().unwrap().hwndTarget, first);
            let observer = spawn_mouse_observer(HWND::default()).unwrap();
            register(later);
            stop(observer);
            assert_eq!(registered_mouse().unwrap().unwrap().hwndTarget, later);
            restore_mouse_registration(later, previous);
            let _ = DestroyWindow(first);
            let _ = DestroyWindow(later);
        }
    }

    /// Opt-in desktop smoke test. Input is confined to this test's own edit
    /// window; every SendInput batch aborts if it stops being foreground.
    #[test]
    #[ignore = "opens an owned edit window; run with MARKEY_NATIVE_INPUT_TEST=1"]
    fn native_correction_in_owned_edit_window() {
        use rekey_core::{
            config::Config,
            engine::{Action, Engine},
            model::Models,
        };
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowTextW, PeekMessageW, SetForegroundWindow, PM_REMOVE, WS_OVERLAPPEDWINDOW,
            WS_VISIBLE,
        };
        assert_eq!(
            std::env::var("MARKEY_NATIVE_INPUT_TEST").as_deref(),
            Ok("1")
        );
        let previous = unsafe { GetForegroundWindow() };
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                windows::core::w!("EDIT"),
                windows::core::w!(""),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                60,
                60,
                500,
                160,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        struct Cleanup(HWND, HWND);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                unsafe {
                    set_injection_target(0);
                    if GetForegroundWindow() == self.0 {
                        let _ = SetForegroundWindow(self.1);
                    }
                    let _ = DestroyWindow(self.0);
                }
            }
        }
        let _cleanup = Cleanup(hwnd, previous);
        unsafe {
            let _ = SetForegroundWindow(hwnd);
            let _ = SetFocus(Some(hwnd));
        }
        set_injection_target(hwnd.0 as isize);
        let pump = || unsafe {
            let deadline = std::time::Instant::now() + Duration::from_millis(100);
            while std::time::Instant::now() < deadline {
                let mut msg = MSG::default();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        pump();
        assert!(
            injection_target_active(),
            "test window could not acquire focus; no input sent"
        );
        assert!(
            !ModState::sample().modifiers().is_shortcut(),
            "release Ctrl/Win before the native test"
        );
        let read = || unsafe {
            let mut text = [0u16; 128];
            let n = GetWindowTextW(hwnd, &mut text);
            String::from_utf16_lossy(&text[..n as usize])
        };
        let models = Models::load_dir(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../rekey-core/data/models"),
        )
        .unwrap();
        assert!(
            !models.is_empty(),
            "native test requires real language models"
        );
        let mut engine = Engine::new(models, Config::default());
        engine.set_hold_boundaries(true);
        let target = hwnd.0 as isize;
        let (actions, pending) = std::sync::mpsc::channel();
        let (ready, installed) = std::sync::mpsc::sync_channel(1);
        let hook = std::thread::spawn(move || {
            run(
                move |_| {
                    let _ = ready
                        .send(unsafe { windows::Win32::System::Threading::GetCurrentThreadId() });
                },
                move |event, _raw| {
                    if foreground_window() != target {
                        return Disposition::Pass;
                    }
                    let action = engine.on_key(&event, &current_context());
                    let disposition = if action.swallows_key() {
                        Disposition::Drop
                    } else {
                        Disposition::Pass
                    };
                    if !matches!(action, Action::None) {
                        let _ = actions.send(action);
                    }
                    disposition
                },
                std::sync::Arc::new(crate::TapHealth::new()),
            )
        });
        let hook_id = installed
            .recv_timeout(Duration::from_secs(3))
            .expect("native hook did not start");
        struct HookCleanup(u32, Option<std::thread::JoinHandle<Result<(), HookError>>>);
        impl Drop for HookCleanup {
            fn drop(&mut self) {
                unsafe {
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW(
                        self.0,
                        windows::Win32::UI::WindowsAndMessaging::WM_QUIT,
                        WPARAM(0),
                        LPARAM(0),
                    );
                }
                if let Some(thread) = self.1.take() {
                    let _ = thread.join();
                }
            }
        }
        let _hook_cleanup = HookCleanup(hook_id, Some(hook));
        let us = installed_layouts()
            .into_iter()
            .find(|hkl| layout_from_langid(langid(*hkl)) == Some("us"))
            .expect("native smoke requires an installed US layout");
        unsafe {
            windows::Win32::UI::Input::KeyboardAndMouse::ActivateKeyboardLayout(
                us,
                windows::Win32::UI::Input::KeyboardAndMouse::ACTIVATE_KEYBOARD_LAYOUT_FLAGS(0),
            )
            .unwrap();
        }
        let mut inputs = Vec::new();
        for vk in b"GHBDTN " {
            for up in [false, true] {
                let mut input = WindowsInjector::key_input(VIRTUAL_KEY(*vk as u16), up);
                // Distinct from MarKey's own output: exercise the real hook's
                // VK/scan-code translation, modifier tracking and boundary hold.
                input.Anonymous.ki.dwExtraInfo = 0x5052_4F42;
                inputs.push(input);
            }
        }
        WindowsInjector::send(&inputs);
        pump();
        assert!(
            read() == "ghbdtn",
            "scripted key events or held boundary did not reach the owned window correctly"
        );
        let correction = pending
            .recv_timeout(Duration::from_secs(1))
            .expect("native hook produced no correction");
        assert!(matches!(correction, Action::Replace { .. }));
        let injector = WindowsInjector::new().unwrap();
        correction.apply(&injector);
        pump();
        assert!(
            read() == "привет ",
            "native correction did not replace the scripted word"
        );
        // A queued action for a different window must send nothing.
        set_injection_target(-1);
        injector.type_text("SHOULD_NOT_APPEAR");
        pump();
        assert!(read() == "привет ", "foreground guard allowed stale input");
    }

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
