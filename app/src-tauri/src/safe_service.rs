//! Local safety fork: observed AX targets only; no global synthetic-key fallback.
use crate::transaction::{self, Outcome, Phase, Target};
use rekey_core::{
    engine::{Action, Engine},
    input::{Context, Key, KeyEvent},
};
use rekey_hook::{
    focus::{Expected, Focus, Range, Watch},
    platform,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use tauri::AppHandle;
pub type SharedEngine = Arc<Mutex<Engine>>;
// Temporary privacy-preserving runtime diagnostics: aggregate counts and
// source reason codes only. No characters, words, app names or field values.
static INPUT_COUNT: AtomicU64 = AtomicU64::new(0);
static CHARACTER_COUNT: AtomicU64 = AtomicU64::new(0);
static OTHER_COUNT: AtomicU64 = AtomicU64::new(0);
static EMPTY_CHARACTER_COUNT: AtomicU64 = AtomicU64::new(0);
static CORE_COUNT: AtomicU64 = AtomicU64::new(0);
static APPLIED_COUNT: AtomicU64 = AtomicU64::new(0);
static ACTION_COUNT: AtomicU64 = AtomicU64::new(0);
// Runtime state is metadata only; it never retains text or field contents.
static GENERATION: OnceLock<Arc<AtomicU64>> = OnceLock::new();
static INPUT_SEQUENCE: OnceLock<Arc<AtomicU64>> = OnceLock::new();
static FOCUS_EPOCH: OnceLock<Arc<AtomicU64>> = OnceLock::new();
static PROPOSAL: AtomicU64 = AtomicU64::new(0);
static WRITING: AtomicBool = AtomicBool::new(false);
static CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);
pub fn set_capture_enabled(enabled: bool) {
    CAPTURE_ENABLED.store(enabled, Ordering::SeqCst);
}
struct WriteScope;
impl Drop for WriteScope {
    fn drop(&mut self) {
        WRITING.store(false, Ordering::SeqCst);
    }
}
fn cancel_proposal(engine: &SharedEngine, sequence: u64) {
    if let Ok(mut e) = engine.lock() {
        if PROPOSAL
            .compare_exchange(sequence, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            e.cancel_proposal();
        }
    }
}
static RUNTIME_STATUS: OnceLock<Mutex<crate::runtime_state::Status>> = OnceLock::new();
fn live_token() -> crate::runtime_state::Token {
    crate::runtime_state::Token {
        generation: GENERATION
            .get()
            .map(|g| g.load(Ordering::SeqCst))
            .unwrap_or(0),
        revision: SETTINGS_REVISION.load(Ordering::SeqCst),
    }
}
fn publish_status(code: u64, source: crate::runtime_state::Token) {
    if let Ok(mut status) = RUNTIME_STATUS
        .get_or_init(|| Mutex::new(Default::default()))
        .lock()
    {
        crate::runtime_state::publish(&mut status, source, live_token(), code);
    }
}
static LAST_CORRECTION: AtomicU64 = AtomicU64::new(0);
pub fn runtime_status(enabled: bool, permission: bool, running: bool) -> &'static str {
    if !enabled {
        return "paused";
    }
    if !permission {
        return "permission";
    }
    if !running {
        return "hook_stopped";
    }
    if platform::secure_input_active() {
        return "secure_input";
    }
    let code = RUNTIME_STATUS
        .get()
        .and_then(|s| {
            s.lock()
                .ok()
                .map(|s| if s.token == live_token() { s.code } else { 3 })
        })
        .unwrap_or(0);
    match code {
        1 => "excluded",
        2 => "unsupported_field",
        3 => "resync",
        4 => "ready",
        5 => "cancelled",
        _ => "waiting",
    }
}
pub fn diagnostic_counts() -> [u64; 4] {
    [
        INPUT_COUNT.load(Ordering::Relaxed),
        CORE_COUNT.load(Ordering::Relaxed),
        ACTION_COUNT.load(Ordering::Relaxed),
        APPLIED_COUNT.load(Ordering::Relaxed),
    ]
}
static SETTINGS_REVISION: AtomicU64 = AtomicU64::new(0);
static WATCHES: OnceLock<Mutex<Vec<Watch>>> = OnceLock::new();
pub fn invalidate_pending() {
    SETTINGS_REVISION.fetch_add(1, Ordering::SeqCst);
    publish_status(0, live_token());
    if let Some(ws) = WATCHES.get() {
        if let Ok(ws) = ws.lock() {
            for w in ws.iter() {
                w.clear();
                w.invalidated.store(true, Ordering::SeqCst);
            }
        }
    }
}
// A timed-out queued main-thread task is marked cancelled before it can mutate layout.
fn on_main<T: Send + 'static>(
    app: &AppHandle,
    f: impl FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    let live = Arc::new(AtomicBool::new(true));
    let active = live.clone();
    app.run_on_main_thread(move || {
        if active.load(Ordering::SeqCst) {
            let result = f(active.clone());
            let _ = tx.send(result);
        }
    })
    .ok()?;
    let result = rx.recv_timeout(Duration::from_millis(100)).ok();
    if result.is_none() {
        live.store(false, Ordering::SeqCst);
    }
    result
}
fn policy_context(app: &AppHandle) -> Option<(Context, i32)> {
    on_main(app, |_| platform::current_context_with_pid()).flatten()
}
fn allowed(engine: &SharedEngine, ctx: &Context) -> bool {
    let Ok(e) = engine.lock() else {
        return false;
    };
    e.config().enabled
        && !ctx.secure_input
        && ctx
            .app
            .as_ref()
            .is_some_and(|a| !e.config().is_excluded_app(a))
}
#[derive(Clone)]
struct Track {
    focus: Focus,
    tail: String,
    revision: u64,
    synchronized: bool,
    epoch: u64,
    layout: Option<String>,
    recovery: Vec<usize>,
}
fn expectation(t: &Track) -> Option<Expected> {
    let length = t.tail.encode_utf16().count();
    if length > 128 || t.focus.caret < length {
        return None;
    }
    Some(Expected {
        boundary_start: t.focus.caret - length,
        focus: t.focus.clone(),
        fragment_range: Range {
            location: (t.focus.caret - length) as isize,
            length: length as isize,
        },
        fragment: t.tail.clone(),
        revision: t.revision,
    })
}
fn state(t: &Track) -> crate::input_phase::State {
    crate::input_phase::State {
        caret: t.focus.caret,
        tail: t.tail.clone(),
    }
}
fn matches(t: &Track, focus: &Focus) -> bool {
    expectation(t)
        .is_some_and(|x| x.matches(focus, |r| SETTINGS_REVISION.load(Ordering::SeqCst) == r))
}
fn boundary_at(focus: &Focus, caret: usize, revision: u64) -> bool {
    if SETTINGS_REVISION.load(Ordering::SeqCst) != revision {
        return false;
    }
    let predecessor = if caret > 0 {
        focus.fragment(Range {
            location: (caret - 1) as isize,
            length: 1,
        })
    } else {
        None
    };
    crate::input_phase::known_boundary(caret, predecessor.as_deref())
}
#[track_caller]
fn reset(engine: &SharedEngine, tracking: &Arc<Mutex<Option<Track>>>, watch: &Watch) {
    log::debug!(
        "AX diagnostic reset code=L{} input_count={} core_count={} action_count={}",
        std::panic::Location::caller().line(),
        INPUT_COUNT.load(Ordering::Relaxed),
        CORE_COUNT.load(Ordering::Relaxed),
        ACTION_COUNT.load(Ordering::Relaxed)
    );
    publish_status(3, live_token());
    if let Ok(mut e) = engine.lock() {
        e.invalidate_caret();
    }
    if let Ok(mut t) = tracking.lock() {
        *t = None;
    }
    watch.clear();
}
/// An old queued correction cannot invalidate a newer word/field.
fn reset_owned(
    engine: &SharedEngine,
    tracking: &Arc<Mutex<Option<Track>>>,
    watch: &Watch,
    source: crate::runtime_state::Token,
) {
    if let Ok(mut e) = engine.lock() {
        if !crate::runtime_state::owns(source, live_token()) {
            return;
        }
        e.invalidate_caret();
    }
    if let Ok(mut t) = tracking.lock() {
        if !crate::runtime_state::owns(source, live_token()) {
            return;
        }
        *t = None;
    }
    watch.expect_owned(source.generation, source.revision, Vec::new());
    publish_status(3, source);
}
struct Pending {
    action: Action,
    origin: Focus,
    generation: u64,
    revision: u64,
    epoch: u64,
    layout: Option<String>,
    sequence: u64,
}
struct AxTarget<'a> {
    pending: &'a Pending,
    range: Range,
    text: &'a str,
    original: &'a str,
    app: &'a AppHandle,
    engine: &'a SharedEngine,
    generation: &'a AtomicU64,
    running: &'a AtomicBool,
    source_verified: bool,
    selected_owned: bool,
}
impl Target for AxTarget<'_> {
    fn guard(&mut self, phase: Phase) -> bool {
        // Cleanup retains narrow authorization to restore our already-selected fragment.
        // A settings change cancels writing but alone does not transfer caret ownership.
        let owner = self.running.load(Ordering::SeqCst)
            && self.generation.load(Ordering::SeqCst) == self.pending.generation
            && !platform::secure_input_active();
        let owner = owner
            && FOCUS_EPOCH
                .get()
                .is_some_and(|e| e.load(Ordering::SeqCst) == self.pending.epoch);
        let owner = owner
            && INPUT_SEQUENCE
                .get()
                .is_some_and(|s| s.load(Ordering::SeqCst) == self.pending.sequence);
        if !owner {
            return false;
        }
        if phase != Phase::Rollback
            && SETTINGS_REVISION.load(Ordering::SeqCst) != self.pending.revision
        {
            return false;
        }
        if phase == Phase::BeforeSelection {
            let Some((ctx, pid)) = policy_context(self.app) else {
                return false;
            };
            if !allowed(self.engine, &ctx) || pid != self.pending.origin.pid {
                return false;
            }
            let Some(now) = Focus::capture() else {
                return false;
            };
            now.same_field(&self.pending.origin)
                && now.selected == 0
                && now.caret == self.pending.origin.caret
                && now.boundary(self.range.location as usize)
                && self.generation.load(Ordering::SeqCst) == self.pending.generation
                && SETTINGS_REVISION.load(Ordering::SeqCst) == self.pending.revision
        } else {
            // No main-thread wait, TIS/AppKit, engine mutex, or full role/window capture after selection.
            self.pending.origin.still_focused()
                && self.pending.origin.current_range() == Some(self.range)
                && self.generation.load(Ordering::SeqCst) == self.pending.generation
                && (phase == Phase::Rollback
                    || SETTINGS_REVISION.load(Ordering::SeqCst) == self.pending.revision)
        }
    }
    fn fragment(&mut self) -> Option<String> {
        if SETTINGS_REVISION.load(Ordering::SeqCst) != self.pending.revision {
            return None;
        }
        if !self.pending.origin.boundary(self.range.location as usize)
            || SETTINGS_REVISION.load(Ordering::SeqCst) != self.pending.revision
        {
            return None;
        }
        let source = self.pending.origin.fragment(self.range);
        self.source_verified = source.as_deref() == Some(self.original);
        source
    }
    fn select(&mut self) -> bool {
        let ok =
            self.guard(Phase::BeforeSelection) && self.pending.origin.set_selection(self.range);
        self.selected_owned = ok;
        ok
    }
    fn replace(&mut self) -> bool {
        self.guard(Phase::BeforeReplacement)
            && self.fragment().as_deref() == Some(self.original)
            && self.guard(Phase::BeforeReplacement)
            && self.pending.origin.set_selected_text(self.text)
    }
    fn rollback(&mut self) -> bool {
        if !self.selected_owned || !self.source_verified || !self.guard(Phase::Rollback) {
            return false;
        }
        if SETTINGS_REVISION.load(Ordering::SeqCst) == self.pending.revision {
            if self.fragment().as_deref() != Some(self.original) || !self.guard(Phase::Rollback) {
                return false;
            }
        }
        // A config-only cancellation may collapse our exact owned range using metadata
        // and the previously verified source, without any new text read while OFF.
        // This never writes/deletes characters; focus/input/range changes prevent it.
        self.guard(Phase::Rollback)
            && self.pending.origin.set_selection(Range {
                location: self.range.location + self.range.length,
                length: 0,
            })
    }
}
pub fn spawn(
    app: &AppHandle,
    engine: SharedEngine,
) -> Result<Arc<AtomicBool>, rekey_hook::HookError> {
    let running = Arc::new(AtomicBool::new(false));
    set_capture_enabled(engine.lock().ok().is_some_and(|e| e.config().enabled));
    let generation = Arc::new(AtomicU64::new(0));
    let _ = GENERATION.set(generation.clone());
    let input_sequence = Arc::new(AtomicU64::new(0));
    let _ = INPUT_SEQUENCE.set(input_sequence.clone());
    let epoch = Arc::new(AtomicU64::new(0));
    let _ = FOCUS_EPOCH.set(epoch.clone());
    let tracking = Arc::new(Mutex::new(None::<Track>));
    let watch = Watch::start(
        generation.clone(),
        epoch.clone(),
        Arc::new(|revision| SETTINGS_REVISION.load(Ordering::SeqCst) == revision),
    )
    .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;
    if let Ok(mut ws) = WATCHES.get_or_init(|| Mutex::new(Vec::new())).lock() {
        ws.push(watch.clone());
    }
    let (tx, rx) = mpsc::channel::<Pending>();
    let handle = app.clone();
    let gen = generation.clone();
    let e = engine.clone();
    let tracker = tracking.clone();
    let w = watch.clone();
    let alive = running.clone();
    std::thread::Builder::new()
        .name("rekey-safe-inject".into())
        .spawn(move || {
            for pending in rx {
                let owner = crate::runtime_state::Token {
                    generation: pending.generation,
                    revision: pending.revision,
                };
                if !crate::runtime_state::owns(owner, live_token()) {
                    cancel_proposal(&e, pending.generation);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(25));
                if !crate::runtime_state::owns(owner, live_token()) {
                    cancel_proposal(&e, pending.generation);
                    continue;
                }
                WRITING.store(true, Ordering::SeqCst);
                let _write_scope = WriteScope;
                let Action::Replace {
                    delete,
                    text,
                    switch_to,
                    original,
                } = &pending.action
                else {
                    continue;
                };
                let count = original.encode_utf16().count();
                // The core deletion count must agree with this exact BMP source fragment.
                if count != *delete
                    || pending.origin.caret < count
                    || count > 128
                    || !w.is_ready(&pending.origin)
                {
                    reset_owned(&e, &tracker, &w, owner);
                    continue;
                }
                let range = Range {
                    location: (pending.origin.caret - count) as isize,
                    length: count as isize,
                };
                let selected = pending.origin.with_range(range);
                let final_focus = pending.origin.with_range(Range {
                    location: range.location + text.encode_utf16().count() as isize,
                    length: 0,
                });
                // Allow only the exact original/own-selection/own-replacement transitions.
                if !w.expect_owned(
                    pending.generation,
                    pending.revision,
                    vec![
                        Expected {
                            boundary_start: range.location as usize,
                            focus: pending.origin.clone(),
                            fragment_range: range,
                            fragment: original.clone(),
                            revision: pending.revision,
                        },
                        Expected {
                            boundary_start: range.location as usize,
                            focus: selected,
                            fragment_range: range,
                            fragment: original.clone(),
                            revision: pending.revision,
                        },
                        Expected {
                            boundary_start: range.location as usize,
                            focus: final_focus.clone(),
                            fragment_range: Range {
                                location: range.location,
                                length: text.encode_utf16().count() as isize,
                            },
                            fragment: text.clone(),
                            revision: pending.revision,
                        },
                    ],
                ) {
                    continue;
                }
                let mut target = AxTarget {
                    pending: &pending,
                    range,
                    text,
                    original,
                    app: &handle,
                    engine: &e,
                    generation: &gen,
                    running: &alive,
                    source_verified: false,
                    selected_owned: false,
                };
                let outcome = transaction::execute(&mut target, original);
                if outcome != Outcome::Replaced {
                    if outcome == Outcome::SelectionNotRestored {
                        log::warn!(
                            "AX correction cancelled; selection restoration was unsafe and skipped"
                        );
                    }
                    reset_owned(&e, &tracker, &w, owner);
                    publish_status(5, owner);
                    continue;
                }
                APPLIED_COUNT.fetch_add(1, Ordering::Relaxed);
                let _ = PROPOSAL.compare_exchange(
                    pending.generation,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                );
                log::info!(
                    "AX replacement committed ({} chars); input-source confirmation follows",
                    text.chars().count()
                );
                let Some(after) = Focus::capture() else {
                    reset_owned(&e, &tracker, &w, owner);
                    continue;
                };
                if !after.same_field(&final_focus)
                    || after.selection() != final_focus.selection()
                    || gen.load(Ordering::SeqCst) != pending.generation
                    || SETTINGS_REVISION.load(Ordering::SeqCst) != pending.revision
                {
                    reset_owned(&e, &tracker, &w, owner);
                    continue;
                }
                let mut layout_ok = switch_to.is_none();
                if let Some(layout) = switch_to {
                    let target_focus = after.clone();
                    let target_text = text.clone();
                    let start = range.location;
                    let layout = layout.clone();
                    let e = e.clone();
                    let gen = gen.clone();
                    let alive = alive.clone();
                    let n = pending.generation;
                    let revision = pending.revision;
                    layout_ok = on_main(&handle, move |token| {
                        let valid = || {
                            token.load(Ordering::SeqCst)
                                && alive.load(Ordering::SeqCst)
                                && gen.load(Ordering::SeqCst) == n
                                && SETTINGS_REVISION.load(Ordering::SeqCst) == revision
                        };
                        if !valid() || !allowed(&e, &platform::current_context()) {
                            return false;
                        }
                        if !Focus::capture().is_some_and(|f| {
                            f.same_field(&target_focus)
                                && f.selection() == target_focus.selection()
                                && f.boundary(start as usize)
                                && f.fragment(Range {
                                    location: start,
                                    length: target_text.encode_utf16().count() as isize,
                                })
                                .as_deref()
                                    == Some(target_text.as_str())
                        }) || !valid()
                        {
                            return false;
                        }
                        platform::select_layout(&layout)
                            && valid()
                            && platform::current_layout().as_deref() == Some(layout.as_str())
                    })
                    .unwrap_or(false);
                }
                if !layout_ok {
                    if let Ok(mut engine) = e.lock() {
                        if crate::runtime_state::owns(owner, live_token()) {
                            engine.invalidate_caret();
                        }
                    }
                    log::info!("text corrected; input-source change was not confirmed");
                }
                if gen.load(Ordering::SeqCst) == pending.generation
                    && SETTINGS_REVISION.load(Ordering::SeqCst) == pending.revision
                {
                    let t = Track {
                        focus: after,
                        tail: text.clone(),
                        revision: pending.revision,
                        synchronized: true,
                        epoch: pending.epoch,
                        layout: switch_to.clone().or(pending.layout.clone()),
                        recovery: Vec::new(),
                    };
                    if let Some(x) = expectation(&t) {
                        if !w.expect_owned(pending.generation, pending.revision, vec![x]) {
                            continue;
                        }
                        if let Ok(mut slot) = tracker.lock() {
                            if crate::runtime_state::owns(owner, live_token()) {
                                *slot = Some(t);
                            }
                        }
                    }
                } else {
                    reset_owned(&e, &tracker, &w, owner);
                }
                LAST_CORRECTION.store(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                    Ordering::Relaxed,
                );
                publish_status(4, owner);
                log::info!(
                    "AX correction applied ({} chars); compatible observed fields only",
                    text.chars().count()
                );
            }
        })
        .map_err(|err| rekey_hook::HookError::Os(err.to_string()))?;
    use crate::queued_input::{EventQueue, Input};
    let (input_tx, input_rx) = mpsc::sync_channel::<Input>(64);
    let handle = app.clone();
    let reconcile_engine = engine.clone();
    let reconcile_generation = generation.clone();
    let reconcile_sequence = input_sequence.clone();
    let reconcile_epoch = epoch.clone();
    let reconcile_track = tracking.clone();
    let reconcile_watch = watch.clone();
    let reconcile_running = running.clone();
    std::thread::Builder::new()
        .name("rekey-ax-reconcile".into())
        .spawn(move || {
            use crate::queued_input::Resolution;
            let mut lineage = (
                reconcile_epoch.load(Ordering::SeqCst),
                SETTINGS_REVISION.load(Ordering::SeqCst),
            );
            let mut queue = EventQueue::new(lineage);
            let mut dirty_seen = 0;
            loop {
                let received = input_rx.recv_timeout(Duration::from_millis(8));
                if matches!(received, Err(mpsc::RecvTimeoutError::Disconnected)) {
                    break;
                }
                let live = (
                    reconcile_epoch.load(Ordering::SeqCst),
                    SETTINGS_REVISION.load(Ordering::SeqCst),
                );
                if live != lineage {
                    queue.reset(live);
                    lineage = live;
                    reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                }
                let mut incoming = received
                    .ok()
                    .into_iter()
                    .chain(input_rx.try_iter())
                    .collect::<Vec<_>>();
                let mut gap = false;
                for item in incoming.drain(..) {
                    if queue.push(item).is_err() {
                        gap = true;
                        break;
                    }
                }
                if gap || queue.len() > 64 {
                    reconcile_epoch.fetch_add(1, Ordering::SeqCst);
                    reconcile_generation.fetch_add(1, Ordering::SeqCst);
                    queue.reset(lineage);
                    reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                    log::debug!("AX diagnostic queue lineage lost");
                    continue;
                }
                let dirty = reconcile_watch.dirty.load(Ordering::SeqCst);
                if queue.is_empty() && dirty == dirty_seen {
                    continue;
                }
                if WRITING.load(Ordering::SeqCst) {
                    continue;
                }
                let Some((ctx, pid)) = policy_context(&handle) else {
                    continue;
                };
                if !allowed(&reconcile_engine, &ctx) || platform::secure_input_active() {
                    queue.clear();
                    reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                    dirty_seen = dirty;
                    let excluded = reconcile_engine.lock().ok().is_some_and(|e| {
                        ctx.app
                            .as_ref()
                            .is_some_and(|a| e.config().is_excluded_app(a))
                    });
                    if excluded {
                        publish_status(1, live_token());
                    }
                    continue;
                }
                let watermark = reconcile_generation.load(Ordering::SeqCst);
                let Some(focus) = Focus::capture() else {
                    queue.clear();
                    reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                    dirty_seen = dirty;
                    publish_status(2, live_token());
                    continue;
                };
                if focus.pid != pid || !reconcile_watch.ensure(&focus) {
                    continue;
                }
                if lineage
                    != (
                        reconcile_epoch.load(Ordering::SeqCst),
                        SETTINGS_REVISION.load(Ordering::SeqCst),
                    )
                {
                    continue;
                }
                // AX IPC may have allowed several keys to arrive. Incorporate every
                // already-enqueued event before calling the snapshot unknown input.
                for item in input_rx.try_iter() {
                    if queue.push(item).is_err() {
                        gap = true;
                        break;
                    }
                }
                if gap || queue.len() > 64 {
                    reconcile_epoch.fetch_add(1, Ordering::SeqCst);
                    reconcile_generation.fetch_add(1, Ordering::SeqCst);
                    queue.clear();
                    reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                    continue;
                }
                let old = reconcile_track
                    .lock()
                    .ok()
                    .and_then(|t| t.clone())
                    .filter(|t| {
                        t.epoch == lineage.0
                            && t.revision == lineage.1
                            && t.focus.same_field(&focus)
                            && t.layout == ctx.layout
                    });
                let events = queue.iter().map(|i| i.event.clone()).collect::<Vec<_>>();
                let resolution = crate::queued_input::reconcile(
                    old.as_ref().map(state).as_ref(),
                    old.as_ref().map(|t| t.recovery.as_slice()).unwrap_or(&[]),
                    &events,
                    focus.caret,
                    |s| {
                        let t = Track {
                            focus: focus.with_range(Range {
                                location: s.caret as isize,
                                length: 0,
                            }),
                            tail: s.tail.clone(),
                            revision: lineage.1,
                            synchronized: true,
                            epoch: lineage.0,
                            layout: ctx.layout.clone(),
                            recovery: Vec::new(),
                        };
                        focus.selected == 0 && matches(&t, &focus)
                    },
                    |caret| focus.selected == 0 && boundary_at(&focus, caret, lineage.1),
                );
                if lineage
                    != (
                        reconcile_epoch.load(Ordering::SeqCst),
                        SETTINGS_REVISION.load(Ordering::SeqCst),
                    )
                    || platform::secure_input_active()
                    || !focus.still_focused()
                {
                    continue;
                }
                match resolution {
                    Resolution::Untrusted => {
                        // Dirty cancellation never reserves or skips a physical key.
                        reconcile_generation.fetch_add(1, Ordering::SeqCst);
                        if let Ok(mut t) = reconcile_track.lock() {
                            if let Some(t) = t.as_mut() {
                                t.synchronized = false;
                            }
                        }
                        let proposal = PROPOSAL.load(Ordering::SeqCst);
                        if proposal != 0 {
                            cancel_proposal(&reconcile_engine, proposal);
                        }
                        dirty_seen = dirty;
                        publish_status(3, live_token());
                    }
                    Resolution::Waiting => {
                        dirty_seen = dirty;
                        publish_status(3, live_token());
                    }

                    Resolution::Baseline(s) => {
                        // Keep every queued event. This real-caret baseline carries
                        // explicit pre/post-Space hypotheses until an exact observed
                        // prefix distinguishes them; no hook watermark is a host ACK.
                        reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                        let t = Track {
                            focus: focus.clone(),
                            tail: s.tail,
                            revision: lineage.1,
                            synchronized: true,
                            epoch: lineage.0,
                            layout: ctx.layout.clone(),
                            recovery: crate::queued_input::recovery_offsets(&events),
                        };
                        if let Ok(mut slot) = reconcile_track.lock() {
                            if lineage
                                == (
                                    reconcile_epoch.load(Ordering::SeqCst),
                                    SETTINGS_REVISION.load(Ordering::SeqCst),
                                )
                            {
                                *slot = Some(t);
                            }
                        }
                        dirty_seen = dirty;
                        publish_status(
                            3,
                            crate::runtime_state::Token {
                                generation: watermark,
                                revision: lineage.1,
                            },
                        );
                    }

                    Resolution::Lost => {
                        // Snapshot could precede the corresponding hook enqueue.
                        // Retry once the sequence/dirty watermark is stable.
                        if watermark != reconcile_generation.load(Ordering::SeqCst) {
                            continue;
                        }
                        queue.clear();
                        reset(&reconcile_engine, &reconcile_track, &reconcile_watch);
                        reconcile_epoch.fetch_add(1, Ordering::SeqCst);
                        reconcile_generation.fetch_add(1, Ordering::SeqCst);
                        dirty_seen = dirty;
                        publish_status(3, live_token());
                    }
                    Resolution::Verified {
                        consumed,
                        skipped,
                        state: s,
                    } => {
                        if consumed == 0 && !queue.is_empty() {
                            continue;
                        }
                        let delivered = queue.drain(..consumed).skip(skipped).collect::<Vec<_>>();
                        let decision_generation = reconcile_generation.load(Ordering::SeqCst);
                        let mut candidate_action = None;
                        if let Ok(mut e) = reconcile_engine.lock() {
                            if !e.config().enabled
                                || ctx
                                    .app
                                    .as_ref()
                                    .is_none_or(|a| e.config().is_excluded_app(a))
                                || platform::secure_input_active()
                                || lineage
                                    != (
                                        reconcile_epoch.load(Ordering::SeqCst),
                                        SETTINGS_REVISION.load(Ordering::SeqCst),
                                    )
                            {
                                queue.clear();
                                e.invalidate_caret();
                                continue;
                            }
                            let proposal = PROPOSAL.load(Ordering::SeqCst);
                            if proposal != 0
                                && proposal != reconcile_generation.load(Ordering::SeqCst)
                            {
                                e.cancel_proposal();
                                PROPOSAL.store(0, Ordering::SeqCst);
                            }
                            let (action, received) = crate::queued_input::deliver(
                                &mut e,
                                &delivered,
                                &ctx,
                                reconcile_sequence.load(Ordering::SeqCst),
                                !queue.is_empty(),
                                || {
                                    lineage
                                        == (
                                            reconcile_epoch.load(Ordering::SeqCst),
                                            SETTINGS_REVISION.load(Ordering::SeqCst),
                                        )
                                        && !platform::secure_input_active()
                                },
                            );
                            CORE_COUNT.fetch_add(received as u64, Ordering::Relaxed);
                            candidate_action = action;
                            if candidate_action.is_some() {
                                PROPOSAL.store(decision_generation, Ordering::SeqCst);
                            }
                        }
                        let compact = crate::queued_input::minimal_state(&s);
                        let t = Track {
                            focus: focus.clone(),
                            tail: compact.tail,
                            revision: lineage.1,
                            synchronized: true,
                            epoch: lineage.0,
                            layout: ctx.layout.clone(),
                            recovery: Vec::new(),
                        };
                        if let Ok(mut slot) = reconcile_track.lock() {
                            if lineage
                                == (
                                    reconcile_epoch.load(Ordering::SeqCst),
                                    SETTINGS_REVISION.load(Ordering::SeqCst),
                                )
                            {
                                *slot = Some(t.clone());
                            }
                        }
                        dirty_seen = dirty;
                        if let Some((action, sequence)) = candidate_action.filter(|(_, seq)| {
                            *seq == reconcile_sequence.load(Ordering::SeqCst)
                                && decision_generation
                                    == reconcile_generation.load(Ordering::SeqCst)
                        }) {
                            ACTION_COUNT.fetch_add(1, Ordering::Relaxed);
                            let _ = tx.send(Pending {
                                action,
                                origin: t.focus,
                                generation: decision_generation,
                                sequence,
                                revision: lineage.1,
                                epoch: lineage.0,
                                layout: ctx.layout.clone(),
                            });
                        }
                        publish_status(
                            4,
                            crate::runtime_state::Token {
                                generation: decision_generation,
                                revision: lineage.1,
                            },
                        );
                    }
                }
            }
            reconcile_running.store(false, Ordering::SeqCst);
        })
        .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;
    let flag = running.clone();
    let shutdown_engine = engine.clone();
    std::thread::Builder::new()
        .name("rekey-safe-hook".into())
        .spawn(move || {
            while !platform::has_permission() {
                std::thread::sleep(Duration::from_secs(1));
            }
            flag.store(true, Ordering::SeqCst);
            let result = platform::run(move |event| {
                if event.synthetic {
                    return;
                }
                INPUT_COUNT.fetch_add(1, Ordering::Relaxed);
                let sequence = crate::queued_input::reserve_input(&input_sequence, &generation);
                // No AX, AppKit/main-thread dispatch, blocking mutex, or queue wait
                // here. Ineligible input invalidates lineage without retaining chars.
                let eligible = crate::queued_input::eligible(
                    &event,
                    CAPTURE_ENABLED.load(Ordering::SeqCst),
                    platform::secure_input_active(),
                );
                if !eligible {
                    epoch.fetch_add(1, Ordering::SeqCst);
                    return;
                }
                let item = Input {
                    event,
                    sequence,
                    epoch: epoch.load(Ordering::SeqCst),
                    revision: SETTINGS_REVISION.load(Ordering::SeqCst),
                };
                if input_tx.try_send(item).is_err() {
                    epoch.fetch_add(1, Ordering::SeqCst);
                    generation.fetch_add(1, Ordering::SeqCst);
                }
            });
            flag.store(false, Ordering::SeqCst);
            invalidate_pending();
            if let Ok(mut e) = shutdown_engine.lock() {
                e.invalidate_caret();
            }
            if let Err(err) = result {
                log::error!("safe hook stopped: {err}");
            }
        })
        .map_err(|e| rekey_hook::HookError::Os(e.to_string()))?;
    Ok(running)
}

#[cfg(test)]
mod tests {
    #[test]
    fn missed_prefix_requires_whitespace_before_tracking() {
        assert!(!crate::input_phase::known_boundary(5, Some("g")));
        assert!(!crate::input_phase::known_boundary(5, None));
        assert!(crate::input_phase::known_boundary(5, Some(" ")));
    }
    #[test]
    fn empty_field_is_a_known_boundary() {
        assert!(crate::input_phase::known_boundary(0, None));
    }
    #[test]
    fn overflow_reset_does_not_license_suffix_correction() {
        assert!(!crate::input_phase::known_boundary(129, Some("x")));
        assert!(crate::input_phase::known_boundary(130, Some(" ")));
    }
}
