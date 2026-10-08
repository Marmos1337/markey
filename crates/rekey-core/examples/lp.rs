use rekey_core::model::{normalize, Models};
fn main() {
    let models =
        Models::load_dir(&std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/models"))
            .unwrap();
    let args: Vec<String> = std::env::args().skip(1).collect();
    for w in &args[1..] {
        println!(
            "{w}: {:?}",
            models.get(&args[0]).unwrap().word_logp(&normalize(w))
        );
    }
}
