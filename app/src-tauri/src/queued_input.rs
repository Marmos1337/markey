//! Reconcile a bounded sequence of known input against an observed AX baseline.
//! Notifications are hints, not guessed keystrokes. Only an exact observed prefix
//! is delivered; whitespace recovery always uses the real observed caret.
use crate::input_phase::State;
use rekey_core::input::{Key, KeyEvent};
use std::collections::VecDeque;
pub fn reserve_input(
    sequence: &std::sync::atomic::AtomicU64,
    cancellation: &std::sync::atomic::AtomicU64,
) -> u64 {
    use std::sync::atomic::Ordering;
    let sequence = sequence.fetch_add(1, Ordering::SeqCst) + 1;
    cancellation.fetch_add(1, Ordering::SeqCst);
    sequence
}
#[derive(Clone)]
pub struct Input {
    pub event: KeyEvent,
    pub sequence: u64,
    pub epoch: u64,
    pub revision: u64,
}
pub struct EventQueue {
    pub items: VecDeque<Input>,
    lineage: (u64, u64),
    last: Option<u64>,
}
impl EventQueue {
    pub fn new(lineage: (u64, u64)) -> Self {
        Self {
            items: VecDeque::new(),
            lineage,
            last: None,
        }
    }
    pub fn reset(&mut self, lineage: (u64, u64)) {
        self.items.clear();
        self.last = None;
        self.lineage = lineage;
    }
    /// A lost event cancels the entire lineage; stale subscriptions are ignored.
    pub fn push(&mut self, input: Input) -> Result<(), ()> {
        if (input.epoch, input.revision) != self.lineage {
            return Ok(());
        }
        if self.items.len() >= 64 || self.last.is_some_and(|n| input.sequence != n + 1) {
            return Err(());
        }
        self.last = Some(input.sequence);
        self.items.push_back(input);
        Ok(())
    }
}
impl std::ops::Deref for EventQueue {
    type Target = VecDeque<Input>;
    fn deref(&self) -> &Self::Target {
        &self.items
    }
}
impl std::ops::DerefMut for EventQueue {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.items
    }
}
pub fn eligible(event: &KeyEvent, enabled: bool, secure: bool) -> bool {
    enabled
        && !secure
        && !event.synthetic
        && matches!(
            event.key,
            Key::Character | Key::Space | Key::ModifiersChanged
        )
        && !event.modifiers.is_shortcut()
}
/// Production core delivery after an exact, fresh host-prefix proof. Earlier
/// word proposals in a batch are cancelled before receiving the next prefix.
pub fn deliver(
    engine: &mut rekey_core::Engine,
    events: &[Input],
    ctx: &rekey_core::input::Context,
    current_sequence: u64,
    remaining: bool,
    mut guard: impl FnMut() -> bool,
) -> (Option<(rekey_core::engine::Action, u64)>, usize) {
    use rekey_core::engine::Action;
    let mut proposal = None;
    let mut received = 0;
    for (index, item) in events.iter().enumerate() {
        if !guard() {
            engine.invalidate_caret();
            return (None, received);
        }
        let action = engine.on_key(&item.event, ctx);
        received += 1;
        if matches!(action, Action::Replace { .. }) {
            if index + 1 == events.len() && !remaining && item.sequence == current_sequence {
                proposal = Some((action, item.sequence));
            } else {
                engine.cancel_proposal();
            }
        }
    }
    (proposal, received)
}

#[derive(Debug, PartialEq, Eq)]
pub enum Resolution {
    Verified {
        consumed: usize,
        skipped: usize,
        state: State,
    },
    Baseline(State),
    /// A dirty notification can precede hook publication. Retain only the
    /// untrusted proof candidate; cancel proposals and deliver no input.
    Untrusted,
    Waiting,
    Lost,
}
pub fn advance(before: &State, event: &KeyEvent) -> Option<State> {
    if event.synthetic || event.modifiers.is_shortcut() {
        return None;
    }
    let mut after = before.clone();
    match event.key {
        Key::Character | Key::Space => {
            let c = if event.key == Key::Space {
                ' '
            } else {
                event.character?
            };
            if c.len_utf16() != 1 {
                return None;
            }
            after.tail.push(c);
            after.caret = after.caret.checked_add(1)?;
        }
        Key::ModifiersChanged => {}
        _ => return None,
    }
    (after.tail.encode_utf16().count() <= 128).then_some(after)
}
pub fn reconcile(
    before: Option<&State>,
    recovery: &[usize],
    events: &[KeyEvent],
    caret: usize,
    mut exact: impl FnMut(&State) -> bool,
    mut boundary: impl FnMut(usize) -> bool,
) -> Resolution {
    if let Some(before) = before {
        let mut accepted: Option<(usize, usize, State)> = None;
        let mut ambiguous = false;
        for skip in if recovery.is_empty() {
            vec![0]
        } else {
            recovery.to_vec()
        } {
            if skip > events.len() {
                continue;
            }
            let mut state = before.clone();
            for count in skip..=events.len() {
                if state.caret == caret && exact(&state) {
                    if accepted
                        .as_ref()
                        .is_some_and(|(_, old_skip, old)| *old_skip != skip || old != &state)
                    {
                        ambiguous = true;
                    } else {
                        // Known modifier-only events with identical state are
                        // delivered in their original order, once, within one phase.
                        accepted = Some((count, skip, state.clone()));
                    }
                }
                if count < events.len() {
                    let Some(next) = advance(&state, &events[count]) else {
                        break;
                    };
                    state = next;
                }
            }
        }
        if ambiguous {
            return Resolution::Waiting;
        }
        if let Some((consumed, skipped, state)) = accepted {
            return Resolution::Verified {
                consumed,
                skipped,
                state,
            };
        }
    }
    if events.is_empty() {
        return Resolution::Untrusted;
    }
    if boundary(caret) {
        Resolution::Baseline(State {
            caret,
            tail: String::new(),
        })
    } else {
        Resolution::Lost
    }
}
/// Recovery records hypotheses, never consumes an event just because it was
/// enqueued. The next exact snapshot distinguishes pre-Space from post-Space.
pub fn recovery_offsets(events: &[KeyEvent]) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            events
                .iter()
                .enumerate()
                .filter(|(_, e)| e.key == Key::Space)
                .map(|(i, _)| i + 1),
        )
        .collect()
}
pub fn minimal_state(state: &State) -> State {
    let tail = state
        .tail
        .rsplit_once(' ')
        .map(|(_, tail)| tail)
        .unwrap_or(&state.tail);
    State {
        caret: state.caret,
        tail: tail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rekey_core::input::Modifiers;
    fn key(c: char) -> KeyEvent {
        KeyEvent {
            character: Some(c),
            key: if c == ' ' { Key::Space } else { Key::Character },
            modifiers: Modifiers::default(),
            synthetic: false,
        }
    }
    // The host and queue have separate clocks. Notifications may arrive before
    // enqueue, and a snapshot can already contain several queued characters.
    struct Host {
        text: String,
        caret: usize,
        notifications: usize,
        sequence: u64,
        cancellations: u64,
        queue: EventQueue,
        baseline: Option<State>,
        recovery: Vec<usize>,
        delivered: String,
        last_delivered: Vec<Input>,
    }
    impl Host {
        fn new(text: &str, baseline: bool) -> Self {
            Self {
                text: text.into(),
                caret: text.len(),
                notifications: 0,
                sequence: 0,
                cancellations: 0,
                queue: EventQueue::new((0, 0)),
                recovery: vec![],
                baseline: baseline.then(|| State {
                    caret: text.len(),
                    tail: String::new(),
                }),
                delivered: String::new(),
                last_delivered: vec![],
            }
        }
        fn host_key(&mut self, c: char) {
            self.text.insert(self.caret, c);
            self.caret += 1;
            self.notifications += 1;
        }
        fn enqueue(&mut self, c: char) {
            self.sequence += 1;
            self.queue
                .push(Input {
                    event: key(c),
                    sequence: self.sequence,
                    epoch: 0,
                    revision: 0,
                })
                .unwrap();
        }
        fn poll(&mut self) -> Resolution {
            self.poll_snapshot(self.text.clone(), self.caret)
        }
        fn poll_snapshot(&mut self, text: String, caret: usize) -> Resolution {
            self.last_delivered.clear();
            let events = self
                .queue
                .iter()
                .map(|i| i.event.clone())
                .collect::<Vec<_>>();
            let result = reconcile(
                self.baseline.as_ref(),
                &self.recovery,
                &events,
                caret,
                |s| {
                    let n = s.tail.len();
                    s.caret >= n
                        && text.get(s.caret - n..s.caret) == Some(s.tail.as_str())
                        && (s.caret == n || text.as_bytes()[s.caret - n - 1].is_ascii_whitespace())
                },
                |p| {
                    p == 0
                        || text
                            .as_bytes()
                            .get(p - 1)
                            .is_some_and(u8::is_ascii_whitespace)
                },
            );
            match &result {
                Resolution::Verified {
                    consumed,
                    skipped,
                    state,
                } => {
                    self.last_delivered = self.queue.drain(..*consumed).skip(*skipped).collect();
                    for e in &self.last_delivered {
                        if let Some(c) = e.event.character {
                            self.delivered.push(c);
                        }
                    }
                    self.baseline = Some(minimal_state(state));
                    self.recovery.clear();
                }
                Resolution::Baseline(s) => {
                    self.baseline = Some(s.clone());
                    self.recovery = recovery_offsets(&events);
                }
                Resolution::Lost => {
                    self.queue.clear();
                    self.baseline = None;
                }
                Resolution::Untrusted => {
                    self.cancellations += 1;
                }
                Resolution::Waiting => {}
            }
            result
        }
    }
    #[test]
    fn failed_word_post_space_recovers_at_real_caret_without_phantom_space() {
        let mut h = Host::new("ghbdtn", false);
        h.host_key(' ');
        h.enqueue(' ');
        assert_eq!(
            h.poll(),
            Resolution::Baseline(State {
                caret: 7,
                tail: String::new()
            })
        );
        assert!(h.delivered.is_empty());
        for c in "ghbdtn ".chars() {
            h.host_key(c);
            h.enqueue(c);
            h.poll();
        }
        assert_eq!(h.delivered, "ghbdtn ");
        assert_eq!(h.baseline.unwrap().caret, 14);
    }
    #[test]
    fn notification_before_hook_publication_does_not_destroy_known_baseline() {
        let mut h = Host::new("", true);
        h.host_key('g');
        assert_eq!(h.poll(), Resolution::Untrusted);
        h.enqueue('g');
        assert!(matches!(h.poll(), Resolution::Verified { consumed: 1, .. }));
        assert_eq!(h.delivered, "g");
    }
    #[test]
    fn snapshot_ahead_two_to_six_known_keys_delivers_once() {
        for n in 2..=6 {
            let mut h = Host::new("", true);
            for c in "ghbdtn"[..n].chars() {
                h.host_key(c);
                h.enqueue(c);
            }
            assert!(matches!(h.poll(),Resolution::Verified{consumed,..} if consumed==n));
            h.poll();
            assert_eq!(h.delivered, &"ghbdtn"[..n]);
        }
    }
    #[test]
    fn unknown_insertion_and_caret_move_never_deliver_guessed_events() {
        let mut h = Host::new("", true);
        h.host_key('x');
        h.enqueue('g');
        assert_eq!(h.poll(), Resolution::Lost);
        assert!(h.delivered.is_empty());
        let mut h = Host::new(" ", true);
        h.host_key('g');
        h.enqueue('g');
        h.caret = 0;
        assert!(matches!(h.poll(), Resolution::Baseline(_)));
        assert!(h.delivered.is_empty());
    }
    #[test]
    fn full_queued_span_is_verified_across_word_boundaries() {
        let mut h = Host::new("", true);
        for c in "a b ".chars() {
            h.host_key(c);
            h.enqueue(c);
        }
        h.text.replace_range(0..1, "x");
        assert!(matches!(h.poll(), Resolution::Baseline(_)));
        assert!(h.delivered.is_empty());
    }
    fn core() -> rekey_core::Engine {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rekey-core/data/models");
        rekey_core::Engine::new(
            rekey_core::Models::load_dir(&path).unwrap(),
            rekey_core::Config::default(),
        )
    }
    fn context() -> rekey_core::Context {
        rekey_core::Context {
            app: Some("com.apple.TextEdit".into()),
            layout: Some("us".into()),
            secure_input: false,
        }
    }
    fn inputs(text: &str, first: u64) -> Vec<Input> {
        text.chars()
            .enumerate()
            .map(|(i, c)| Input {
                event: key(c),
                sequence: first + i as u64,
                epoch: 0,
                revision: 0,
            })
            .collect()
    }
    #[test]
    fn failed_word_actual_space_then_next_word_yields_one_real_core_proposal() {
        let mut h = Host::new("ghbdtn", false);
        let mut e = core();
        h.host_key(' ');
        h.enqueue(' ');
        h.poll();
        let mut proposals = vec![];
        for c in "ghbdtn ".chars() {
            h.host_key(c);
            h.enqueue(c);
            h.poll();
            let (action, _) = deliver(
                &mut e,
                &h.last_delivered,
                &context(),
                h.sequence,
                !h.queue.is_empty(),
                || true,
            );
            if let Some(a) = action {
                proposals.push(a);
            }
        }
        assert_eq!(proposals.len(), 1);
        assert!(
            matches!(&proposals[0].0,rekey_core::Action::Replace{original,text,..} if original=="ghbdtn " && text=="привет ")
        );
    }
    #[test]
    fn new_keys_during_ipc_remain_after_snapshot_prefix_and_are_delivered_once() {
        let mut h = Host::new("", true);
        h.host_key('g');
        h.enqueue('g');
        let snapshot = h.text.clone();
        let caret = h.caret;
        h.host_key('h');
        h.enqueue('h');
        assert!(matches!(
            h.poll_snapshot(snapshot, caret),
            Resolution::Verified { consumed: 1, .. }
        ));
        assert_eq!(h.queue.len(), 1);
        h.poll();
        assert_eq!(h.delivered, "gh");
        assert!(h.queue.is_empty());
    }
    #[test]
    fn overflow_gap_and_focus_away_back_discard_old_lineage() {
        let mut q = EventQueue::new((0, 0));
        for item in inputs(&"g".repeat(64), 1) {
            q.push(item).unwrap();
        }
        assert!(q.push(inputs("h", 65).remove(0)).is_err());
        q.reset((1, 0));
        assert!(q.is_empty());
        q.push(inputs("g", 1).remove(0)).unwrap();
        assert!(q.is_empty()); // old subscription
        let mut item = inputs("g", 70).remove(0);
        item.epoch = 1;
        q.push(item).unwrap();
        let mut gap = inputs("h", 72).remove(0);
        gap.epoch = 1;
        assert!(q.push(gap).is_err());
        q.reset((2, 0));
        q.push(inputs("g", 73).remove(0)).unwrap();
        assert!(q.is_empty());
    }
    #[test]
    fn off_secure_and_mid_batch_policy_change_deliver_no_more_core_input() {
        assert!(!eligible(&key('g'), false, false));
        assert!(!eligible(&key('g'), true, true));
        let mut e = core();
        let mut calls = 0;
        let (proposal, received) =
            deliver(&mut e, &inputs("ghbdtn ", 1), &context(), 7, false, || {
                calls += 1;
                calls < 3
            });
        assert!(proposal.is_none());
        assert_eq!(received, 2);
        assert!(matches!(e.cycle_last_word(), rekey_core::Action::None));
    }
    #[test]
    fn cancelled_boundary_preserves_next_prefix_and_clears_speculative_cycle() {
        let mut e = core();
        let (proposal, _) = deliver(
            &mut e,
            &inputs("ghbdtn ghb", 1),
            &context(),
            10,
            false,
            || true,
        );
        assert!(proposal.is_none());
        let (proposal, _) = deliver(&mut e, &inputs("dtn ", 11), &context(), 14, false, || true);
        assert!(
            matches!(proposal,Some((rekey_core::Action::Replace{original,..},14)) if original=="ghbdtn ")
        );
    }
    #[test]
    fn modifier_equivalent_prefixes_deliver_in_known_order_once() {
        let mut h = Host::new("", true);
        for alt in [true, false] {
            h.sequence += 1;
            h.queue
                .push(Input {
                    event: KeyEvent {
                        key: Key::ModifiersChanged,
                        character: None,
                        synthetic: false,
                        modifiers: Modifiers {
                            alt,
                            ..Default::default()
                        },
                    },
                    sequence: h.sequence,
                    epoch: 0,
                    revision: 0,
                })
                .unwrap();
        }
        assert!(matches!(h.poll(), Resolution::Verified { consumed: 2, .. }));
        assert!(h.last_delivered[0].event.modifiers.alt);
        assert!(!h.last_delivered[1].event.modifiers.alt);
        h.poll();
        assert!(h.last_delivered.is_empty());
    }
    #[test]
    fn same_caret_tail_with_changed_boundary_cancels_proof() {
        let mut h = Host::new(" ", true);
        for c in "ghb".chars() {
            h.host_key(c);
            h.enqueue(c);
            h.poll();
        }
        h.text.replace_range(0..1, "x");
        h.notifications += 1;
        assert_eq!(h.poll(), Resolution::Untrusted);
        assert!(h.last_delivered.is_empty());
        h.host_key('d');
        h.enqueue('d');
        assert_eq!(h.poll(), Resolution::Lost);
        assert!(h.last_delivered.is_empty());
    }
    #[test]
    fn paused_reservation_before_publication_is_not_a_phantom_gap() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let physical = AtomicU64::new(0);
        let cancel = AtomicU64::new(0);
        let mut queue = EventQueue::new((0, 0));
        let reserved = reserve_input(&physical, &cancel);
        // Hook is paused here before try_send; dirty notification revokes the
        // writer through another clock, never changing physical queue order.
        cancel.fetch_add(1, Ordering::SeqCst);
        queue
            .push(Input {
                event: key('g'),
                sequence: reserved,
                epoch: 0,
                revision: 0,
            })
            .unwrap();
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.front().unwrap().sequence, 1);
        assert_eq!(physical.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn queued_character_before_host_application_is_retained_at_whitespace_baseline() {
        let mut h = Host::new("ghbdtn ", false);
        h.enqueue('g');
        assert!(matches!(h.poll(), Resolution::Baseline(_)));
        assert_eq!(h.queue.len(), 1);
        assert!(h.delivered.is_empty());
        h.host_key('g');
        assert!(matches!(
            h.poll(),
            Resolution::Verified {
                consumed: 1,
                skipped: 0,
                ..
            }
        ));
        assert_eq!(h.delivered, "g");
    }
    #[test]
    fn pre_and_post_space_hypotheses_are_disambiguated_by_real_snapshot() {
        for post in [true, false] {
            let mut h = Host::new("x ", false);
            h.enqueue(' ');
            if post {
                h.host_key(' ');
            }
            h.poll();
            assert_eq!(h.queue.len(), 1);
            assert!(h.delivered.is_empty());
            if !post {
                h.host_key(' ');
            }
            h.host_key('g');
            h.enqueue('g');
            h.poll();
            assert_eq!(h.delivered, if post { "g" } else { " g" });
            assert!(h.queue.is_empty());
        }
    }
    #[test]
    fn long_session_compacts_only_after_exact_batch_delivery() {
        let mut h = Host::new("", true);
        for _ in 0..80 {
            for c in "hello ".chars() {
                h.host_key(c);
                h.enqueue(c);
                h.poll();
            }
            assert!(h.baseline.as_ref().unwrap().tail.is_empty());
        }
        assert_eq!(h.delivered, "hello ".repeat(80));
    }
}
