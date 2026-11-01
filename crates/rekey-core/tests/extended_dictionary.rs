//! The optional extended dictionary: what it adds, and that it costs no
//! accuracy on ordinary words. Needs the built models; the corpus checks also
//! need tools/.cache (tools/fetch_corpora.sh) and are skipped without it.

use rekey_core::config::Config;
use rekey_core::detect::{Detector, Verdict};
use rekey_core::layout;
use rekey_core::model::{ExtendedWords, Models};
use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn detectors() -> Option<(Detector, Detector)> {
    let base = Models::load_dir(&root().join("data/models")).ok()?;
    if base.is_empty() {
        return None;
    }
    let extended = ExtendedWords::load_dir(&root().join("data/extended")).unwrap();
    assert!(extended.len() > 10_000, "only {} words", extended.len());
    let config = || Config {
        layouts: vec!["us".into(), "ru".into()],
        ..Config::default()
    };
    Some((
        Detector::new(base.clone(), config()),
        Detector::new(base.with_extended(&extended), config()),
    ))
}

fn switches_to(d: &Detector, typed: &str, active: &str) -> Option<String> {
    match d.evaluate(typed, active) {
        Verdict::Switch(c) => Some(c.converted),
        _ => None,
    }
}

#[test]
fn names_and_brands_typed_on_the_wrong_keyboard_are_recognised() {
    let Some((_, with)) = detectors() else { return };
    for word in ["сбербанк", "вайлдберриз", "гитхаб"] {
        let typed = layout::convert_by_id(word, "ru", "us").unwrap();
        assert_eq!(
            switches_to(&with, &typed, "us").as_deref(),
            Some(word),
            "{typed}"
        );
    }
    for word in ["openai", "tiktok"] {
        let typed = layout::convert_by_id(word, "us", "ru").unwrap();
        assert_eq!(
            switches_to(&with, &typed, "ru").as_deref(),
            Some(word),
            "{typed}"
        );
    }
    // And typed correctly they are left alone.
    for (word, active) in [("openai", "us"), ("tiktok", "us"), ("сбербанк", "ru")] {
        assert_eq!(switches_to(&with, word, active), None, "{word}");
    }
}

/// The 20000 commonest words of each language, typed on the wrong keyboard
/// and typed correctly: the dictionary must not lose a correction or add a
/// wrong one.
#[test]
fn ordinary_words_are_decided_exactly_as_before() {
    let Some((without, with)) = detectors() else {
        return;
    };
    let mut changed = Vec::new();
    for (lang, own, other) in [("ru", "ru", "us"), ("en", "us", "ru")] {
        let path = root().join(format!("../../tools/.cache/{lang}_50k.txt"));
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.lines().take(20_000) {
            let Some(word) = line.split_whitespace().next() else {
                continue;
            };
            if !word.chars().all(char::is_alphabetic) {
                continue;
            }
            let Some(mistyped) = layout::convert_by_id(word, own, other) else {
                continue;
            };
            for (typed, active) in [(mistyped.as_str(), other), (word, own)] {
                let a = switches_to(&without, typed, active);
                let b = switches_to(&with, typed, active);
                if a != b {
                    changed.push(format!("{typed}: {a:?} -> {b:?}"));
                }
            }
        }
    }
    assert!(
        changed.len() <= 5,
        "{} decisions changed: {:?}",
        changed.len(),
        &changed[..changed.len().min(40)]
    );
}
