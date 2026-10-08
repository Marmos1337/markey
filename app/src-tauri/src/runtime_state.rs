//! Ownership of asynchronous state changes, independently testable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Token {
    pub generation: u64,
    pub revision: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub token: Token,
    pub code: u64,
}
pub fn owns(source: Token, current: Token) -> bool {
    source == current
}
pub fn publish(status: &mut Status, source: Token, current: Token, code: u64) -> bool {
    if !owns(source, current) {
        return false;
    }
    *status = Status {
        token: source,
        code,
    };
    true
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_worker_cannot_replace_new_exclusion_or_settings_status() {
        let old = Token {
            generation: 3,
            revision: 1,
        };
        let current = Token {
            generation: 4,
            revision: 1,
        };
        let mut s = Status::default();
        assert!(publish(&mut s, current, current, 1));
        assert!(!publish(&mut s, old, current, 4));
        assert_eq!(s.code, 1);
        let settings = Token {
            generation: 4,
            revision: 2,
        };
        assert!(publish(&mut s, settings, settings, 0));
        assert!(!publish(&mut s, current, settings, 4));
        assert_eq!(s.code, 0);
    }
    #[test]
    fn obsolete_cancellation_preserves_new_word_in_real_core() {
        use rekey_core::{
            config::Config,
            engine::{Action, Engine},
            input::{Context, Key, KeyEvent, Modifiers},
            model::Models,
        };
        let models = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/rekey-core/data/models");
        let mut e = Engine::new(Models::load_dir(&models).unwrap(), Config::default());
        let ctx = Context {
            app: Some("com.apple.TextEdit".into()),
            layout: Some("us".into()),
            secure_input: false,
        };
        let key = |c| KeyEvent {
            character: Some(c),
            key: if c == ' ' { Key::Space } else { Key::Character },
            modifiers: Modifiers::default(),
            synthetic: false,
        };
        let stale = Token {
            generation: 7,
            revision: 1,
        };
        let current = Token {
            generation: 10,
            revision: 1,
        };
        // A real old replacement has been queued, then the next word begins
        // before that worker discovers its generation is obsolete.
        let mut pending = Action::None;
        for c in "ghbdtn ".chars() {
            pending = e.on_key(&key(c), &ctx);
        }
        assert!(matches!(pending, Action::Replace { .. }));
        for c in "ghb".chars() {
            e.on_key(&key(c), &ctx);
        }
        if owns(stale, current) {
            e.invalidate_caret();
        }
        let mut final_action = Action::None;
        for c in "dtn ".chars() {
            final_action = e.on_key(&key(c), &ctx);
        }
        assert!(matches!(final_action,Action::Replace {ref original,..} if original=="ghbdtn "));
    }
}
