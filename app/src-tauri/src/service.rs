//! The background service: watches the keyboard, runs the engine, applies
//! corrections.
//!
//! The engine runs synchronously inside the keyboard hook, so it sees every
//! keystroke in order and can decide, before a word-ending key reaches the
//! application, whether that key must be held back while the word is fixed.
//! The actual typing happens on a separate injector thread: the hook callback
//! has to return within milliseconds or macOS disables it.
//!
//! While the injector is typing a correction, further keys the user presses are
//! held and queued behind it, then replayed in order — so fast typing never
//! lands in the middle of a correction.

use rekey_core::config::Config;
use rekey_core::engine::{Action, Engine};
use rekey_core::input::{Context, TextWriter};
use rekey_hook::{platform, Capabilities, Disposition, RawKey};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// Shared handle to the running engine, used by the UI and the tray.
pub type SharedEngine = Arc<Mutex<Engine>>;

/// Called with the new configuration when the engine changed it by itself —
/// when the user undid a correction and the word became an exception.
pub type ConfigSink = Arc<dyn Fn(Config) + Send + Sync>;

/// How often the frontmost app, active layout and secure-input state are
/// sampled. Text Input Services is main-thread-only, so this cannot happen in
/// the hook; a short interval keeps the sample current within a keystroke or
/// two, and the engine double-checks the layout against the characters typed.
const CONTEXT_INTERVAL: Duration = Duration::from_millis(40);

/// Pause before typing a correction when the word-ending key could not be
/// held back: the key is still on its way to the application, and replacing
/// immediately can race it.
const LATE_REPLACE_DELAY: Duration = Duration::from_millis(25);

/// The same pause when the key was held: only the word's own letters may
/// still be in flight, and they are ahead of anything posted now.
const HELD_REPLACE_DELAY: Duration = Duration::from_millis(8);

/// Keys are never held for longer than this, whatever happens to the
/// injector. A stuck correction must not take the keyboard with it.
const MAX_HOLD: Duration = Duration::from_millis(1500);

/// How often to re-check whether Accessibility has been granted.
const PERMISSION_POLL: Duration = Duration::from_secs(1);

/// How long to wait for a requested layout to become active, and in what steps.
const LAYOUT_SETTLE_STEP: Duration = Duration::from_millis(10);
const LAYOUT_SETTLE_STEPS: usize = 10;
const MAIN_THREAD_TIMEOUT: Duration = Duration::from_millis(400);

/// How long to wait for the frontmost app to answer ⌘C.
const COPY_TIMEOUT: Duration = Duration::from_millis(450);

/// How long the converted text stays on the clipboard before the user's own
/// clipboard is put back. The paste is handled asynchronously by the app.
const PASTE_SETTLE: Duration = Duration::from_millis(250);

/// The longest selection the shortcut converts. Beyond this it is not a
/// mistyped phrase, and replacing it through the clipboard is a risk.
const MAX_SELECTION: usize = 4000;

/// What the keyboard is doing right now, for the settings window.
#[derive(Default)]
pub struct Runtime {
    /// The hook is installed.
    pub running: AtomicBool,
    /// It is an active filter, able to hold keys back.
    pub can_hold: AtomicBool,
    /// Corrections actually typed.
    pub applied: AtomicU64,
    /// The last application the user worked in, other than MarKey and the
    /// system shell — what "this app" means in the tray menu.
    pub last_app: Arc<Mutex<Option<String>>>,
    queue: SharedQueue,
}

impl Runtime {
    /// Carry out an action that did not come from a keystroke — the menu's
    /// undo, for instance.
    pub fn perform(&self, action: Action) {
        if matches!(action, Action::None) {
            return;
        }
        let (lock, wake) = &*self.queue;
        if let Ok(mut q) = lock.lock() {
            q.jobs.push_back(Job::Act(action, None));
            if !q.writing {
                q.writing = true;
                q.since = Some(Instant::now());
            }
        }
        wake.notify_one();
    }
}

enum Job {
    /// An action, and the key it held back to send afterwards.
    Act(Action, Option<RawKey>),
    /// A key the user pressed while a correction was being typed.
    Replay(RawKey),
    /// Caps Lock was left on; switch it off.
    CapsLockOff,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    /// The injector is busy; keys pressed now must wait their turn.
    writing: bool,
    since: Option<Instant>,
}

type SharedQueue = Arc<(Mutex<Queue>, Condvar)>;

/// Start the keyboard hook, waiting for Accessibility if it is not granted yet.
///
/// Waiting rather than failing matters: the alternative is telling the user to
/// quit and reopen the app after granting permission, which is the single most
/// confusing moment in installing a tool like this.
pub fn spawn(
    app: &AppHandle,
    engine: SharedEngine,
    on_config: ConfigSink,
) -> Result<Arc<Runtime>, rekey_hook::HookError> {
    let runtime = Arc::new(Runtime::default());
    let context: Arc<Mutex<Context>> = Arc::new(Mutex::new(Context::default()));
    spawn_context_sampler(app, context.clone(), runtime.last_app.clone());
    refresh_system_layouts(app);

    let queue: SharedQueue = runtime.queue.clone();
    spawn_injector(
        app,
        engine.clone(),
        context.clone(),
        queue.clone(),
        runtime.clone(),
        on_config,
    )?;

    let flag = runtime.clone();
    std::thread::Builder::new()
        .name("markey-hook".into())
        .spawn(move || {
            if !platform::has_permission() {
                log::info!("waiting for Accessibility permission…");
                while !platform::has_permission() {
                    std::thread::sleep(PERMISSION_POLL);
                }
                log::info!("Accessibility granted; starting the keyboard hook");
            }

            let can_hold = Arc::new(AtomicBool::new(false));
            let ready_engine = engine.clone();
            let ready_flag = flag.clone();
            let ready_hold = can_hold.clone();
            let on_ready = move |caps: Capabilities| {
                ready_flag.running.store(true, Ordering::SeqCst);
                ready_flag.can_hold.store(caps.can_hold, Ordering::SeqCst);
                ready_hold.store(caps.can_hold, Ordering::SeqCst);
                if let Ok(mut e) = ready_engine.lock() {
                    e.set_hold_boundaries(caps.can_hold);
                }
            };

            let handler =
                move |event: rekey_core::input::KeyEvent, raw: Option<RawKey>| -> Disposition {
                    // Our own typing passes untouched and unexamined.
                    if event.synthetic {
                        return Disposition::Pass;
                    }
                    let ctx = match context.lock() {
                        Ok(current) => current.clone(),
                        Err(poisoned) => poisoned.into_inner().clone(),
                    };
                    // MarKey's own windows are never corrected.
                    if ctx.app.as_deref().is_some_and(is_own_app) {
                        return Disposition::Pass;
                    }
                    let hold = can_hold.load(Ordering::Relaxed);
                    let (lock, wake) = &*queue;
                    let mut q = match lock.lock() {
                        Ok(q) => q,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    // Keys typed while a correction is going in wait behind it.
                    let busy = hold
                        && q.writing
                        && q.since.is_some_and(|t| t.elapsed() < MAX_HOLD)
                        && raw.is_some();

                    let (action, tapped, caps_lock) = {
                        let mut engine = match engine.lock() {
                            Ok(engine) => engine,
                            Err(poisoned) => {
                                // A panic in the engine must not silently stop
                                // corrections for the rest of the session.
                                log::error!("engine mutex was poisoned; recovering");
                                poisoned.into_inner()
                            }
                        };
                        let action = engine.on_key(&event, &ctx);
                        let caps = engine.take_caps_lock();
                        (action, engine.take_tap(), caps)
                    };

                    let swallow = hold && action.swallows_key();
                    let mut disposition = if tapped {
                        Disposition::Mask
                    } else {
                        Disposition::Pass
                    };
                    if busy && !swallow {
                        if let Some(raw) = raw.clone() {
                            q.jobs.push_back(Job::Replay(raw));
                            disposition = Disposition::Drop;
                        }
                    }
                    if !matches!(action, Action::None) {
                        // A held key goes back exactly as it was pressed —
                        // Option+Space, Shift+Return and all.
                        let held = if swallow {
                            disposition = Disposition::Drop;
                            raw
                        } else {
                            None
                        };
                        q.jobs.push_back(Job::Act(action, held));
                    }
                    if caps_lock {
                        q.jobs.push_back(Job::CapsLockOff);
                    }
                    if !q.jobs.is_empty() && !q.writing {
                        q.writing = true;
                        q.since = Some(Instant::now());
                    }
                    drop(q);
                    wake.notify_one();
                    disposition
                };

            let result = platform::run(on_ready, handler);
            flag.running.store(false, Ordering::SeqCst);
            if let Err(e) = result {
                log::error!("keyboard hook stopped: {e}");
            }
        })
        .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;

    Ok(runtime)
}

/// Poll the OS for the frontmost app, active layout and secure-input state,
/// on the main thread, and publish the result for the hook thread.
///
/// The hook thread must never sample this itself. macOS Text Input Services
/// and AppKit are main-thread-only: calling `TISGetInputSourceProperty` from
/// the event tap thread kills the process with SIGILL on the first keystroke.
fn spawn_context_sampler(
    app: &AppHandle,
    context: Arc<Mutex<Context>>,
    last_app: Arc<Mutex<Option<String>>>,
) {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("markey-context".into())
        .spawn(move || loop {
            let slot = context.clone();
            let last = last_app.clone();
            let posted = handle.run_on_main_thread(move || {
                let sampled = platform::current_context();
                if let Some(app) = sampled.app.as_deref().filter(|a| !is_shell(a)) {
                    if let Ok(mut last) = last.lock() {
                        if last.as_deref() != Some(app) {
                            *last = Some(app.to_string());
                        }
                    }
                }
                if let Ok(mut current) = slot.lock() {
                    *current = sampled;
                }
            });
            if posted.is_err() {
                break;
            }
            std::thread::sleep(CONTEXT_INTERVAL);
        })
        .expect("spawn context sampler");
}

/// Read the user's installed keyboard layouts from the system, so conversion
/// follows the exact variant they use.
pub fn refresh_system_layouts(app: &AppHandle) {
    let _ = app.run_on_main_thread(|| {
        for (id, lower, upper) in platform::system_layouts() {
            if rekey_core::layout::override_keys(&id, &lower, &upper) {
                log::info!("using the installed key table for layout {id}");
            }
        }
    });
}

/// MarKey's own windows: the settings window has a text box of its own.
fn is_own_app(app: &str) -> bool {
    app.eq_ignore_ascii_case("com.marmos.markey") || app.eq_ignore_ascii_case("markey.exe")
}

/// Apps that are never the one the user means by "this app": the shell's
/// taskbar takes focus when the tray icon is clicked on Windows.
fn is_shell(app: &str) -> bool {
    is_own_app(app)
        || [
            "explorer.exe",
            "shellexperiencehost.exe",
            "searchhost.exe",
            "startmenuexperiencehost.exe",
        ]
        .iter()
        .any(|s| app.eq_ignore_ascii_case(s))
}

fn spawn_injector(
    app: &AppHandle,
    engine: SharedEngine,
    context: Arc<Mutex<Context>>,
    queue: SharedQueue,
    runtime: Arc<Runtime>,
    on_config: ConfigSink,
) -> Result<(), rekey_hook::HookError> {
    let injector = new_injector()?;
    let handle = app.clone();
    std::thread::Builder::new()
        .name("markey-inject".into())
        .spawn(move || loop {
            let job = {
                let (lock, wake) = &*queue;
                let mut q = match lock.lock() {
                    Ok(q) => q,
                    Err(poisoned) => poisoned.into_inner(),
                };
                while q.jobs.is_empty() {
                    q.writing = false;
                    q.since = None;
                    q = match wake.wait(q) {
                        Ok(q) => q,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                }
                q.jobs.pop_front().expect("queue is not empty")
            };
            let config = engine
                .lock()
                .map(|e| e.config().clone())
                .unwrap_or_default();
            match job {
                Job::Replay(raw) => replay(&injector, &raw),
                Job::CapsLockOff => platform::caps_lock_off(),
                Job::Act(action @ Action::Replace { .. }, held) => {
                    std::thread::sleep(if action.swallows_key() {
                        HELD_REPLACE_DELAY
                    } else {
                        LATE_REPLACE_DELAY
                    });
                    apply(&handle, &injector, &action, held.as_ref(), &config);
                    runtime.applied.fetch_add(1, Ordering::Relaxed);
                }
                Job::Act(Action::ConvertSelection { select_word }, _) => {
                    let active = context.lock().ok().and_then(|c| c.layout.clone());
                    convert_selection(&handle, &injector, &engine, active, select_word, &config);
                }
                Job::Act(Action::None, _) => {}
            }
            // An undo may have taught the engine a new exception; keep it.
            let changed = engine
                .lock()
                .ok()
                .and_then(|mut e| e.take_config_dirty().then(|| e.config().clone()));
            if let Some(config) = changed {
                on_config(config);
            }
        })
        .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;
    Ok(())
}

/// Carry out one correction.
///
/// The keyboard is switched *before* the replacement is typed, not after.
/// The replacement is typed by replaying physical key presses, and a key press
/// only means the right character once the matching layout is active. It also
/// leaves the user on the right layout for the next word, which is the point
/// of switching at all — unless they asked for the keyboard to stay put.
fn apply(
    handle: &AppHandle,
    injector: &Injector,
    action: &Action,
    held: Option<&RawKey>,
    config: &Config,
) {
    let Action::Replace {
        delete,
        text,
        switch_to,
        then,
        ..
    } = action
    else {
        return;
    };
    log::info!(
        "applying: delete={delete} length={} switch_to={switch_to:?} held={}",
        text.chars().count(),
        then.is_some()
    );

    let original = if config.switch_layout {
        None
    } else {
        run_on_main(handle, platform::current_layout).flatten()
    };
    let selected = match switch_to {
        Some(layout) => select_layout_and_wait(handle, layout),
        None => None,
    };

    if *delete > 0 {
        injector.backspace(*delete);
    }
    // Replay the presses when the right layout is active; otherwise attach the
    // text to the events and hope the app honours it.
    let presses = selected
        .as_deref()
        .and_then(|layout| rekey_core::layout::presses_for(text, layout));
    let replayed = match presses {
        Some(presses) => injector.type_keys(&presses),
        None => false,
    };
    if !replayed && !text.is_empty() {
        injector.type_text(text);
    }
    // The key that ended the word, held back until now.
    match (held, then) {
        (Some(raw), _) => injector.replay(raw),
        (None, Some(boundary)) => {
            injector.boundary(boundary.key, boundary.shift);
        }
        (None, None) => {}
    }

    if let Some(original) = original {
        select_layout_and_wait(handle, &original);
    }
    if config.sound {
        let _ = handle.run_on_main_thread(play_sound);
    }
}

fn play_sound() {
    platform::play_sound();
}

fn replay(injector: &Injector, raw: &RawKey) {
    injector.replay(raw);
}

/// Convert the selected text, or the word before the caret, through the
/// clipboard — which is the one way to read and replace a selection that every
/// application supports. The user's clipboard is put back afterwards.
fn convert_selection(
    handle: &AppHandle,
    injector: &Injector,
    engine: &SharedEngine,
    active: Option<String>,
    select_word: bool,
    config: &Config,
) {
    // When the app can say, trust it: nothing selected means nothing to do.
    if !select_word && platform::focused_selection_length() == Some(0) {
        log::info!("shortcut: the app reports no selection");
        return;
    }
    let snapshot = platform::pasteboard_snapshot();
    let before = platform::pasteboard_change_count();
    if select_word {
        injector.select_word_left();
        std::thread::sleep(Duration::from_millis(30));
    }
    injector.copy();
    let started = Instant::now();
    while platform::pasteboard_change_count() == before && started.elapsed() < COPY_TIMEOUT {
        std::thread::sleep(Duration::from_millis(10));
    }
    let copied = (platform::pasteboard_change_count() != before)
        .then(platform::pasteboard_string)
        .flatten()
        .filter(|t| !t.trim().is_empty() && t.chars().count() <= MAX_SELECTION);
    let changed = platform::pasteboard_change_count() != before;
    let abort = |why: &str| {
        log::info!("shortcut: selection not converted ({why})");
        if select_word {
            injector.collapse_right();
        }
        // Put the clipboard back only if the copy touched it: rewriting it
        // for nothing would show up as a duplicate in clipboard managers.
        if changed {
            platform::pasteboard_restore(&snapshot);
        }
    };
    let Some(text) = copied else {
        abort("nothing was copied");
        return;
    };
    let converted = engine
        .lock()
        .ok()
        .and_then(|mut e| e.convert_text(&text, active.as_deref()));
    let Some((converted, target)) = converted else {
        abort("no other reading");
        return;
    };
    if config.switch_layout {
        select_layout_and_wait(handle, &target);
    }
    platform::pasteboard_set_string(&converted);
    injector.paste();
    std::thread::sleep(PASTE_SETTLE);
    platform::pasteboard_restore(&snapshot);
    log::info!(
        "shortcut: converted a selection of {} chars",
        converted.chars().count()
    );
}

/// Select `layout` on the main thread and confirm it took effect.
///
/// Returns the layout once it is actually active. Typing replayed key presses
/// against the wrong layout would produce the wrong characters, so this is
/// deliberately a confirmation rather than a request.
fn select_layout_and_wait(handle: &AppHandle, layout: &str) -> Option<String> {
    let requested = layout.to_string();
    if !run_on_main(handle, move || platform::select_layout(&requested))? {
        log::debug!("layout {layout} is not enabled; keyboard left alone");
        return None;
    }
    // Selection is asynchronous, so confirm it before replaying key presses
    // against it. The waiting happens here, on the injector thread — sleeping
    // on the main thread would stall the UI.
    for _ in 0..LAYOUT_SETTLE_STEPS {
        let wanted = layout.to_string();
        let active = run_on_main(handle, move || {
            platform::current_layout().as_deref() == Some(wanted.as_str())
        })?;
        if active {
            return Some(layout.to_string());
        }
        std::thread::sleep(LAYOUT_SETTLE_STEP);
    }
    log::warn!("layout {layout} did not become active; typing the text directly");
    None
}

/// Run `f` on the main thread and wait for its result.
fn run_on_main<T, F>(handle: &AppHandle, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    handle
        .run_on_main_thread(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    rx.recv_timeout(MAIN_THREAD_TIMEOUT).ok()
}

#[cfg(target_os = "macos")]
type Injector = rekey_hook::macos::MacInjector;
#[cfg(target_os = "windows")]
type Injector = rekey_hook::windows::WindowsInjector;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
type Injector = rekey_hook::noop::RecordingInjector;

#[cfg(target_os = "macos")]
fn new_injector() -> Result<Injector, rekey_hook::HookError> {
    rekey_hook::macos::MacInjector::new()
}

#[cfg(target_os = "windows")]
fn new_injector() -> Result<Injector, rekey_hook::HookError> {
    rekey_hook::windows::WindowsInjector::new()
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn new_injector() -> Result<Injector, rekey_hook::HookError> {
    Ok(rekey_hook::noop::RecordingInjector::new())
}
