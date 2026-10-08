//! Precision/recall harness for the detector.
//!
//! Two questions, measured separately because they trade off against each
//! other and only one of them is career-limiting:
//!
//! * **Recall** — of words genuinely typed on the wrong layout, how many does
//!   it fix? Misses are annoying; the user has to reach for the shortcut.
//! * **Precision** — of words typed correctly, how many does it wreck? Every
//!   one of these is a small betrayal, so the thresholds are tuned to keep the
//!   false-positive rate near zero even at the cost of recall.
//!
//! Besides the classic table over each language's commonest words, the report
//! measures what a person actually experiences while typing:
//!
//! * **weighted** — every word counted as often as it is used, short words
//!   included. Missing `да` costs more than missing `экзистенциальный`.
//! * **tail** — words ranked 50,000–300,000: real vocabulary that a small word
//!   list does not contain.
//! * **punct** — words followed by a comma or a full stop, typed on the wrong
//!   keyboard together with their punctuation.
//!
//! Usage: `eval <corpus-dir> <models-dir> [sample-size]`

use rekey_core::config::{Config, Sensitivity};
use rekey_core::detect::{Detector, Verdict};
use rekey_core::model::Models;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Pairs we report on. Latin-to-Latin is included precisely because it is the
/// hard case and its numbers should be visibly worse.
const PAIRS: &[(&str, &str)] = &[
    ("us", "ru"),
    ("us", "uk"),
    ("us", "he"),
    ("us", "ar"),
    ("us", "el"),
    ("us", "de"),
    ("us", "fr"),
    ("us", "es"),
    ("us", "tr"),
];

/// Pairs that get the detailed, usage-weighted report.
const DETAILED: &[(&str, &str)] = &[("us", "ru"), ("us", "uk")];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: eval <corpus-dir> <models-dir> [sample-size]");
        return ExitCode::FAILURE;
    }
    let corpus = PathBuf::from(&args[1]);
    let models_dir = PathBuf::from(&args[2]);
    let sample: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3000);

    let models = match Models::load_dir(&models_dir) {
        Ok(m) if !m.is_empty() => m,
        Ok(_) => {
            eprintln!("no models in {}", models_dir.display());
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("cannot load models: {e}");
            return ExitCode::FAILURE;
        }
    };

    for sensitivity in [
        Sensitivity::Cautious,
        Sensitivity::Balanced,
        Sensitivity::Eager,
    ] {
        println!("\n=== {sensitivity:?} ===");
        println!(
            "{:<10} {:>8} {:>8} {:>10} {:>10}",
            "pair", "recall", "recall", "false-pos", "false-pos"
        );
        println!(
            "{:<10} {:>8} {:>8} {:>10} {:>10}",
            "", "A->B", "B->A", "in A", "in B"
        );

        let mut tot_recall = (0usize, 0usize);
        let mut tot_fp = (0usize, 0usize);

        for (a, b) in PAIRS {
            let det = detector(&models, a, b, sensitivity);
            let lang_a = rekey_core::layout(a).unwrap().def.lang;
            let lang_b = rekey_core::layout(b).unwrap().def.lang;
            let words_a = top_words(&corpus, lang_a, sample, 3);
            let words_b = top_words(&corpus, lang_b, sample, 3);

            let rec_ba = recall(&det, &words_b, b, a);
            let rec_ab = recall(&det, &words_a, a, b);
            let fp_a = false_positives(&det, &words_a, a);
            let fp_b = false_positives(&det, &words_b, b);

            println!(
                "{:<10} {:>7.1}% {:>7.1}% {:>9.2}% {:>9.2}%",
                format!("{a}<->{b}"),
                pct(rec_ba),
                pct(rec_ab),
                pct(fp_a),
                pct(fp_b),
            );
            tot_recall.0 += rec_ba.0 + rec_ab.0;
            tot_recall.1 += rec_ba.1 + rec_ab.1;
            tot_fp.0 += fp_a.0 + fp_b.0;
            tot_fp.1 += fp_a.1 + fp_b.1;
        }
        println!(
            "{:<10} {:>7.1}% (overall recall)   {:>7.2}% (overall false positives)",
            "TOTAL",
            pct(tot_recall),
            pct(tot_fp)
        );
    }

    println!("\n=== Balanced, as experienced while typing ===");
    for (a, b) in DETAILED {
        let det = detector(&models, a, b, Sensitivity::Balanced);
        let lang_a = rekey_core::layout(a).unwrap().def.lang;
        let lang_b = rekey_core::layout(b).unwrap().def.lang;
        let freq_a = frequencies(&corpus, lang_a, 20_000);
        let freq_b = frequencies(&corpus, lang_b, 20_000);
        for (lang_layout, other, freq) in [(b, a, &freq_b), (a, b, &freq_a)] {
            let (fixed, wrecked, misses, bad) = weighted(&det, freq, lang_layout, other);
            println!(
                "{other}->{lang_layout}  weighted recall {:>5.1}%   weighted false-pos {:>5.2}%",
                fixed * 100.0,
                wrecked * 100.0
            );
            if !misses.is_empty() {
                println!("    commonest misses: {}", misses.join(" "));
            }
            if !bad.is_empty() {
                println!("    commonest wrecks: {}", bad.join(" "));
            }
        }
        for (lang_layout, other, lang) in [(b, a, lang_b), (a, b, lang_a)] {
            let tail = tail_words(&corpus, lang, 50_000, 300_000, sample);
            if !tail.is_empty() {
                let r = recall(&det, &tail, lang_layout, other);
                let fp = false_positives(&det, &tail, lang_layout);
                println!(
                    "{other}->{lang_layout}  tail (rank 50k–300k) recall {:>5.1}%   false-pos {:>5.2}%",
                    pct(r),
                    pct(fp)
                );
            }
            let typos =
                typo_false_positives(&det, &top_words(&corpus, lang, sample, 4), lang_layout);
            println!(
                "{lang_layout}       typos typed correctly: wrongly converted {:>5.2}%",
                pct(typos)
            );
            let top = top_words(&corpus, lang, sample.min(2000), 2);
            let punct = punctuated_recall(&det, &top, lang_layout, other);
            println!(
                "{other}->{lang_layout}  with , and .  recall {:>5.1}%",
                pct(punct)
            );
        }
    }
    ExitCode::SUCCESS
}

fn detector(models: &Models, a: &str, b: &str, sensitivity: Sensitivity) -> Detector {
    let config = Config {
        layouts: vec![a.to_string(), b.to_string()],
        sensitivity,
        ..Config::default()
    };
    Detector::new(models.clone(), config)
}

fn pct((hit, total): (usize, usize)) -> f64 {
    if total == 0 {
        0.0
    } else {
        100.0 * hit as f64 / total as f64
    }
}

fn fixes(det: &Detector, mistyped: &str, typed_on: &str, intended: &str) -> bool {
    match det.evaluate(mistyped, typed_on) {
        Verdict::Switch(c) => c.converted == intended,
        _ => false,
    }
}

/// Words genuinely meant for `target_layout` but typed while `typed_on` was
/// active. Returns (fixed, attempted).
fn recall(det: &Detector, words: &[String], target_layout: &str, typed_on: &str) -> (usize, usize) {
    let mut fixed = 0;
    let mut total = 0;
    for w in words {
        let Some(mistyped) = rekey_core::convert_by_id(w, target_layout, typed_on) else {
            continue;
        };
        total += 1;
        if fixes(det, &mistyped, typed_on, w) {
            fixed += 1;
        }
    }
    (fixed, total)
}

/// The same, with sentence punctuation typed on the wrong keyboard too.
fn punctuated_recall(
    det: &Detector,
    words: &[String],
    target_layout: &str,
    typed_on: &str,
) -> (usize, usize) {
    let mut fixed = 0;
    let mut total = 0;
    for w in words.iter().filter(|w| w.chars().count() >= 3) {
        for p in [",", "."] {
            let intended = format!("{w}{p}");
            let Some(mistyped) = rekey_core::convert_by_id(&intended, target_layout, typed_on)
            else {
                continue;
            };
            total += 1;
            if fixes(det, &mistyped, typed_on, &intended) {
                fixed += 1;
            }
        }
    }
    (fixed, total)
}

/// Correctly typed words that the detector wrongly rewrites. Returns
/// (wrecked, attempted).
fn false_positives(det: &Detector, words: &[String], layout: &str) -> (usize, usize) {
    let bad = words
        .iter()
        .filter(|w| det.evaluate(w, layout).is_switch())
        .count();
    (bad, words.len())
}

/// Correct-layout words with an ordinary typing slip — a letter dropped,
/// doubled, swapped or replaced by its neighbour. These are not in any
/// dictionary, so they are where a lowered bar would bite.
fn typo_false_positives(det: &Detector, words: &[String], layout: &str) -> (usize, usize) {
    let mut bad = 0;
    let mut total = 0;
    for (i, w) in words.iter().enumerate() {
        let chars: Vec<char> = w.chars().collect();
        let n = chars.len();
        let k = i % n;
        let variants: Vec<String> = vec![
            // dropped
            chars
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != k)
                .map(|(_, c)| c)
                .collect(),
            // doubled
            chars
                .iter()
                .enumerate()
                .flat_map(|(j, c)| if j == k { vec![*c, *c] } else { vec![*c] })
                .collect(),
            // swapped with the next letter
            {
                let mut v = chars.clone();
                v.swap(k.min(n - 2), k.min(n - 2) + 1);
                v.into_iter().collect()
            },
        ];
        for v in variants {
            if v == *w {
                continue;
            }
            total += 1;
            if let Verdict::Switch(c) = det.evaluate(&v, layout) {
                bad += 1;
                if bad <= 12 && std::env::var_os("EVAL_VERBOSE").is_some() {
                    println!("    typo {v} -> {}", c.converted);
                }
            }
        }
    }
    (bad, total)
}

/// Usage-weighted recall and false-positive rate, with the commonest misses.
fn weighted(
    det: &Detector,
    freq: &[(String, u64)],
    lang_layout: &str,
    other: &str,
) -> (f64, f64, Vec<String>, Vec<String>) {
    let total: u64 = freq.iter().map(|(_, c)| c).sum();
    let mut fixed = 0u64;
    let mut wrecked = 0u64;
    let mut misses = Vec::new();
    let mut bad = Vec::new();
    for (w, c) in freq {
        let Some(mistyped) = rekey_core::convert_by_id(w, lang_layout, other) else {
            continue;
        };
        if fixes(det, &mistyped, other, w) {
            fixed += c;
        } else if misses.len() < 25 && w.chars().count() >= 2 {
            misses.push(w.clone());
        }
        if det.evaluate(w, lang_layout).is_switch() {
            wrecked += c;
            if bad.len() < 25 {
                bad.push(w.clone());
            }
        }
    }
    (
        fixed as f64 / total as f64,
        wrecked as f64 / total as f64,
        misses,
        bad,
    )
}

fn corpus_file(corpus: &Path, lang: &str) -> Option<String> {
    for name in [format!("{lang}_full.txt"), format!("{lang}_50k.txt")] {
        if let Ok(text) = std::fs::read_to_string(corpus.join(name)) {
            return Some(text);
        }
    }
    eprintln!("warning: no corpus for {lang}");
    None
}

fn parse(text: &str) -> impl Iterator<Item = (String, u64)> + '_ {
    text.lines().filter_map(|l| {
        let mut parts = l.split_whitespace();
        let w = rekey_core::model::normalize(parts.next()?);
        let c = parts.next()?.parse().ok()?;
        w.chars().all(|c| c.is_alphabetic()).then_some((w, c))
    })
}

fn top_words(corpus: &Path, lang: &str, n: usize, min_len: usize) -> Vec<String> {
    let Some(text) = corpus_file(corpus, lang) else {
        return Vec::new();
    };
    parse(&text)
        .map(|(w, _)| w)
        .filter(|w| w.chars().count() >= min_len)
        .take(n)
        .collect()
}

fn frequencies(corpus: &Path, lang: &str, n: usize) -> Vec<(String, u64)> {
    let Some(text) = corpus_file(corpus, lang) else {
        return Vec::new();
    };
    parse(&text).take(n).collect()
}

/// An evenly spaced sample of words between two frequency ranks.
fn tail_words(corpus: &Path, lang: &str, from: usize, to: usize, n: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(corpus.join(format!("{lang}_full.txt"))) else {
        return Vec::new();
    };
    let words: Vec<String> = parse(&text)
        .skip(from)
        .take(to - from)
        .map(|(w, _)| w)
        .filter(|w| w.chars().count() >= 4)
        .collect();
    let step = (words.len() / n.max(1)).max(1);
    words.into_iter().step_by(step).take(n).collect()
}
