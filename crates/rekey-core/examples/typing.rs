//! Type whole phrases through the engine and show what ends up on screen.
//!
//! `cargo run --release --example typing -- [--hold] <layout> "<keys>" ...`
//!
//! Each argument is typed key by key on `layout` (the keyboard the user forgot
//! to switch). Prefix an argument with `=` to give the *intended* text instead:
//! `=я тут` is typed as the keys that produce it on the other layout. A `|`
//! taps the manual shortcut (right Command) at that point.

use rekey_core::config::Config;
use rekey_core::engine::{Action, Engine};
use rekey_core::input::{Context, Key, KeyEvent, Modifiers};
use rekey_core::model::Models;
use std::path::PathBuf;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let hold = args.first().is_some_and(|a| a == "--hold");
    if hold {
        args.remove(0);
    }
    let layout = args.remove(0);
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/models");
    let models = Models::load_dir(&dir).expect("models");
    for raw in args {
        let mut engine = Engine::new(
            models.clone(),
            Config {
                layouts: vec!["us".into(), "ru".into()],
                ..Config::default()
            },
        );
        engine.set_hold_boundaries(hold);
        let keys = match raw.strip_prefix('=') {
            Some(intended) => {
                let other = if layout == "us" { "ru" } else { "us" };
                rekey_core::layout::convert_by_id(intended, other, &layout).unwrap()
            }
            None => raw.clone(),
        };
        let mut screen = String::new();
        let mut current = layout.clone();
        for ch in keys.chars() {
            let ctx = Context {
                app: Some("test".into()),
                layout: Some(current.clone()),
                secure_input: false,
            };
            let mut events = Vec::new();
            if ch == '|' {
                let m = |down: bool| KeyEvent {
                    character: None,
                    key: Key::ModifiersChanged,
                    modifiers: Modifiers {
                        meta: down,
                        right_meta: down,
                        ..Default::default()
                    },
                    synthetic: false,
                };
                events.push((m(true), None));
                events.push((m(false), None));
            } else {
                // The user keeps pressing the same physical keys; once the
                // keyboard has been switched they produce the other layout.
                let typed =
                    rekey_core::layout::convert_by_id(&ch.to_string(), &layout, &current).unwrap();
                let c = typed.chars().next().unwrap();
                let key = match c {
                    ' ' => Key::Space,
                    '\n' => Key::Enter,
                    _ => Key::Character,
                };
                events.push((
                    KeyEvent {
                        character: Some(c),
                        key,
                        modifiers: Modifiers::default(),
                        synthetic: false,
                    },
                    Some(c),
                ));
            }
            for (ev, produced) in events {
                let action = engine.on_key(&ev, &ctx);
                let swallowed = hold && action.swallows_key();
                if let (Some(c), false) = (produced, swallowed) {
                    screen.push(c);
                }
                if let Action::Replace {
                    delete,
                    text,
                    then,
                    switch_to,
                    ..
                } = &action
                {
                    for _ in 0..*delete {
                        screen.pop();
                    }
                    screen.push_str(text);
                    if then.is_some() {
                        screen.push(produced.unwrap_or(' '));
                    }
                    if let Some(l) = switch_to {
                        current = l.clone();
                    }
                }
            }
        }
        println!("{raw:<34} -> {screen:?}");
    }
}
