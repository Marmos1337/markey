//! Debug helper: show exactly why the detector reached its verdict.
//!
//! `cargo run --example explain -- <active-layout> <word> [layouts...]`
//!
//! Prefix the word with `=` to give the *intended* text instead, e.g.
//! `explain us =привет` types привет on the US keyboard first.

use rekey_core::config::Config;
use rekey_core::detect::Detector;
use rekey_core::model::Models;
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: explain <active-layout> <word>... ");
        std::process::exit(1);
    }
    let active = args[0].clone();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/models");
    let models = Models::load_dir(&dir).expect("models");
    let det = Detector::new(
        models,
        Config {
            layouts: vec!["us".into(), "ru".into()],
            ..Config::default()
        },
    );
    for raw in &args[1..] {
        let word = match raw.strip_prefix('=') {
            Some(intended) => {
                let from = det.source_layout(intended, "ru").unwrap().def.id;
                let to = if from == "us" { "ru" } else { "us" };
                rekey_core::convert_by_id(intended, from, to).unwrap()
            }
            None => raw.clone(),
        };
        let verdict = det.evaluate(&word, &active);
        match verdict.candidate() {
            Some(c) => println!(
                "{word:<14} -> {:<14} {:<10} typed={:>6.2} conv={:>6.2} delta={:>5.2} tk={} sk={} typo={:?}",
                c.converted,
                if verdict.is_switch() { "SWITCH" } else { "ambiguous" },
                c.score_as_typed,
                c.score_converted,
                c.delta,
                c.target_knows_word,
                c.source_knows_word,
                c.target_typo_of
            ),
            None => println!("{word:<14} -> kept: {verdict:?}"),
        }
    }
}
