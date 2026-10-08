//! Pure reconciliation of a listen-only key event with an AX before/after snapshot.
use rekey_core::input::{Key, KeyEvent};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    pub caret: usize,
    pub tail: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Before,
    After,
    Unchanged,
}
/// A single event has one candidate after-state. Unsupported/overflow input cancels.
pub fn advance(before: &State, event: &KeyEvent) -> Option<State> {
    if event.synthetic || event.modifiers.is_shortcut() {
        return None;
    }
    let mut after = before.clone();
    let inserted = match event.key {
        Key::Character => Some(event.character?),
        Key::Space => Some(' '),
        Key::ModifiersChanged => None,
        _ => return None,
    };
    if let Some(c) = inserted {
        // Core deletion counts currently assume BMP. Keep that conservative contract.
        if c.len_utf16() != 1 {
            return None;
        }
        if event.key == Key::Character && after.tail.ends_with(' ') {
            after.tail.clear();
        }
        after.tail.push(c);
        after.caret = after.caret.checked_add(c.len_utf16())?;
    }
    (after.tail.encode_utf16().count() <= 128).then_some(after)
}
/// Both matches are allowed only when there was no text/range change.
pub fn reconcile(
    before: &State,
    after: &State,
    before_matches: bool,
    after_matches: bool,
) -> Option<Phase> {
    match (before_matches, after_matches) {
        (true, false) => Some(Phase::Before),
        (false, true) => Some(Phase::After),
        (true, true) if before == after => Some(Phase::Unchanged),
        _ => None,
    }
}
/// Fresh delivery fence sampled by the adapter immediately before core input.
#[derive(Clone, Copy)]
pub struct Fence {
    pub expected_generation: u64,
    pub generation: u64,
    pub expected_revision: u64,
    pub revision: u64,
    pub secure: bool,
    pub same_field: bool,
    pub allowed: bool,
}
pub fn deliver<T>(fence: Fence, receive: impl FnOnce() -> T) -> Option<T> {
    if fence.generation != fence.expected_generation
        || fence.revision != fence.expected_revision
        || fence.secure
        || !fence.same_field
        || !fence.allowed
    {
        return None;
    }
    Some(receive())
}
/// Take identity/policy probes before sampling the final atomic fence.
pub fn after_probe(probe: impl FnOnce() -> bool, sample: impl FnOnce() -> Fence) -> Fence {
    let same_field = probe();
    let mut fence = sample();
    fence.same_field = same_field;
    fence
}
pub fn bootstrap(
    caret: usize,
    event: &KeyEvent,
    mut boundary: impl FnMut(usize) -> bool,
    mut matches: impl FnMut(&State) -> bool,
) -> Option<(State, State)> {
    let before = State {
        caret,
        tail: String::new(),
    };
    let pre = boundary(caret)
        .then(|| advance(&before, event))
        .flatten()
        .map(|after| (before.clone(), after));
    let inserted = match event.key {
        Key::Character => event.character,
        Key::Space => Some(' '),
        _ => None,
    };
    let post = inserted.and_then(|c| {
        let before = State {
            caret: caret.checked_sub(c.len_utf16())?,
            tail: String::new(),
        };
        if !boundary(before.caret) {
            return None;
        }
        let after = advance(&before, event)?;
        matches(&after).then_some((before, after))
    });
    match (pre, post) {
        (Some(pair), None) | (None, Some(pair)) => Some(pair),
        _ => None,
    }
}
/// A deferred bootstrap hint can alter the predecessor without altering tail.
/// Re-prove the boundary on the final snapshot, never cache its earlier proof.
pub fn confirm_bootstrap(
    before: &State,
    after: &State,
    boundary: impl FnOnce(usize) -> bool,
    mut matches: impl FnMut(&State) -> bool,
) -> Option<Phase> {
    if !boundary(before.caret) {
        return None;
    }
    reconcile(before, after, matches(before), matches(after))
}
pub fn known_boundary(caret: usize, predecessor: Option<&str>) -> bool {
    rekey_hook::focus::boundary_proven(caret, predecessor)
}
/// Exact bounded snapshot comparison, also used by deterministic OS-free tests.
#[cfg(test)]
fn content_matches(state: &State, caret: usize, text: &str) -> bool {
    if state.caret != caret {
        return false;
    }
    let units: Vec<_> = text.encode_utf16().collect();
    let n = state.tail.encode_utf16().count();
    if caret < n || caret > units.len() {
        return false;
    }
    String::from_utf16(&units[caret - n..caret]).ok().as_deref() == Some(state.tail.as_str())
}
#[cfg(test)]
fn matches(state: &State, caret: usize, text: &str) -> bool {
    if !content_matches(state, caret, text) {
        return false;
    }
    let start = state.caret - state.tail.encode_utf16().count();
    let units: Vec<_> = text.encode_utf16().collect();
    let predecessor = if start > 0 {
        String::from_utf16(&units[start - 1..start]).ok()
    } else {
        None
    };
    known_boundary(start, predecessor.as_deref())
}
#[cfg(test)]
mod tests {
    use super::*;
    use rekey_core::input::Modifiers;
    fn event(c: char) -> KeyEvent {
        KeyEvent {
            character: Some(c),
            key: if c == ' ' { Key::Space } else { Key::Character },
            modifiers: Modifiers::default(),
            synthetic: false,
        }
    }
    fn state(tail: &str) -> State {
        State {
            caret: tail.encode_utf16().count(),
            tail: tail.into(),
        }
    }
    fn resolve(before: &State, event: &KeyEvent, caret: usize, text: &str) -> Option<State> {
        let after = advance(before, event)?;
        reconcile(
            before,
            &after,
            matches(before, caret, text),
            matches(&after, caret, text),
        )?;
        Some(after)
    }
    #[test]
    fn pre_to_pre_and_pre_to_post_space_have_identical_origin() {
        let before = state("ghbdtn");
        let e = event(' ');
        let pre = resolve(&before, &e, 6, "ghbdtn").unwrap();
        let post = resolve(&before, &e, 7, "ghbdtn ").unwrap();
        assert_eq!(pre, post);
        assert_eq!(post.caret, 7);
        assert_eq!(post.tail, "ghbdtn ");
    }
    #[test]
    fn post_to_post_and_mixed_phases_consume_each_event_once() {
        for phases in [
            [false, false, false],
            [true, true, true],
            [true, false, true],
            [false, true, false],
        ] {
            let mut s = state("");
            let mut text = String::new();
            let mut delivered = 0;
            for (c, post) in ['g', 'h', ' '].into_iter().zip(phases) {
                if post {
                    text.push(c);
                }
                s = resolve(&s, &event(c), text.len(), &text).unwrap();
                delivered += 1;
                if !post {
                    text.push(c);
                }
            }
            assert_eq!(s, state("gh "));
            assert_eq!(delivered, 3);
        }
    }
    #[test]
    fn observer_before_and_after_snapshot_accept_only_published_transition() {
        let before = state("ghbdtn");
        let after = advance(&before, &event(' ')).unwrap();
        for (caret, text) in [(6, "ghbdtn"), (7, "ghbdtn ")] {
            assert!(matches(&before, caret, text) || matches(&after, caret, text));
            assert!(reconcile(
                &before,
                &after,
                matches(&before, caret, text),
                matches(&after, caret, text)
            )
            .is_some());
        }
        assert!(!matches(&before, 8, "ghbdtn x") && !matches(&after, 8, "ghbdtn x"));
    }
    #[test]
    fn two_delivered_characters_or_foreign_edit_cancel() {
        assert!(resolve(&state("g"), &event('h'), 3, "ghb").is_none());
        assert!(resolve(&state("ghbdtn"), &event(' '), 7, "xxxxxx ").is_none());
        assert!(resolve(&state("ghbdtn"), &event(' '), 6, "xxxxxx").is_none());
    }
    fn clear_fence() -> Fence {
        Fence {
            expected_generation: 7,
            generation: 7,
            expected_revision: 2,
            revision: 2,
            secure: false,
            same_field: true,
            allowed: true,
        }
    }
    #[test]
    fn revision_secure_focus_generation_changes_cancel_before_engine_delivery() {
        let mut received = Vec::new();
        let mutations: [fn(&mut Fence); 5] = [
            |f| f.revision += 1,
            |f| f.secure = true,
            |f| f.same_field = false,
            |f| f.generation += 1,
            |f| f.allowed = false,
        ];
        for change in mutations {
            let mut fence = clear_fence();
            change(&mut fence);
            assert!(deliver(fence, || received.push('h')).is_none());
            assert!(received.is_empty());
        }
        assert!(deliver(clear_fence(), || received.push('h')).is_some());
        assert_eq!(received, vec!['h']);
    }
    #[test]
    fn all_phase_sequences_deliver_exact_word_once_to_real_core() {
        use rekey_core::{
            config::Config,
            engine::{Action, Engine},
            input::Context,
            model::Models,
        };
        let models = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rekey-core/data/models");
        for phases in [
            [false; 7],
            [true; 7],
            [true, false, true, false, true, false, true],
        ] {
            let mut engine = Engine::new(
                Models::load_dir(&models).unwrap(),
                Config {
                    layouts: vec!["us".into(), "ru".into()],
                    ..Config::default()
                },
            );
            let ctx = Context {
                app: Some("com.apple.TextEdit".into()),
                layout: Some("us".into()),
                secure_input: false,
            };
            let mut s = state("");
            let mut text = String::new();
            let mut received = 0;
            let mut replacements = Vec::new();
            for (c, post) in "ghbdtn ".chars().zip(phases) {
                let e = event(c);
                if post {
                    text.push(c);
                }
                s = resolve(&s, &e, text.len(), &text).unwrap();
                let action = deliver(clear_fence(), || {
                    received += 1;
                    engine.on_key(&e, &ctx)
                })
                .unwrap();
                if let Action::Replace { original, text, .. } = action {
                    replacements.push((s.caret, original, text));
                }
                if !post {
                    text.push(c);
                }
            }
            assert_eq!(received, 7);
            assert_eq!(replacements, vec![(7, "ghbdtn ".into(), "привет ".into())]);
        }
    }
    #[test]
    fn blocking_focus_probe_invalidation_is_sampled_after_probe() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let generation = AtomicU64::new(7);
        let fence = after_probe(
            || {
                generation.store(8, Ordering::SeqCst);
                true
            },
            || Fence {
                generation: generation.load(Ordering::SeqCst),
                ..clear_fence()
            },
        );
        let mut received = 0;
        assert!(deliver(fence, || received += 1).is_none());
        assert_eq!(received, 0);
    }
    #[test]
    fn bootstrap_notification_during_policy_wait_is_reconciled_not_authorized() {
        use rekey_hook::focus::BootstrapState;
        use std::sync::Arc;
        for text in ["g", "x", "gh"] {
            let observer = Arc::new(BootstrapState::default());
            // No Track/expectations yet. Only this event's text hint is scoped.
            let scope = observer.begin(7, 2).unwrap();
            assert!(observer.defer_hint(false, 7, |r| r == 2));
            assert!(!observer.defer_hint(true, 7, |r| r == 2));
            assert!(!observer.defer_hint(false, 8, |r| r == 2));
            assert!(!observer.defer_hint(false, 7, |r| r == 3));
            let caret = text.len();
            let pair = bootstrap(
                caret,
                &event('g'),
                |c| known_boundary(c, if c > 0 { Some(&text[c - 1..c]) } else { None }),
                |s| matches(s, caret, text),
            );
            assert_eq!(scope.finish(), 1);
            assert!(!observer.defer_hint(false, 7, |r| r == 2));
            let mut received = 0;
            if let Some((before, after)) = pair {
                assert!(reconcile(
                    &before,
                    &after,
                    matches(&before, caret, text),
                    matches(&after, caret, text)
                )
                .is_some());
                deliver(clear_fence(), || received += 1).unwrap();
                assert_eq!(after, state("g"));
            }
            assert_eq!(received, usize::from(text == "g"));
        }
    }
    #[test]
    fn deferred_foreign_edit_removing_boundary_cancels_unchanged_tail() {
        use rekey_hook::focus::BootstrapState;
        use std::sync::Arc;
        let observer = Arc::new(BootstrapState::default());
        let scope = observer.begin(7, 2).unwrap();
        let initial = " g";
        let (before, after) = bootstrap(
            2,
            &event('g'),
            |c| {
                known_boundary(
                    c,
                    if c > 0 {
                        Some(&initial[c - 1..c])
                    } else {
                        None
                    },
                )
            },
            |s| matches(s, 2, initial),
        )
        .unwrap();
        assert!(observer.defer_hint(false, 7, |r| r == 2));
        assert_eq!(scope.finish(), 1);
        for fresh in [" g", "xg"] {
            assert!(content_matches(&after, 2, fresh)); // exact tail survives foreign edit
            let phase = confirm_bootstrap(
                &before,
                &after,
                |c| known_boundary(c, Some(&fresh[c - 1..c])),
                |s| matches(s, 2, fresh),
            );
            let mut received = 0;
            if phase.is_some() {
                deliver(clear_fence(), || received += 1).unwrap();
            }
            assert_eq!(received, usize::from(fresh == " g"));
        }
    }
    #[test]
    fn foreign_edit_after_bootstrap_scope_before_next_key_invalidates_word_start() {
        use rekey_hook::focus::BootstrapState;
        use std::sync::Arc;
        let observer = Arc::new(BootstrapState::default());
        let scope = observer.begin(7, 2).unwrap();
        let initial = " g";
        let (before, tracked) = bootstrap(
            2,
            &event('g'),
            |c| {
                known_boundary(
                    c,
                    if c > 0 {
                        Some(&initial[c - 1..c])
                    } else {
                        None
                    },
                )
            },
            |s| matches(s, 2, initial),
        )
        .unwrap();
        assert_eq!(scope.finish(), 0);
        assert!(confirm_bootstrap(
            &before,
            &tracked,
            |c| known_boundary(c, Some(&initial[c - 1..c])),
            |s| matches(s, 2, initial)
        )
        .is_some());
        // Later unrelated value notification, then next h event; tail/range
        // remain identical while the whole-word start proof is destroyed.
        let changed = "xg";
        assert!(!observer.defer_hint(false, 7, |r| r == 2));
        assert!(content_matches(&tracked, 2, changed));
        assert!(!matches(&tracked, 2, changed));
        let after = advance(&tracked, &event('h')).unwrap();
        assert!(content_matches(&after, 3, "xgh"));
        assert!(!matches(&after, 3, "xgh"));
        let mut received = 0;
        if reconcile(
            &tracked,
            &after,
            matches(&tracked, 2, changed),
            matches(&after, 2, changed),
        )
        .is_some()
        {
            deliver(clear_fence(), || received += 1).unwrap();
        }
        assert_eq!(received, 0);
    }
    #[test]
    fn bootstrap_needs_phase_specific_boundary_not_blind_caret_subtraction() {
        assert!(known_boundary(0, None));
        assert!(known_boundary(4, Some(" ")));
        assert!(!known_boundary(4, Some("g")));
        assert!(!known_boundary(4, None));
        let before = State {
            caret: 4,
            tail: String::new(),
        };
        let after = advance(&before, &event('g')).unwrap();
        assert!(matches(&after, 5, "abc g"));
        assert!(!matches(&after, 5, "abc x"));
        assert!(!known_boundary(4, Some("c")));
    }
    #[test]
    fn unchanged_modifier_is_not_double_advanced() {
        let before = state("gh ");
        let mut e = event(' ');
        e.key = Key::ModifiersChanged;
        let after = advance(&before, &e).unwrap();
        assert_eq!(before, after);
        assert_eq!(
            reconcile(&before, &after, true, true),
            Some(Phase::Unchanged)
        );
    }
    #[test]
    fn unknown_ambiguous_overflow_and_shortcuts_cancel() {
        let before = state("g");
        let after = advance(&before, &event('h')).unwrap();
        assert_eq!(reconcile(&before, &after, true, true), None);
        assert_eq!(reconcile(&before, &after, false, false), None);
        assert!(advance(&state(&"g".repeat(128)), &event('h')).is_none());
        let mut e = event('h');
        e.modifiers.meta = true;
        assert!(advance(&before, &e).is_none());
    }
}
