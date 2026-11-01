//! The § key of ISO keyboards (MacBooks sold outside the US, external
//! keyboards with the extra key). The layout tables are process-wide, so this
//! lives in its own test binary.
//!
//! On a Mac the tables come from the system (`override_keys`): the ISO key is
//! table position 47, which the built-in tables leave empty. Here the system
//! tables are stood in for: US gives § / ±, Russian gives ё / Ё on that key.

use rekey_core::config::Config;
use rekey_core::detect::{Detector, Verdict};
use rekey_core::layout::{self, KEY_COUNT};
use rekey_core::model::Models;

fn with_iso_key(id: &str, lower: &str, upper: &str) {
    let table = layout::layout(id).unwrap();
    let mut lo: Vec<Option<String>> = (0..KEY_COUNT)
        .map(|i| Some(table.token_at(i, false).to_string()))
        .collect();
    let mut up: Vec<Option<String>> = (0..KEY_COUNT)
        .map(|i| Some(table.token_at(i, true).to_string()))
        .collect();
    lo[KEY_COUNT - 1] = Some(lower.to_string());
    up[KEY_COUNT - 1] = Some(upper.to_string());
    assert!(
        layout::override_keys(id, &lo, &up),
        "table for {id} refused"
    );
}

#[test]
fn the_iso_key_converts_like_any_other_key() {
    with_iso_key("us", "§", "±");
    with_iso_key("ru", "ё", "Ё");
    assert_eq!(
        layout::convert_by_id("§krf", "us", "ru").as_deref(),
        Some("ёлка")
    );
    assert_eq!(
        layout::convert_by_id("dc§", "us", "ru").as_deref(),
        Some("всё")
    );

    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/models");
    let Ok(models) = Models::load_dir(&dir) else {
        return;
    };
    if models.is_empty() {
        return;
    }
    let detector = Detector::new(
        models,
        Config {
            layouts: vec!["us".into(), "ru".into()],
            ..Config::default()
        },
    );
    // § inside a word is a letter on the other layout, not punctuation.
    for (typed, expected) in [("dc§", "всё"), ("t§", "её"), ("§krf", "ёлка")] {
        match detector.evaluate(typed, "us") {
            Verdict::Switch(c) => assert_eq!(c.converted, expected, "{typed}"),
            other => panic!("{typed}: {other:?}"),
        }
    }
}
