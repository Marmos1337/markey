//! Turns `word count` frequency lists into Rekey's binary `.rkm` models.
//!
//! Usage: `modelgen <corpus-dir> <output-dir>`
//!
//! Every `<lang>_full.txt` (or, failing that, `<lang>_50k.txt`) in the corpus
//! directory becomes `<lang>.rkm`.
//!
//! The full lists matter. Fifty thousand word *forms* is a small vocabulary
//! for an inflected language — Russian alone spends most of it on the cases of
//! the commonest nouns — so ordinary words like `напишешь` fell outside it and
//! were judged on letter shape alone, which is exactly when the detector
//! declines to act.

use rekey_core::model::Model;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Words shorter than this carry almost no evidence and mostly add noise to the
/// trigram table, but they are still kept in the word table so the detector can
/// recognise "a", "я", "и" as real words and refuse to convert them.
const MIN_TRIGRAM_LEN: usize = 2;

/// Entries below this count are overwhelmingly OCR noise and foreign-script
/// fragments in the OpenSubtitles lists.
const MIN_COUNT: u64 = 3;

/// Upper bound on the vocabulary taken from a full list, most frequent first.
/// Keeps the bundled models to a few megabytes while still covering the long
/// tail of inflected forms people actually type.
const MAX_WORDS: usize = 400_000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: modelgen <corpus-dir> <output-dir>");
        return ExitCode::FAILURE;
    }
    let corpus = PathBuf::from(&args[1]);
    let out = PathBuf::from(&args[2]);

    let mut files: Vec<PathBuf> = match std::fs::read_dir(&corpus) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.ends_with("_50k.txt") || n.ends_with("_full.txt"))
            })
            .collect(),
        Err(e) => {
            eprintln!("cannot read corpus dir {}: {e}", corpus.display());
            return ExitCode::FAILURE;
        }
    };
    files.sort();
    // One list per language: the full one when it was fetched.
    let full: Vec<PathBuf> = files
        .iter()
        .filter(|p| p.to_string_lossy().ends_with("_full.txt"))
        .cloned()
        .collect();
    files.retain(|p| {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name.strip_suffix("_50k.txt") {
            Some(lang) => !full.iter().any(|f| {
                f.file_name().and_then(|n| n.to_str()) == Some(&format!("{lang}_full.txt"))
            }),
            None => true,
        }
    });

    if files.is_empty() {
        eprintln!("no *_50k.txt or *_full.txt files in {}", corpus.display());
        return ExitCode::FAILURE;
    }

    let mut total_bytes = 0u64;
    for path in files {
        let lang = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.split('_').next())
            .unwrap_or("??")
            .to_string();

        match build(&path, &lang) {
            Ok(model) => {
                let dest = out.join(format!("{lang}.rkm"));
                if let Err(e) = model.save(&dest) {
                    eprintln!("{lang}: cannot write {}: {e}", dest.display());
                    return ExitCode::FAILURE;
                }
                let size = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
                total_bytes += size;
                println!(
                    "{lang}: {:>6} words, {:>6} trigrams, {:>5} KB",
                    model.word_count(),
                    model.trigram_count(),
                    size / 1024
                );
            }
            Err(e) => {
                eprintln!("{lang}: {e}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!("total: {} KB", total_bytes / 1024);
    ExitCode::SUCCESS
}

fn build(path: &Path, lang: &str) -> std::io::Result<Model> {
    let text = std::fs::read_to_string(path)?;
    let mut counts: HashMap<String, u64> = HashMap::new();

    // The lists are sorted by frequency, so the cap keeps the commonest words.
    for line in text.lines().take(MAX_WORDS * 2) {
        let mut parts = line.split_whitespace();
        let (Some(word), Some(count)) = (parts.next(), parts.next()) else {
            continue;
        };
        let Ok(count) = count.parse::<u64>() else {
            continue;
        };
        if count < MIN_COUNT {
            continue;
        }
        let w = rekey_core::model::normalize(word);
        // Drop entries containing digits or punctuation: they are artefacts of
        // subtitle formatting, not vocabulary.
        if w.is_empty() || w.chars().any(|c| !c.is_alphabetic()) {
            continue;
        }
        if w.chars().count() < MIN_TRIGRAM_LEN && count < 1000 {
            continue;
        }
        *counts.entry(w).or_insert(0) += count;
        if counts.len() >= MAX_WORDS {
            break;
        }
    }

    // Words of everyday chat that subtitles under-represent: `спс`, `ок`,
    // `кринж`, `btw`. A word already more common in the corpus keeps its count.
    let supplement = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../supplement")
        .join(format!("{lang}.txt"));
    if let Ok(extra) = std::fs::read_to_string(&supplement) {
        let mut added = 0;
        for line in extra.lines().filter(|l| !l.starts_with('#')) {
            let mut parts = line.split_whitespace();
            let (Some(word), Some(count)) = (parts.next(), parts.next()) else {
                continue;
            };
            let Ok(count) = count.parse::<u64>() else {
                continue;
            };
            let w = rekey_core::model::normalize(word);
            let slot = counts.entry(w).or_insert(0);
            if *slot < count {
                *slot = count;
                added += 1;
            }
        }
        println!("{lang}: {added} words raised from the supplement");
    }

    if counts.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "no usable entries in corpus",
        ));
    }
    Ok(Model::train(lang, &counts))
}
