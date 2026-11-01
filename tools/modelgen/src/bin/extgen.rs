//! Builds the optional extended dictionary from Wikidata labels (CC0) and
//! MarKey's own hand-written lists (MIT).
//!
//!   cargo run --release -p modelgen --bin extgen -- tools/.cache crates/rekey-core/data/models crates/rekey-core/data/extended
//!
//! Inputs: `<cache>/wikidata/*.tsv` from tools/extended/fetch_wikidata.sh and
//! `tools/extended/<lang>.txt`. Output: `<out>/<lang>.txt`, one word per
//! line, sorted, with a header naming the sources and their licences.
//!
//! A word is kept only when it adds something and costs nothing:
//! * letters only (a hyphen or apostrophe inside is fine), 3 to 24 of them;
//! * the base model does not know it already;
//! * typed on the wrong keyboard, it is not a common word of the other
//!   language — otherwise knowing it would stop a real correction.

use rekey_core::layout;
use rekey_core::model::{normalize, Models};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The other reading must be rarer than this for a word to be added.
const BLOCKS_IF_COMMONER_THAN: f32 = -6.5;
const MIN_LEN: usize = 3;
const MAX_LEN: usize = 24;

/// (list, its keyboard layout, the layouts it could be mistyped on)
const LANGS: &[(&str, &str, &[&str])] = &[
    ("ru", "ru", &["us"]),
    ("uk", "uk", &["us"]),
    ("en", "us", &["ru", "uk"]),
];

fn script_of(word: &str) -> Option<&'static str> {
    let cyr = word
        .chars()
        .all(|c| !c.is_alphabetic() || ('\u{0400}'..='\u{04FF}').contains(&c));
    let lat = word
        .chars()
        .all(|c| !c.is_alphabetic() || c.is_ascii_alphabetic());
    match (cyr, lat) {
        (true, false) => Some("cyr"),
        (false, true) => Some("lat"),
        _ => None,
    }
}

fn usable(word: &str) -> bool {
    let n = word.chars().count();
    (MIN_LEN..=MAX_LEN).contains(&n)
        && word.chars().next().is_some_and(char::is_alphabetic)
        && word.chars().last().is_some_and(char::is_alphabetic)
        && word
            .chars()
            .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
}

/// Words of the labels in the TSVs, with the label's language.
fn wikidata(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "tsv") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let mut cols = line.split('\t');
            let (Some(label), Some(lang)) = (cols.next(), cols.next()) else {
                continue;
            };
            let label = label
                .rsplit_once('@')
                .map_or(label, |(l, _)| l)
                .trim_matches('"');
            let lang = lang.trim_matches('"');
            for word in label.split(|c: char| c.is_whitespace() || ",()«»\".:/".contains(c)) {
                out.push((word.to_string(), lang.to_string()));
            }
        }
    }
    out
}

fn hand_list(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: extgen <tools/.cache> <models dir> <out dir>");
        return ExitCode::FAILURE;
    }
    let cache = PathBuf::from(&args[1]);
    let models = match Models::load_dir(Path::new(&args[2])) {
        Ok(m) if !m.is_empty() => m,
        _ => {
            eprintln!("no models in {}; build them first", args[2]);
            return ExitCode::FAILURE;
        }
    };
    let out = PathBuf::from(&args[3]);
    if std::fs::create_dir_all(&out).is_err() {
        return ExitCode::FAILURE;
    }
    let labels = wikidata(&cache.join("wikidata"));
    let hand_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../extended");

    for &(list, keyboard, wrong_on) in LANGS {
        let Some(own) = layout::layout(keyboard).map(|l| l.def.lang) else {
            continue;
        };
        let Some(model) = models.get(own) else {
            continue;
        };
        let script = if list == "en" { "lat" } else { "cyr" };
        let mut candidates: Vec<String> = labels
            .iter()
            .filter(|(w, lang)| lang == list && script_of(w) == Some(script))
            .map(|(w, _)| w.clone())
            .collect();
        candidates.extend(hand_list(&hand_dir.join(format!("{list}.txt"))));

        let mut kept = BTreeSet::new();
        let (mut known, mut blocking, mut odd) = (0, 0, 0);
        for word in candidates {
            if !usable(&word) || script_of(&word) != Some(script) {
                odd += 1;
                continue;
            }
            let norm = normalize(&word);
            if model.knows(&norm) {
                known += 1;
                continue;
            }
            // Would this word, typed on the wrong keyboard, read as a common
            // word there? Then knowing it would shield that typing slip.
            let blocks = wrong_on.iter().any(|other_layout| {
                let Some(other) = layout::layout(other_layout).and_then(|l| models.get(l.def.lang))
                else {
                    return false;
                };
                layout::convert_by_id(&norm, keyboard, other_layout)
                    .and_then(|typed| other.word_logp(&normalize(&typed)))
                    .is_some_and(|lp| lp >= BLOCKS_IF_COMMONER_THAN)
            });
            if blocks {
                blocking += 1;
                continue;
            }
            kept.insert(norm);
        }
        let header = format!(
            "# MarKey extended dictionary: {list}. Generated by tools/modelgen/src/bin/extgen.rs.\n\
             # Sources: Wikidata labels (CC0 1.0, https://www.wikidata.org/wiki/Wikidata:Licensing)\n\
             # and tools/extended/{list}.txt (written for MarKey, MIT). See docs/DATA-LICENSES.md.\n"
        );
        let body: String = kept.iter().map(|w| format!("{w}\n")).collect();
        if std::fs::write(out.join(format!("{list}.txt")), header + &body).is_err() {
            return ExitCode::FAILURE;
        }
        println!(
            "{list}: {} words ({known} already known, {blocking} would block a correction, {odd} not usable)",
            kept.len()
        );
    }
    ExitCode::SUCCESS
}
