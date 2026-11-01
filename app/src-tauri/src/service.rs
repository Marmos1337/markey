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
use rekey_core::engine::{convert_with, Action, Engine};
use rekey_core::input::{Context, KeyEvent, TextWriter};
use rekey_hook::{platform, Capabilities, Disposition, RawKey, TapHealth, TapHealthSnapshot};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, TryLockError};
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

/// How often the tap's health counters are read and logged.
const HEALTH_INTERVAL: Duration = Duration::from_secs(2);

/// This many disables within [`UNSTABLE_WINDOW`] and the settings window says
/// that the system keeps switching the keyboard filter off.
pub const UNSTABLE_DISABLES: usize = 3;
pub const UNSTABLE_WINDOW: Duration = Duration::from_secs(60);

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
    /// Timings and disables of the keyboard hook.
    pub health: Arc<TapHealth>,
    /// The keyboard layout active right now, for the menu bar badge.
    pub layout: Arc<Mutex<Option<String>>>,
    queue: SharedQueue,
}

impl Runtime {
    /// The system switched the keyboard filter off several times in the last
    /// minute. MarKey re-arms it each time, but the user should know.
    pub fn tap_unstable(&self) -> bool {
        self.health.disables_within(UNSTABLE_WINDOW) >= UNSTABLE_DISABLES
    }

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
    spawn_context_sampler(
        app,
        context.clone(),
        runtime.last_app.clone(),
        runtime.layout.clone(),
    );
    refresh_system_layouts(app);

    let queue: SharedQueue = runtime.queue.clone();
    spawn_health_monitor(runtime.health.clone());
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

            let mut router = KeyRouter::new(engine, context, queue, can_hold, flag.health.clone());
            let handler = move |event: KeyEvent, raw: Option<RawKey>| router.route(event, raw);

            let result = platform::run(on_ready, handler, flag.health.clone());
            flag.running.store(false, Ordering::SeqCst);
            if let Err(e) = result {
                log::error!("keyboard hook stopped: {e}");
            }
        })
        .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;

    Ok(runtime)
}

/// Decides what happens to each keystroke, inside the keyboard hook.
///
/// This runs on the hook's own thread with the OS waiting for the answer, so
/// it never waits for anything another thread may hold for long:
/// * the engine is taken with `try_lock`; if the injector or the settings
///   window has it, the key passes through untouched and the engine is told
///   to forget the word in progress, because it missed a keystroke of it;
/// * the frontmost-app context falls back to the last one seen;
/// * the queue is only ever held for a push or a pop, and never together
///   with the engine.
pub struct KeyRouter {
    engine: SharedEngine,
    context: Arc<Mutex<Context>>,
    queue: SharedQueue,
    can_hold: Arc<AtomicBool>,
    health: Arc<TapHealth>,
    last_context: Context,
    /// A keystroke went by without the engine; its idea of the word under
    /// the caret is stale.
    missed: bool,
}

impl KeyRouter {
    fn new(
        engine: SharedEngine,
        context: Arc<Mutex<Context>>,
        queue: SharedQueue,
        can_hold: Arc<AtomicBool>,
        health: Arc<TapHealth>,
    ) -> KeyRouter {
        KeyRouter {
            engine,
            context,
            queue,
            can_hold,
            health,
            last_context: Context::default(),
            missed: false,
        }
    }

    pub fn route(&mut self, event: KeyEvent, raw: Option<RawKey>) -> Disposition {
        // Our own typing passes untouched and unexamined.
        if event.synthetic {
            return Disposition::Pass;
        }
        match self.context.try_lock() {
            Ok(current) => self.last_context = current.clone(),
            Err(TryLockError::Poisoned(poisoned)) => {
                self.last_context = poisoned.into_inner().clone()
            }
            // The sampler is writing a fresh one this instant; the previous
            // sample is at most 40 ms old.
            Err(TryLockError::WouldBlock) => {}
        }
        // MarKey's own windows are never corrected.
        if self.last_context.app.as_deref().is_some_and(is_own_app) {
            return Disposition::Pass;
        }

        let engine = self.engine.clone();
        let (action, tapped, caps_lock) = match engine.try_lock() {
            Ok(mut engine) => self.decide(&mut engine, &event),
            Err(TryLockError::Poisoned(poisoned)) => {
                // A panic in the engine must not silently stop corrections
                // for the rest of the session.
                log::error!("engine mutex was poisoned; recovering");
                self.decide(&mut poisoned.into_inner(), &event)
            }
            Err(TryLockError::WouldBlock) => {
                self.missed = true;
                self.health.record_skipped();
                (Action::None, false, false)
            }
        };

        let hold = self.can_hold.load(Ordering::Relaxed);
        let swallow = hold && action.swallows_key();
        let mut disposition = if tapped {
            Disposition::Mask
        } else {
            Disposition::Pass
        };
        let (lock, wake) = &*self.queue;
        let mut q = match lock.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        // Keys typed while a correction is going in wait behind it.
        let busy =
            hold && q.writing && q.since.is_some_and(|t| t.elapsed() < MAX_HOLD) && raw.is_some();
        if busy && !swallow {
            if let Some(raw) = raw.clone() {
                q.jobs.push_back(Job::Replay(raw));
                disposition = Disposition::Drop;
            }
        }
        if !matches!(action, Action::None) {
            // A held key goes back exactly as it was pressed — Option+Space,
            // Shift+Return and all.
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
    }

    fn decide(&mut self, engine: &mut Engine, event: &KeyEvent) -> (Action, bool, bool) {
        if std::mem::take(&mut self.missed) {
            engine.invalidate_caret();
        }
        let action = engine.on_key(event, &self.last_context);
        let caps = engine.take_caps_lock();
        (action, engine.take_tap(), caps)
    }
}

/// Read the hook's health counters off the keyboard path and log what changed:
/// disables with their kind and count, slow callbacks with their duration.
fn spawn_health_monitor(health: Arc<TapHealth>) {
    std::thread::Builder::new()
        .name("markey-tap-health".into())
        .spawn(move || {
            let mut seen = TapHealthSnapshot::default();
            loop {
                std::thread::sleep(HEALTH_INTERVAL);
                let now = health.snapshot();
                for line in health_report(&seen, &now, health.disables_within(UNSTABLE_WINDOW)) {
                    log::warn!("{line}");
                }
                seen = now;
            }
        })
        .ok();
}

/// What to log about the hook between two readings of its counters.
fn health_report(
    before: &TapHealthSnapshot,
    now: &TapHealthSnapshot,
    last_minute: usize,
) -> Vec<String> {
    let mut lines = Vec::new();
    let ms = |micros: u64| micros as f64 / 1000.0;
    for (kind, was, is) in [
        ("timeout", before.disabled_timeout, now.disabled_timeout),
        ("user input", before.disabled_user, now.disabled_user),
    ] {
        if is > was {
            lines.push(format!(
                "keyboard tap was disabled by the system ({kind}) {}x; re-armed. \
                 This session: {} by timeout, {} by user input, {last_minute} in the last minute; \
                 last slow callback {:.1} ms, slowest {:.1} ms",
                is - was,
                now.disabled_timeout,
                now.disabled_user,
                ms(now.last_slow_micros),
                ms(now.slowest_micros),
            ));
        }
    }
    if now.slow > before.slow {
        lines.push(format!(
            "{} keyboard callback(s) took over {} ms (last {:.1} ms, slowest {:.1} ms, {} events so far)",
            now.slow - before.slow,
            rekey_hook::SLOW_CALLBACK.as_millis(),
            ms(now.last_slow_micros),
            ms(now.slowest_micros),
            now.events,
        ));
    }
    if now.skipped_busy > before.skipped_busy {
        lines.push(format!(
            "{} keystroke(s) passed through while the engine was busy",
            now.skipped_busy - before.skipped_busy
        ));
    }
    lines
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
    layout: Arc<Mutex<Option<String>>>,
) {
    let handle = app.clone();
    std::thread::Builder::new()
        .name("markey-context".into())
        .spawn(move || loop {
            let slot = context.clone();
            let last = last_app.clone();
            let current_layout = layout.clone();
            let tray = handle.clone();
            let posted = handle.run_on_main_thread(move || {
                let sampled = platform::current_context();
                // The menu bar badge follows the layout.
                let changed = current_layout
                    .lock()
                    .map(|mut l| {
                        let changed = *l != sampled.layout;
                        if changed {
                            l.clone_from(&sampled.layout);
                        }
                        changed
                    })
                    .unwrap_or(false);
                if changed {
                    crate::refresh_tray_icon(&tray);
                }
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
            // Browsers and web chats drop keys sent too close together.
            let app = context.lock().ok().and_then(|c| c.app.clone());
            injector.set_keystroke_gap(rekey_core::apps::keystroke_gap(app.as_deref()));
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
                Job::Act(Action::ConvertSelection { select_word, to }, _) => {
                    let active = context.lock().ok().and_then(|c| c.layout.clone());
                    convert_selection(
                        &handle,
                        &injector,
                        &engine,
                        active,
                        select_word,
                        to.as_deref(),
                        &config,
                    );
                }
                Job::Act(Action::SwitchLayout { to }, _) => {
                    select_layout_and_wait(&handle, &to);
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
    to: Option<&str>,
    config: &Config,
) {
    // When the app can say, trust it: nothing selected means nothing to do.
    if !select_word && platform::focused_selection_length() == Some(0) {
        log::info!("shortcut: the app reports no selection");
        return;
    }
    let Ok(snapshot) = platform::pasteboard_snapshot() else {
        log::info!(
            "shortcut: the clipboard holds over {} MB; selection left alone",
            rekey_hook::MAX_PASTEBOARD_SNAPSHOT / (1024 * 1024)
        );
        return;
    };
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
    // Converted on a copy of the detector: the keyboard hook needs the engine
    // on every keystroke, and a long selection takes a while to score.
    let detector = engine.lock().ok().map(|e| e.detector_snapshot());
    let converted = detector.and_then(|d| convert_with(&d, &text, active.as_deref(), to));
    let Some((converted, target)) = converted else {
        abort("no other reading");
        return;
    };
    if let Ok(mut e) = engine.lock() {
        e.note_converted(&target);
    }
    // The force-layout hotkeys always leave the keyboard on their layout.
    if config.switch_layout || to.is_some() {
        select_layout_and_wait(handle, &target);
    }
    platform::pasteboard_set_string(&converted);
    let ours = platform::pasteboard_change_count();
    injector.paste();
    std::thread::sleep(PASTE_SETTLE);
    // Put the user's clipboard back — unless something else wrote to it in
    // the meantime (the user copied again, or a clipboard manager did): then
    // that newer content is what they expect to find.
    if should_restore_clipboard(ours, platform::pasteboard_change_count()) {
        platform::pasteboard_restore(&snapshot);
    } else {
        log::info!("shortcut: the clipboard changed during the paste; left as it is");
    }
    log::info!(
        "shortcut: converted a selection of {} chars",
        converted.chars().count()
    );
}

/// Whether the clipboard still holds what MarKey put there for the paste.
fn should_restore_clipboard(ours: isize, now: isize) -> bool {
    ours == now
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

#[cfg(test)]
mod tests {
    use super::*;
    use rekey_core::input::{Key, Modifiers};
    use rekey_core::model::Models;

    fn router(engine: SharedEngine) -> (KeyRouter, Arc<TapHealth>) {
        let health = Arc::new(TapHealth::new());
        let context = Arc::new(Mutex::new(Context {
            app: Some("com.apple.TextEdit".into()),
            layout: Some("us".into()),
            secure_input: false,
        }));
        let router = KeyRouter::new(
            engine,
            context,
            Arc::new((Mutex::new(Queue::default()), Condvar::new())),
            Arc::new(AtomicBool::new(true)),
            health.clone(),
        );
        (router, health)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            character: Some(c),
            key: if c == ' ' { Key::Space } else { Key::Character },
            modifiers: Modifiers::default(),
            synthetic: false,
        }
    }

    fn raw(c: char) -> Option<RawKey> {
        Some(RawKey {
            code: 0,
            flags: 0,
            text: c.to_string(),
        })
    }

    #[test]
    fn a_busy_engine_never_blocks_the_keyboard() {
        let engine: SharedEngine =
            Arc::new(Mutex::new(Engine::new(Models::new(), Config::default())));
        let (mut router, health) = router(engine.clone());
        // Another thread holds the engine, as the injector or the settings
        // window may.
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let holder = engine.clone();
        let t = std::thread::spawn(move || {
            let _guard = holder.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        locked_rx.recv().unwrap();
        let started = Instant::now();
        let disposition = router.route(key('a'), raw('a'));
        let took = started.elapsed();
        assert_eq!(disposition, Disposition::Pass);
        assert!(
            took < Duration::from_millis(5),
            "routing took {took:?} with the engine held"
        );
        assert_eq!(health.snapshot().skipped_busy, 1);
        assert!(router.missed, "the engine must learn it missed a key");
        release_tx.send(()).unwrap();
        t.join().unwrap();
        // Next key: the engine is free again and the stale word is dropped.
        router.route(key('b'), raw('b'));
        assert!(!router.missed);
        assert_eq!(health.snapshot().skipped_busy, 1);
    }

    #[test]
    fn the_queue_and_the_engine_are_never_held_together() {
        // The injector takes the queue, then the engine, in separate steps.
        // If routing nested them the other way round, this would deadlock.
        let engine: SharedEngine =
            Arc::new(Mutex::new(Engine::new(Models::new(), Config::default())));
        let (mut router, _) = router(engine.clone());
        let queue = router.queue.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = std::thread::spawn(move || {
            while !worker_stop.load(Ordering::Relaxed) {
                let q = queue.0.lock().unwrap();
                let _e = engine.lock().unwrap();
                drop(q);
            }
        });
        for i in 0..20_000 {
            router.route(key(if i % 7 == 0 { ' ' } else { 'a' }), raw('a'));
        }
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }

    /// Times the hook path with the real language models, as a regression
    /// guard for the "keyboard tap was disabled by the system" bug. Skipped
    /// when the models have not been built.
    #[test]
    fn routing_with_real_models_stays_under_the_slow_threshold() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rekey-core/data/models");
        let Ok(models) = Models::load_dir(&dir) else {
            return;
        };
        if models.is_empty() {
            return;
        }
        let config = Config {
            layouts: vec!["us".into(), "ru".into()],
            ..Config::default()
        };
        let engine: SharedEngine = Arc::new(Mutex::new(Engine::new(models, config)));
        let (mut router, _) = router(engine);
        let text = "ghbdtn rfr ltkf z nen hello world xnj nfrjt vs gjlevfkb ";
        let mut times: Vec<Duration> = Vec::new();
        for c in text.chars().cycle().take(5_000) {
            let started = Instant::now();
            router.route(key(c), raw(c));
            times.push(started.elapsed());
        }
        times.sort();
        let p99 = times[times.len() * 99 / 100];
        let max = *times.last().unwrap();
        eprintln!("routing 5000 keys: p99 {p99:?}, max {max:?}");
        assert!(p99 < rekey_hook::SLOW_CALLBACK, "p99 {p99:?}");
    }

    #[test]
    fn the_health_report_names_the_kind_count_and_timing() {
        let before = TapHealthSnapshot::default();
        let now = TapHealthSnapshot {
            events: 900,
            slow: 2,
            slowest_micros: 7_400,
            last_slow_micros: 5_100,
            skipped_busy: 1,
            disabled_timeout: 1,
            disabled_user: 0,
        };
        let lines = health_report(&before, &now, 1);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].contains("(timeout) 1x"));
        assert!(lines[0].contains("slowest 7.4 ms"));
        assert!(lines[1].contains("2 keyboard callback(s) took over 5 ms"));
        assert!(health_report(&now, &now, 0).is_empty());
    }

    #[test]
    fn the_clipboard_is_restored_only_if_it_still_holds_our_text() {
        assert!(should_restore_clipboard(41, 41));
        assert!(!should_restore_clipboard(41, 42));
    }
}
