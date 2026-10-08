//! Shared transaction orchestration, exercised with a stateful AX mock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    BeforeSelection,
    BeforeReplacement,
    Rollback,
}
pub trait Target {
    fn guard(&mut self, phase: Phase) -> bool;
    fn fragment(&mut self) -> Option<String>;
    fn select(&mut self) -> bool;
    fn replace(&mut self) -> bool;
    fn rollback(&mut self) -> bool;
}
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Replaced,
    Skipped,
    SelectionRestored,
    SelectionNotRestored,
}
pub fn execute(target: &mut impl Target, original: &str) -> Outcome {
    if !target.guard(Phase::BeforeSelection)
        || target.fragment().as_deref() != Some(original)
        || !target.guard(Phase::BeforeSelection)
    {
        return Outcome::Skipped;
    }
    if !target.select() {
        return Outcome::Skipped;
    }
    // A collapsed original caret is never acceptable once selection has been made.
    // The backend performs fast combined retained-target/range/fragment guards.
    if target.replace() {
        return Outcome::Replaced;
    }
    // Restoration needs unchanged originating focus, revision, range and fragment.
    if target.rollback() {
        Outcome::SelectionRestored
    } else {
        Outcome::SelectionNotRestored
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Copy)]
    enum Change {
        None,
        Focus,
        Collapsed,
        Settings,
        TapLost,
        Timeout,
        Fragment,
        Deleted,
        NewInput,
        Secure,
        Toggle,
        SelectFailure,
        ReplaceFailure,
        HostInput,
    }
    struct Mock {
        step: usize,
        at: usize,
        change: Change,
        live: bool,
        selected: bool,
        revision: bool,
        fragment: String,
        writes: usize,
        restores: usize,
        host_replacements: usize,
    }
    impl Mock {
        fn new(change: Change, at: usize) -> Self {
            Self {
                step: 0,
                at,
                change,
                live: true,
                selected: false,
                revision: true,
                fragment: "ghbdtn ".into(),
                writes: 0,
                restores: 0,
                host_replacements: 0,
            }
        }
        fn advance(&mut self) {
            self.step += 1;
            if self.step == self.at {
                match self.change {
                    Change::None => {}
                    Change::Collapsed => self.selected = false,
                    Change::Fragment => self.fragment = "different".into(),
                    Change::Settings => self.revision = false,
                    Change::HostInput => {
                        if self.selected {
                            self.fragment = "z".into();
                            self.selected = false;
                            self.host_replacements += 1;
                        }
                        self.revision = false;
                        self.live = false;
                    }
                    Change::TapLost | Change::Timeout | Change::NewInput => {
                        self.revision = false;
                        self.live = false;
                    }
                    _ => self.live = false,
                }
            }
        }
    }
    impl Target for Mock {
        fn guard(&mut self, p: Phase) -> bool {
            self.advance();
            self.live
                && self.revision
                && if p == Phase::BeforeSelection {
                    !self.selected
                } else {
                    self.selected
                }
        }
        fn fragment(&mut self) -> Option<String> {
            self.advance();
            self.live.then(|| self.fragment.clone())
        }
        fn select(&mut self) -> bool {
            self.advance();
            if matches!(self.change, Change::SelectFailure) {
                false
            } else {
                self.selected = true;
                true
            }
        }
        fn replace(&mut self) -> bool {
            self.advance();
            if !self.live
                || !self.revision
                || !self.selected
                || self.fragment != "ghbdtn "
                || matches!(self.change, Change::ReplaceFailure)
            {
                return false;
            }
            self.writes += 1;
            self.selected = false;
            true
        }
        fn rollback(&mut self) -> bool {
            self.advance();
            if !self.live
                || (!self.revision && !matches!(self.change, Change::Settings))
                || !self.selected
                || self.fragment != "ghbdtn "
            {
                return false;
            }
            self.selected = false;
            self.restores += 1;
            true
        }
    }
    #[test]
    fn exact_origin_replaces() {
        let mut m = Mock::new(Change::None, usize::MAX);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::Replaced);
        assert_eq!(m.writes, 1);
    }
    #[test]
    fn original_fragment_mismatch_skips() {
        let mut m = Mock::new(Change::None, usize::MAX);
        m.fragment = "another".into();
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::Skipped);
        assert_eq!(m.writes, 0);
    }
    #[test]
    fn adversarial_transitions_at_every_check() {
        for change in [
            Change::Focus,
            Change::Settings,
            Change::TapLost,
            Change::Timeout,
            Change::Fragment,
            Change::Deleted,
            Change::NewInput,
            Change::Secure,
            Change::Toggle,
        ] {
            for at in 1..=5 {
                let mut m = Mock::new(change, at);
                let _ = execute(&mut m, "ghbdtn ");
                assert_eq!(m.writes, 0, "write after transition at {at}");
            }
        }
    }
    #[test]
    fn collapsed_selection_never_inserts() {
        for at in 5..=5 {
            let mut m = Mock::new(Change::Collapsed, at);
            let _ = execute(&mut m, "ghbdtn ");
            assert_eq!(m.writes, 0);
        }
    }
    #[test]
    fn selection_failure_has_no_writes() {
        let mut m = Mock::new(Change::SelectFailure, usize::MAX);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::Skipped);
        assert_eq!(m.writes, 0);
    }
    #[test]
    fn replace_failure_restores_only_unchanged_selection() {
        let mut m = Mock::new(Change::ReplaceFailure, usize::MAX);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::SelectionRestored);
        assert_eq!(m.restores, 1);
        assert_eq!(m.writes, 0);
    }
    #[test]
    fn cancellation_does_not_blindly_restore() {
        let mut m = Mock::new(Change::NewInput, 5);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::SelectionNotRestored);
        assert_eq!(m.restores, 0);
    }
    #[test]
    fn host_input_in_selection_window_is_detected_without_more_writes() {
        let mut m = Mock::new(Change::HostInput, 5);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::SelectionNotRestored);
        assert_eq!(m.writes, 0);
        assert_eq!(m.host_replacements, 1);
        assert_eq!(m.fragment, "z");
    }
    #[test]
    fn config_only_cancel_collapses_owned_selection_without_new_capture() {
        let mut m = Mock::new(Change::Settings, 5);
        assert_eq!(execute(&mut m, "ghbdtn "), Outcome::SelectionRestored);
        assert_eq!(m.writes, 0);
        assert_eq!(m.restores, 1);
    }
}
