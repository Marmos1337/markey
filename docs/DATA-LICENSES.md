# Data licences

## Language models (`crates/rekey-core/data/models/*.rkm`)

- **Source:** [hermitdave/FrequencyWords](https://github.com/hermitdave/FrequencyWords), `content/2018/<lang>/<lang>_50k.txt` and, for en, ru and uk, `<lang>_full.txt`. These are word frequencies counted over the OpenSubtitles 2018 corpus.
- **Licence of the source:** the repository README says "MIT License for code. CC-by-sa-4.0 for content." The word lists are content, so they are **CC BY-SA 4.0** (https://creativecommons.org/licenses/by-sa/4.0/).
- **What MarKey ships:** hashed word and trigram scores computed from those lists by `tools/modelgen`. This is adapted material, so it is distributed under CC BY-SA 4.0 with this attribution: "Word frequencies © Hermit Dave, FrequencyWords, from the OpenSubtitles 2018 corpus, CC BY-SA 4.0. Adapted by MarKey (hashed scoring tables)."
- `tools/fetch_corpora.sh` downloads the lists; they are not committed.

Before 01.11.2026 the README and `fetch_corpora.sh` called the lists "MIT". That was wrong for the content and has been corrected.

## Extended dictionary (`crates/rekey-core/data/extended/{en,ru,uk}.txt`)

An optional word list (setting «Расширенный словарь», off by default): names, surnames, places, companies, brands, software, websites and new words that the 2018 subtitle corpus lacks. About 18 000 words, 256 KB. When it is on, these words count as real (rare) words: typed on the wrong keyboard they are converted, typed correctly they are left alone.

- **Wikidata labels** — Russian, Ukrainian and English labels of items of these classes with several Wikipedia articles each: given names (Q202444, Q12308941, Q11879590, Q3409032; 3+ sitelinks), family names (Q101352; 6+), countries and cities (Q6256, Q515, Q1549591, Q5119; 30+), brands and products (Q431289, Q167270, Q2424752; 6+), companies (Q4830453, Q783794, Q891723; 15+), software (Q7397, Q166142, Q1639024, Q9135; 6+), websites and online services (Q35127, Q1668024, Q2136985; 6+). Wikidata's structured data is **CC0 1.0** (https://www.wikidata.org/wiki/Wikidata:Licensing): no conditions. Fetched on 01.11.2026 by `tools/extended/fetch_wikidata.sh`.
- **`tools/extended/en.txt`, `tools/extended/ru.txt`** — new words and slang (кринж, нейросеть, маркетплейс, rizz, chatgpt...) written for MarKey by hand; **MIT**, like the code.

`tools/modelgen/src/bin/extgen.rs` builds the lists: it keeps single words of 3–24 letters that the base models do not know, and drops any word whose mistyped form on the other keyboard is a common word there, so the dictionary never blocks a real correction. `crates/rekey-core/tests/extended_dictionary.rs` checks that on the 20 000 commonest Russian and English words, typed both correctly and on the wrong keyboard, at most 5 decisions change.

Rebuild:
```sh
./tools/extended/fetch_wikidata.sh
cargo run --release -p modelgen --bin extgen -- tools/.cache crates/rekey-core/data/models crates/rekey-core/data/extended
```
