#!/usr/bin/env bash
# Download the raw material for the extended dictionary from Wikidata.
#
# Wikidata's data is CC0 1.0 (public domain dedication,
# https://www.wikidata.org/wiki/Wikidata:Licensing), so the labels can be
# shipped with MarKey without conditions. Only labels are fetched: Russian,
# Ukrainian and English names of people's given names and surnames, places,
# companies, brands, software and websites, limited to items notable enough
# to have several Wikipedia articles.
#
# Output: tools/.cache/wikidata/<set>.tsv (label, lang). Then build the lists:
#   cargo run --release -p modelgen --bin extgen -- tools/.cache crates/rekey-core/data/models crates/rekey-core/data/extended
set -euo pipefail

OUT="$(dirname "$0")/../.cache/wikidata"
mkdir -p "$OUT"
ENDPOINT="https://query.wikidata.org/sparql"
AGENT="MarKey-dictionary-builder/0.3 (https://github.com/Marmos1337/MarKey)"

# $1 = set name, $2 = class VALUES, $3 = extra pattern, $4 = minimum sitelinks
fetch() {
  local name="$1" classes="$2" extra="$3" min="$4"
  local dest="$OUT/$name.tsv"
  if [[ -s "$dest" ]]; then
    echo "cached   $name ($(wc -l < "$dest" | tr -d ' ') lines)"
    return
  fi
  local query="SELECT DISTINCT ?label (LANG(?label) AS ?lang) WHERE {
    VALUES ?class { $classes }
    ?item wdt:P31 ?class ; wikibase:sitelinks ?links .
    FILTER(?links >= $min)
    $extra
    ?item rdfs:label ?label .
    FILTER(LANG(?label) IN (\"ru\", \"uk\", \"en\"))
  }"
  curl -fsS -m 300 -G "$ENDPOINT" --data-urlencode "query=$query" \
    -H "Accept: text/tab-separated-values" -A "$AGENT" -o "$dest.part"
  mv "$dest.part" "$dest"
  echo "fetched  $name ($(wc -l < "$dest" | tr -d ' ') lines)"
  sleep 2
}

# Given names: given name, male, female, unisex.
fetch given_names "wd:Q202444 wd:Q12308941 wd:Q11879590 wd:Q3409032" "" 3
fetch surnames "wd:Q101352" "" 6
# Places people write about: countries, cities, big towns.
fetch places "wd:Q6256 wd:Q515 wd:Q1549591 wd:Q5119" "" 30
# Companies, brands, products, software, websites, online services.
fetch brands "wd:Q431289 wd:Q167270 wd:Q2424752" "" 6
fetch companies "wd:Q4830453 wd:Q783794 wd:Q891723" "" 15
fetch software "wd:Q7397 wd:Q166142 wd:Q1639024 wd:Q9135" "" 6
fetch websites "wd:Q35127 wd:Q1668024 wd:Q2136985" "" 6
