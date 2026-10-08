#!/usr/bin/env bash
# Download word-frequency lists used to train Rekey's language models.
#
# Source: hermitdave/FrequencyWords (MIT), built from the OpenSubtitles corpus.
# Subtitles are transcribed speech, so these lists are unusually rich in slang,
# contractions and profanity — exactly the vocabulary a layout switcher has to
# recognise and that dictionary-based word lists miss.
set -euo pipefail

BASE="https://raw.githubusercontent.com/hermitdave/FrequencyWords/master/content/2018"
OUT="$(dirname "$0")/.cache"
mkdir -p "$OUT"

LANGS=(en ru uk he ar el de fr es tr)
# Languages that get the full list rather than the top 50k: inflected forms
# fall off the end of a 50k list surprisingly early.
FULL=(en ru uk)

for lang in "${LANGS[@]}"; do
  dest="$OUT/${lang}_50k.txt"
  if [[ -s "$dest" ]]; then
    echo "cached   $lang ($(wc -l < "$dest" | tr -d ' ') lines)"
    continue
  fi
  if curl -fsS "$BASE/$lang/${lang}_50k.txt" -o "$dest"; then
    echo "fetched  $lang ($(wc -l < "$dest" | tr -d ' ') lines)"
  else
    echo "MISSING  $lang" >&2
    rm -f "$dest"
  fi
done

for lang in "${FULL[@]}"; do
  dest="$OUT/${lang}_full.txt"
  if [[ -s "$dest" ]]; then
    echo "cached   $lang full ($(wc -l < "$dest" | tr -d ' ') lines)"
    continue
  fi
  if curl -fsS "$BASE/$lang/${lang}_full.txt" -o "$dest"; then
    echo "fetched  $lang full ($(wc -l < "$dest" | tr -d ' ') lines)"
  else
    echo "MISSING  $lang full (falling back to 50k)" >&2
    rm -f "$dest"
  fi
done
