# Data licences

## Language models (`crates/rekey-core/data/models/*.rkm`)

- **Source:** [hermitdave/FrequencyWords](https://github.com/hermitdave/FrequencyWords), `content/2018/<lang>/<lang>_50k.txt` and, for en, ru and uk, `<lang>_full.txt`. These are word frequencies counted over the OpenSubtitles 2018 corpus.
- **Licence of the source:** the repository README says "MIT License for code. CC-by-sa-4.0 for content." The word lists are content, so they are **CC BY-SA 4.0** (https://creativecommons.org/licenses/by-sa/4.0/).
- **What MarKey ships:** hashed word and trigram scores computed from those lists by `tools/modelgen`. This is adapted material, so it is distributed under CC BY-SA 4.0 with this attribution: "Word frequencies © Hermit Dave, FrequencyWords, from the OpenSubtitles 2018 corpus, CC BY-SA 4.0. Adapted by MarKey (hashed scoring tables)."
- `tools/fetch_corpora.sh` downloads the lists; they are not committed.

Before 01.11.2026 the README and `fetch_corpora.sh` called the lists "MIT". That was wrong for the content and has been corrected.
