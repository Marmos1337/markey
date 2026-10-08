//! Conservative AX-targeted operations. Reads only bounded originating fragments.
use core_foundation::{
    base::{CFEqual, CFGetTypeID, CFRelease, CFTypeRef, TCFType},
    runloop::{kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopAddSource, CFRunLoopRemoveSource},
    string::{CFString, CFStringGetTypeID, CFStringRef},
};
use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    time::Duration,
};
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub location: isize,
    pub length: isize,
}
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(pid: i32) -> CFTypeRef;
    fn AXUIElementGetTypeID() -> usize;
    fn AXValueGetTypeID() -> usize;
    fn AXValueGetType(value: CFTypeRef) -> i32;
    fn AXUIElementCopyAttributeValue(e: CFTypeRef, attr: CFStringRef, out: *mut CFTypeRef) -> i32;
    fn AXUIElementCopyParameterizedAttributeValue(
        e: CFTypeRef,
        attr: CFStringRef,
        param: CFTypeRef,
        out: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementSetAttributeValue(e: CFTypeRef, attr: CFStringRef, value: CFTypeRef) -> i32;
    fn AXUIElementGetPid(e: CFTypeRef, pid: *mut i32) -> i32;
    fn AXUIElementSetMessagingTimeout(e: CFTypeRef, seconds: f32) -> i32;
    fn AXUIElementIsAttributeSettable(e: CFTypeRef, attr: CFStringRef, out: *mut bool) -> i32;
    fn AXValueGetValue(value: CFTypeRef, kind: i32, out: *mut c_void) -> bool;
    fn AXValueCreate(kind: i32, value: *const c_void) -> CFTypeRef;
    fn AXObserverCreate(
        pid: i32,
        callback: unsafe extern "C" fn(CFTypeRef, CFTypeRef, CFStringRef, *mut c_void),
        out: *mut CFTypeRef,
    ) -> i32;
    fn AXObserverAddNotification(
        observer: CFTypeRef,
        e: CFTypeRef,
        name: CFStringRef,
        refcon: *mut c_void,
    ) -> i32;
    fn AXObserverGetRunLoopSource(
        observer: CFTypeRef,
    ) -> core_foundation::runloop::CFRunLoopSourceRef;
}
// Generic owned CF references remain local to the thread; no blanket Send/Sync.
struct Owned(CFTypeRef);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) }
        }
    }
}
struct AXElement(Owned);
// Apple AXUIElement references are thread-safe; only validated AX references cross threads.
unsafe impl Send for AXElement {}
unsafe impl Sync for AXElement {}
impl AXElement {
    fn from_owned(x: Owned) -> Option<Self> {
        if !x.0.is_null() && unsafe { CFGetTypeID(x.0) == AXUIElementGetTypeID() } {
            Some(Self(x))
        } else {
            None
        }
    }
    fn raw(&self) -> CFTypeRef {
        self.0 .0
    }
}
fn attr(e: &AXElement, name: &str) -> Option<Owned> {
    let mut out = std::ptr::null();
    if unsafe {
        AXUIElementCopyAttributeValue(e.raw(), CFString::new(name).as_concrete_TypeRef(), &mut out)
    } == 0
        && !out.is_null()
    {
        Some(Owned(out))
    } else {
        None
    }
}
fn optional_attr(e: &AXElement, name: &str) -> Result<Option<Owned>, i32> {
    let mut out = std::ptr::null();
    let status = unsafe {
        AXUIElementCopyAttributeValue(e.raw(), CFString::new(name).as_concrete_TypeRef(), &mut out)
    };
    match status {
        0 if !out.is_null() => Ok(Some(Owned(out))),
        -25205 | -25212 => Ok(None),
        other => Err(other),
    }
}
fn string(x: &Owned) -> Option<String> {
    if unsafe { CFGetTypeID(x.0) != CFStringGetTypeID() } {
        return None;
    }
    Some(unsafe { CFString::wrap_under_get_rule(x.0 as CFStringRef) }.to_string())
}
fn range(x: &Owned) -> Option<Range> {
    if unsafe { CFGetTypeID(x.0) != AXValueGetTypeID() || AXValueGetType(x.0) != 4 } {
        return None;
    }
    let mut r = Range {
        location: -1,
        length: -1,
    };
    if unsafe { AXValueGetValue(x.0, 4, &mut r as *mut _ as *mut c_void) }
        && r.location >= 0
        && r.length >= 0
    {
        Some(r)
    } else {
        None
    }
}
fn range_value(r: Range) -> Option<Owned> {
    let x = Owned(unsafe { AXValueCreate(4, &r as *const _ as *const c_void) });
    (!x.0.is_null()).then_some(x)
}
#[derive(Clone)]
pub struct Focus {
    field: Arc<AXElement>,
    window: Arc<AXElement>,
    pub pid: i32,
    pub caret: usize,
    pub selected: usize,
}
impl Focus {
    pub fn same_field(&self, other: &Self) -> bool {
        self.pid == other.pid
            && unsafe {
                CFEqual(self.field.raw(), other.field.raw()) != 0
                    && CFEqual(self.window.raw(), other.window.raw()) != 0
            }
    }
    pub fn selection(&self) -> Range {
        Range {
            location: self.caret as isize,
            length: self.selected as isize,
        }
    }
    pub fn with_range(&self, r: Range) -> Self {
        let mut f = self.clone();
        f.caret = r.location as usize;
        f.selected = r.length as usize;
        f
    }
    pub fn capture() -> Option<Self> {
        if super::macos::secure_input_active() {
            return None;
        }
        let root = AXElement::from_owned(Owned(unsafe { AXUIElementCreateSystemWide() }))?;
        unsafe {
            AXUIElementSetMessagingTimeout(root.raw(), 0.025);
        }
        let field = Arc::new(AXElement::from_owned(attr(&root, "AXFocusedUIElement")?)?);
        unsafe {
            AXUIElementSetMessagingTimeout(field.raw(), 0.005);
        }
        let role = string(&attr(&field, "AXRole")?)?;
        if role != "AXTextField" && role != "AXTextArea" {
            return None;
        }
        match optional_attr(&field, "AXSubrole") {
            Ok(Some(subrole)) => {
                if string(&subrole)?.contains("Secure") {
                    return None;
                }
            }
            Ok(None) => {}
            Err(_) => return None,
        }
        for key in ["AXSelectedText", "AXSelectedTextRange"] {
            let mut yes = false;
            if unsafe {
                AXUIElementIsAttributeSettable(
                    field.raw(),
                    CFString::new(key).as_concrete_TypeRef(),
                    &mut yes,
                )
            } != 0
                || !yes
            {
                return None;
            }
        }
        let selection = range(&attr(&field, "AXSelectedTextRange")?)?;
        let window = Arc::new(AXElement::from_owned(attr(&field, "AXWindow")?)?);
        let mut pid = 0;
        if unsafe { AXUIElementGetPid(field.raw(), &mut pid) } != 0 || pid <= 0 {
            return None;
        }
        Some(Self {
            field,
            window,
            pid,
            caret: selection.location as usize,
            selected: selection.length as usize,
        })
    }
    /// Reads only the exact requested fragment (max128UTF16units), never AXValue.
    pub fn fragment(&self, r: Range) -> Option<String> {
        if r.location < 0 || r.length < 0 || r.length > 128 {
            return None;
        }
        let parameter = range_value(r)?;
        let mut out = std::ptr::null();
        if unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                self.field.raw(),
                CFString::new("AXStringForRange").as_concrete_TypeRef(),
                parameter.0,
                &mut out,
            )
        } != 0
            || out.is_null()
        {
            return None;
        }
        string(&Owned(out))
    }
    /// Proof of a whole-word start, limited to one UTF16 predecessor.
    pub fn boundary(&self, start: usize) -> bool {
        if start == 0 {
            return true;
        }
        let predecessor = self.fragment(Range {
            location: (start - 1) as isize,
            length: 1,
        });
        boundary_proven(start, predecessor.as_deref())
    }
    /// Fast post-selection checks avoid main-thread dispatch and full metadata capture.
    pub fn still_focused(&self) -> bool {
        if super::macos::secure_input_active() {
            return false;
        }
        let Some(root) = AXElement::from_owned(Owned(unsafe { AXUIElementCreateSystemWide() }))
        else {
            return false;
        };
        unsafe {
            AXUIElementSetMessagingTimeout(root.raw(), 0.005);
        }
        let Some(raw) = attr(&root, "AXFocusedUIElement") else {
            return false;
        };
        let Some(current) = AXElement::from_owned(raw) else {
            return false;
        };
        unsafe { CFEqual(current.raw(), self.field.raw()) != 0 }
    }
    pub fn current_range(&self) -> Option<Range> {
        range(&attr(&self.field, "AXSelectedTextRange")?)
    }
    pub fn set_selection(&self, r: Range) -> bool {
        let Some(v) = range_value(r) else {
            return false;
        };
        (unsafe {
            AXUIElementSetAttributeValue(
                self.field.raw(),
                CFString::new("AXSelectedTextRange").as_concrete_TypeRef(),
                v.0,
            )
        }) == 0
    }
    pub fn set_selected_text(&self, text: &str) -> bool {
        (unsafe {
            AXUIElementSetAttributeValue(
                self.field.raw(),
                CFString::new("AXSelectedText").as_concrete_TypeRef(),
                CFString::new(text).as_CFTypeRef(),
            )
        }) == 0
    }
}
pub fn boundary_proven(start: usize, predecessor: Option<&str>) -> bool {
    start == 0 || predecessor.is_some_and(|s| !s.is_empty() && s.chars().all(char::is_whitespace))
}
#[derive(Clone)]
pub struct Expected {
    pub boundary_start: usize,
    pub focus: Focus,
    pub fragment_range: Range,
    pub fragment: String,
    pub revision: u64,
}
impl Expected {
    pub fn matches(&self, now: &Focus, policy: impl Fn(u64) -> bool) -> bool {
        policy(self.revision)
            && now.same_field(&self.focus)
            && now.selection() == self.focus.selection()
            && policy(self.revision)
            && now.boundary(self.boundary_start)
            && policy(self.revision)
            && ((self.fragment_range.length == 0 && self.fragment.is_empty())
                || now.fragment(self.fragment_range).as_deref() == Some(self.fragment.as_str()))
    }
}
/// Only text/selection notifications can be deferred while one bootstrap event
/// is being reconciled. Focus/window changes and stale policy always invalidate.
#[derive(Default)]
pub struct BootstrapState {
    scope: Mutex<Option<(u64, u64)>>,
    hints: AtomicU64,
}
impl BootstrapState {
    pub fn begin(self: &Arc<Self>, generation: u64, revision: u64) -> Option<BootstrapGuard> {
        let mut scope = self.scope.lock().ok()?;
        if scope.is_some() {
            return None;
        }
        self.hints.store(0, Ordering::SeqCst);
        *scope = Some((generation, revision));
        Some(BootstrapGuard {
            state: self.clone(),
            token: (generation, revision),
        })
    }
    pub fn defer_hint(
        &self,
        focus_change: bool,
        generation: u64,
        policy: impl Fn(u64) -> bool,
    ) -> bool {
        if focus_change {
            return false;
        }
        let Ok(scope) = self.scope.lock() else {
            return false;
        };
        if scope
            .as_ref()
            .is_some_and(|(g, r)| *g == generation && policy(*r))
        {
            self.hints.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }
}
pub struct BootstrapGuard {
    state: Arc<BootstrapState>,
    token: (u64, u64),
}
impl BootstrapGuard {
    pub fn finish(self) -> u64 {
        // Closing the scope and taking its count is synchronized with callbacks.
        let Ok(mut scope) = self.state.scope.lock() else {
            return u64::MAX;
        };
        if *scope != Some(self.token) {
            return u64::MAX;
        }
        *scope = None;
        self.state.hints.load(Ordering::SeqCst)
    }
}
impl Drop for BootstrapGuard {
    fn drop(&mut self) {
        if let Ok(mut scope) = self.state.scope.lock() {
            if *scope == Some(self.token) {
                *scope = None;
            }
        }
    }
}
struct WatchState {
    bootstrap: Arc<BootstrapState>,
    generation: Arc<AtomicU64>,
    epoch: Arc<AtomicU64>,
    dirty: Arc<AtomicU64>,
    invalidated: Arc<AtomicBool>,
    expected: Arc<Mutex<Vec<Expected>>>,
    policy: Arc<dyn Fn(u64) -> bool + Send + Sync>,
}
unsafe extern "C" fn notify(
    _observer: CFTypeRef,
    _element: CFTypeRef,
    name: CFStringRef,
    refcon: *mut c_void,
) {
    if refcon.is_null() {
        return;
    }
    let state = unsafe { &*(refcon as *const WatchState) };
    // A notification is only an invalidation hint; payload text is never read.
    let focus_change =
        if name.is_null() || unsafe { CFGetTypeID(name as CFTypeRef) != CFStringGetTypeID() } {
            true
        } else {
            unsafe { CFString::wrap_under_get_rule(name) }
                .to_string()
                .contains("Focused")
        };
    if focus_change {
        state.epoch.fetch_add(1, Ordering::SeqCst);
        state.generation.fetch_add(1, Ordering::SeqCst);
        state.invalidated.store(true, Ordering::SeqCst);
    } else {
        // The host may notify before the tap has enqueued the corresponding
        // event. A single reconciler validates this hint against the whole
        // known queue. Notifications never authorize a write or invent input.
        state.dirty.fetch_add(1, Ordering::SeqCst);
    }
}
#[derive(Clone)]
pub struct Watch {
    generation: Arc<AtomicU64>,
    policy: Arc<dyn Fn(u64) -> bool + Send + Sync>,
    pub dirty: Arc<AtomicU64>,
    bootstrap: Arc<BootstrapState>,
    tx: mpsc::Sender<Focus>,
    ready: Arc<Mutex<Option<Focus>>>,
    expected: Arc<Mutex<Vec<Expected>>>,
    pub invalidated: Arc<AtomicBool>,
}
impl Watch {
    pub fn start(
        generation: Arc<AtomicU64>,
        epoch: Arc<AtomicU64>,
        policy: Arc<dyn Fn(u64) -> bool + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        let owned_gen = generation.clone();
        let owned_policy = policy.clone();
        let bootstrap = Arc::new(BootstrapState::default());
        let boot = bootstrap.clone();
        let (tx, rx) = mpsc::channel::<Focus>();
        let ready = Arc::new(Mutex::new(None));
        let expected = Arc::new(Mutex::new(Vec::new()));
        let invalidated = Arc::new(AtomicBool::new(false));
        let dirty = Arc::new(AtomicU64::new(0));
        let changes = dirty.clone();
        let r = ready.clone();
        let xs = expected.clone();
        let invalid = invalidated.clone();
        std::thread::Builder::new()
            .name("rekey-ax-observer".into())
            .spawn(move || {
                let state = Box::new(WatchState {
                    bootstrap: boot,
                    generation: generation.clone(),
                    epoch: epoch.clone(),
                    dirty: changes,
                    invalidated: invalid,
                    expected: xs,
                    policy,
                });
                let refcon = (&*state as *const WatchState) as *mut c_void;
                let runloop = CFRunLoop::get_current();
                let mut observer: Option<Owned> = None;
                let mut installed: Option<Focus> = None;
                loop {
                    match rx.recv_timeout(Duration::from_millis(10)) {
                        Ok(focus) => {
                            if installed.as_ref().is_some_and(|x| x.same_field(&focus)) {
                                continue;
                            }
                            if let Some(old) = observer.take() {
                                unsafe {
                                    CFRunLoopRemoveSource(
                                        runloop.as_concrete_TypeRef(),
                                        AXObserverGetRunLoopSource(old.0),
                                        kCFRunLoopDefaultMode,
                                    );
                                }
                                drop(old);
                            }
                            installed = None;
                            if let Ok(mut slot) = r.lock() {
                                *slot = None;
                            }
                            let mut raw = std::ptr::null();
                            if unsafe { AXObserverCreate(focus.pid, notify, &mut raw) } != 0
                                || raw.is_null()
                            {
                                continue;
                            }
                            let own = Owned(raw);
                            let Some(app) = AXElement::from_owned(Owned(unsafe {
                                AXUIElementCreateApplication(focus.pid)
                            })) else {
                                continue;
                            };
                            let mut ok = true;
                            for (element, name) in [
                                (&app, "AXFocusedUIElementChanged"),
                                (&app, "AXFocusedWindowChanged"),
                                (&*focus.field, "AXSelectedTextChanged"),
                                (&*focus.field, "AXValueChanged"),
                            ] {
                                if unsafe {
                                    AXObserverAddNotification(
                                        own.0,
                                        element.raw(),
                                        CFString::new(name).as_concrete_TypeRef(),
                                        refcon,
                                    )
                                } != 0
                                {
                                    ok = false;
                                    break;
                                }
                            }
                            if !ok {
                                log::debug!("AX diagnostic observer subscription rejected");
                                continue;
                            }
                            unsafe {
                                CFRunLoopAddSource(
                                    runloop.as_concrete_TypeRef(),
                                    AXObserverGetRunLoopSource(own.0),
                                    kCFRunLoopDefaultMode,
                                );
                            }
                            log::debug!("AX diagnostic observer ready");
                            // Replacing a subscription also ends the old lineage.
                            epoch.fetch_add(1, Ordering::SeqCst);
                            generation.fetch_add(1, Ordering::SeqCst);
                            installed = Some(focus.clone());
                            observer = Some(own);
                            if let Ok(mut slot) = r.lock() {
                                *slot = Some(focus);
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            generation.fetch_add(1, Ordering::SeqCst);
                            if let Some(old) = observer.take() {
                                unsafe {
                                    CFRunLoopRemoveSource(
                                        runloop.as_concrete_TypeRef(),
                                        AXObserverGetRunLoopSource(old.0),
                                        kCFRunLoopDefaultMode,
                                    );
                                }
                            }
                            break;
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if observer.is_some() {
                        CFRunLoop::run_in_mode(
                            unsafe { kCFRunLoopDefaultMode },
                            Duration::from_millis(10),
                            false,
                        );
                    }
                }
            })?;
        Ok(Self {
            generation: owned_gen,
            policy: owned_policy,
            dirty,
            bootstrap,
            tx,
            ready,
            expected,
            invalidated,
        })
    }
    pub fn begin_bootstrap(&self, generation: u64, revision: u64) -> Option<BootstrapGuard> {
        self.bootstrap.begin(generation, revision)
    }
    pub fn is_ready(&self, focus: &Focus) -> bool {
        self.ready
            .lock()
            .ok()
            .is_some_and(|x| x.as_ref().is_some_and(|f| f.same_field(focus)))
    }
    pub fn ensure(&self, focus: &Focus) -> bool {
        if self.is_ready(focus) {
            true
        } else {
            let _ = self.tx.send(focus.clone());
            false
        }
    }
    /// Publication is serialized with all other expectations. A stale queued
    /// action can neither replace a newer event's expectations nor clear them.
    pub fn expect_owned(&self, generation: u64, revision: u64, xs: Vec<Expected>) -> bool {
        let Ok(mut expected) = self.expected.lock() else {
            return false;
        };
        if self.generation.load(Ordering::SeqCst) != generation || !(self.policy)(revision) {
            return false;
        }
        *expected = xs;
        true
    }
    pub fn expect(&self, xs: Vec<Expected>) {
        if let Ok(mut x) = self.expected.lock() {
            *x = xs;
        }
    }
    pub fn clear(&self) {
        self.expect(Vec::new());
    }
}
