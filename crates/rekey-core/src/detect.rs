//! The decision engine: given a token and the layout it was typed with, decide
//! whether the user meant it on a different layout.
//!
//! The mechanic is simple — re-type the token on every enabled layout and
//! score each reading — but almost all of the value is in the guards. A
//! switcher that is right 95% of the time is worse than useless, because the 5%
//! lands in the middle of a password field or a git command. Everything here is
//! biased toward doing nothing when the evidence is less than clear.
//!
//! A *token* is everything typed between two pieces of whitespace, so it can
//! carry punctuation: `ghbdtn,` is `привет,` typed on English, and `руддщю` is
//! `hello.` typed on Russian. Punctuation is converted along with the letters —
//! it was typed on the same wrong keyboard — and then set aside, so only the
//! word itself is judged.

use crate::config::Config;
use crate::layout::{self, Layout};
use crate::model::{Model, Models};

/// A rejected or accepted alternative reading, with the reasoning attached so
/// the UI can explain itself instead of feeling like magic.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub to_layout: &'static str,
    /// The full replacement for the token, punctuation included.
    pub converted: String,
    /// log10 score of the text as typed, in the active layout's language.
    pub score_as_typed: f32,
    /// log10 score of the converted reading, in the target layout's language.
    pub score_converted: f32,
    /// How much better the alternative is. Positive means "probably a mistake".
    pub delta: f32,
    pub target_knows_word: bool,
    pub source_knows_word: bool,
    /// Set when the corrected reading is not a word, but is one letter away
    /// from a real one — carrying that word's log10 probability.
    pub target_typo_of: Option<f32>,
}

impl Candidate {
    /// Squash the score advantage into a 0..1 confidence for display.
    pub fn confidence(&self) -> f32 {
        1.0 / (1.0 + (-(self.delta - 1.5)).exp())
    }
}

/// Why the detector declined to act. Surfaced in the debug log, which is the
/// difference between "it randomly does nothing" and a tool people trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    Disabled,
    TooShort,
    AllCaps,
    UserException,
    /// Contains digits or symbols that no layout turns into a letter: paths,
    /// handles, passwords.
    NotAWord,
    /// No other layouts configured.
    NoAlternatives,
    /// No trained model for one of the languages involved.
    ModelMissing,
    /// The alternative reading was not convincing enough.
    NotConfident,
    /// Conversion produced characters the target layout cannot type.
    Unconvertible,
}

#[derive(Debug, Clone)]
pub enum Verdict {
    /// Leave the text alone.
    Keep(Skip),
    /// Replace the typed token with `best.converted`.
    Switch(Box<Candidate>),
    /// Plausible but under threshold. With AI assist on, this is what gets
    /// escalated; otherwise it is treated as Keep.
    Ambiguous(Box<Candidate>),
}

impl Verdict {
    pub fn is_switch(&self) -> bool {
        matches!(self, Verdict::Switch(_))
    }

    pub fn candidate(&self) -> Option<&Candidate> {
        match self {
            Verdict::Switch(c) | Verdict::Ambiguous(c) => Some(c),
            Verdict::Keep(_) => None,
        }
    }
}

/// What the surrounding text says about the language being written.
///
/// One word on its own is often genuinely ambiguous — `vs` is English, and it
/// is also `мы` typed on the wrong keyboard. The word before it usually is not.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Hint<'a> {
    /// Language of the previous word in the same run of typing, when it was
    /// settled with confidence.
    pub previous_lang: Option<&'a str>,
}

pub struct Detector {
    pub models: Models,
    pub config: Config,
}

/// How much of a word must be typeable on the target layout for the conversion
/// to be considered meaningful rather than mangling.
const MIN_COVERAGE: f32 = 0.99;

/// Shortest word that may be matched through a typo.
///
/// Below this almost everything is one edit from something, so a match stops
/// being evidence of anything.
const MIN_TYPO_LEN: usize = 3;

/// Below this log10 frequency a corpus "word" is treated as a possible typo
/// of a commoner word, and both readings are considered.
const RARE_FORM: f32 = -5.5;

/// How much commoner the word behind a converted slip must be than the word
/// behind the typed one.
const SLIP_TIE: f32 = 0.5;

/// How common the word behind a three-letter slip must be.
const SHORT_TYPO_FLOOR: f32 = -3.3;

/// A typo match only counts when the word it corrects to is genuinely common.
/// The corpus tail is easy to land beside by accident.
const TYPO_COMMON_FLOOR: f32 = -5.0;

/// How much less likely a word with one letter wrong is than the same word
/// spelled correctly, in log10 units.
const TYPO_SCORE_PENALTY: f32 = -1.5;

/// Band below the switch threshold that counts as "worth asking the AI about".
const AMBIGUOUS_BAND: f32 = 1.5;

/// Characters that mark a token as structure rather than prose — URLs, paths,
/// namespaced identifiers, Windows drive letters. None of them is reachable as
/// a letter on any layout Rekey ships, so rejecting them costs nothing.
const STRUCTURAL: &[char] = &['@', ':', '\\', '|', '<', '>', '{', '}', '=', '_', '+'];

/// A single `/` is allowed because it is the Arabic letter ظ on a US keyboard
/// and `.` on Russian, but two or more mean a path or URL.
const MAX_SLASHES: usize = 1;

/// log10 unigram probability above which a word counts as genuinely common.
///
/// The frequency lists have a long tail of subtitle noise — OCR errors, foreign
/// fragments, stray letter pairs — and treating those as "already a real word"
/// would hand them the full anti-clobbering surcharge and block legitimate
/// corrections. Real vocabulary sits comfortably above this line.
const COMMON_WORD_FLOOR: f32 = -5.5;

/// Fraction of the surcharge granted to a word that is in the corpus but below
/// the floor: some protection, since it might be a rare real word, but not
/// enough to veto strong evidence.
const RARE_WORD_PROTECTION: f32 = 0.4;

/// When both readings are real words, how many times more common (in log10
/// units) the converted one must be before the typed one stops being
/// protected.
///
/// This is what lets `lf` become `да`. Subtitle corpora are full of short
/// letter pairs — "lf", "ye", "ds", "dj" — that are in the English list only
/// because someone, somewhere, typed Russian on the wrong keyboard, or because
/// they are initials. Shielding them as English words meant the commonest
/// Russian words (да, ну, вы, во, из, тут, ещё) were never corrected at all.
/// A hundredfold difference in frequency is decisive; anything closer is left
/// to context.
const DOMINANT_READING: f32 = 1.5;

/// …and only for typed words that are uncommon in their own right. `руку` is
/// `here` on the other keyboard, and both are everyday words; that one has to
/// be settled by context, never by frequency alone.
const DOMINATED_CEILING: f32 = -4.5;

/// Shift of the switching threshold when the previous word settles which
/// language is being written.
const CONTEXT_BIAS: f32 = 1.0;

/// A reading that keeps the typed trailing punctuation as punctuation, rather
/// than converting it, is a guess about intent; it costs this much.
const LITERAL_TAIL_PENALTY: f32 = 0.5;

/// Punctuation that ends a word in running text, set aside before the typed
/// word is judged.
const SENTENCE_PUNCTUATION: &[char] = &[',', '.', '!', '?', ';', ':', ')', '"', '\'', '»', '…'];

/// How common a two- or three-letter reading must be to count as a word.
const SHORT_WORD_FLOOR: f32 = -5.5;

/// Threshold relief when the converted reading is a dictionary word and the
/// typed text is not: an exact hit against gibberish is strong evidence even
/// when the word itself is uncommon.
const KNOWN_TARGET_RELIEF: f32 = 0.5;

/// Extra evidence demanded when converting a real typed word would turn some of
/// its letters into punctuation.
const LETTER_LOSS_PENALTY: f32 = 1.0;

/// A dictionary word in the target language scores at least this, however
/// rare it is, when the typed text is not a word at all.
///
/// Unigram probability and letter-shape plausibility are different scales.
/// Comparing them raw meant a real but uncommon word — `напишешь`,
/// `subscription` — lost to the shape score of the gibberish it was typed as,
/// so most of the vocabulary beyond the commonest few thousand words was
/// never corrected. Being in the dictionary is itself the evidence.
const KNOWN_WORD_FLOOR: f32 = -6.0;

/// One-letter words, by language. A lone letter outside this list is not a
/// word, and if the other keyboard turns it into one, it was mistyped.
const ONE_LETTER_WORDS: &[(&str, &[&str])] = &[
    ("en", &["a", "i"]),
    ("ru", &["а", "б", "в", "ж", "и", "к", "о", "с", "у", "я"]),
    ("uk", &["а", "в", "є", "з", "й", "і", "о", "у", "я"]),
];

/// English contractions: `don't` is `don` + `'t`. The word lists split them, so
/// a contraction is scored as its stem.
const CONTRACTION_SUFFIXES: &[&str] = &["t", "s", "m", "re", "ve", "ll", "d"];

/// How a reading of the letters scored in one language.
#[derive(Debug, Clone, Copy)]
struct Reading {
    score: f32,
    /// Exact dictionary hit, with its log10 probability.
    known: Option<f32>,
    typo_of: Option<f32>,
}

/// Split `s` into leading punctuation, the word, and trailing punctuation.
///
/// The word may contain an apostrophe or a hyphen between letters (`don't`,
/// `кто-то`) but nothing else that is not a letter. Returns `None` when there
/// is no such word in the middle.
fn split_word(s: &str) -> Option<(&str, &str, &str)> {
    let start = s.char_indices().find(|(_, c)| c.is_alphabetic())?.0;
    let end = s
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_alphabetic())
        .map(|(i, c)| i + c.len_utf8())?;
    let (lead, mid, trail) = (&s[..start], &s[start..end], &s[end..]);
    let joiners_ok = mid
        .chars()
        .all(|c| c.is_alphabetic() || c == '-' || c == '\'' || c == '’');
    let lone_joiners = mid.contains("--") || mid.contains("''");
    if !joiners_ok || lone_joiners {
        return None;
    }
    if lead
        .chars()
        .chain(trail.chars())
        .any(|c| c.is_alphanumeric())
    {
        return None;
    }
    Some((lead, mid, trail))
}

fn is_all_caps(word: &str) -> bool {
    word.chars().filter(|c| c.is_alphabetic()).count() > 1
        && word.chars().any(|c| c.is_uppercase())
        && word.chars().all(|c| !c.is_lowercase())
}

impl Detector {
    pub fn new(models: Models, config: Config) -> Detector {
        Detector { models, config }
    }

    /// Decide what to do about `token`, typed while `active` was the layout.
    pub fn evaluate(&self, token: &str, active: &str) -> Verdict {
        self.evaluate_with(token, active, Hint::default())
    }

    /// [`Detector::evaluate`] with what is known about the surrounding text.
    pub fn evaluate_with(&self, token: &str, active: &str, hint: Hint<'_>) -> Verdict {
        if !self.config.enabled {
            return Verdict::Keep(Skip::Disabled);
        }
        let Some(from) = self.source_layout(token, active) else {
            return Verdict::Keep(Skip::ModelMissing);
        };
        if let Some(skip) = self.prefilter(token, from) {
            return Verdict::Keep(skip);
        }
        let Some(src_model) = self.models.get(from.def.lang) else {
            return Verdict::Keep(Skip::ModelMissing);
        };
        let alternatives = self.config.alternatives(from.def.id);
        if alternatives.is_empty() {
            return Verdict::Keep(Skip::NoAlternatives);
        }

        // The word as typed. Trailing punctuation is set aside — `hello,` is
        // the English word `hello` — but leading characters that some layout
        // turns into a letter stay: nobody writes `,s` in English, while `бы`
        // typed on the wrong keyboard is exactly that. Punctuation in mid-word
        // stays in too: `cgfcb,j` is not an English word.
        let alternatives_for_trim = self.config.alternatives(from.def.id);
        let lead_trimmed = token.trim_start_matches(|c: char| {
            !c.is_alphabetic() && !self.is_letter_capable(c, from, &alternatives_for_trim)
        });
        let sentence_trimmed =
            lead_trimmed.trim_end_matches(|c: char| SENTENCE_PUNCTUATION.contains(&c));
        // Only when a real word is left behind: `j,` is not `j` and a comma.
        let typed_core = if sentence_trimmed
            .chars()
            .filter(|c| c.is_alphabetic())
            .count()
            >= 2
        {
            sentence_trimmed
        } else {
            lead_trimmed
        };
        // Keys that are letters on another layout count as letters: `nt,,z`
        // is five keystrokes of `теббя`, not three letters and some commas.
        let typed_letters = typed_core
            .chars()
            .filter(|c| {
                c.is_alphabetic() || self.is_letter_capable(*c, from, &alternatives_for_trim)
            })
            .count();
        // Typed text gets the same benefit of the doubt as its conversion: a
        // Russian word with a slip in it is still Russian, not English.
        let mut typed = self.read(src_model, typed_core, None);
        // What the typed text would be a slip of, if it is not a word itself.
        // Used only to settle slip-against-slip below, never to score.
        let typed_slip_of = if typed.known.is_none_or(|lp| lp < RARE_FORM)
            && typed_core.chars().count() >= MIN_TYPO_LEN
            && typed_core.chars().all(char::is_alphabetic)
        {
            src_model.best_within_one_edit(&crate::model::normalize(typed_core), &from.alphabet())
        } else {
            None
        };
        // A lone letter is not much of a word to protect: `j,` is `об`.
        if typed_core.chars().count() == 1 {
            typed.known = None;
        }
        let score_as_typed = typed.score;
        let source_knows = typed.known.is_some();

        let mut best: Option<Candidate> = None;
        let mut saw_convertible = false;

        for alt in alternatives {
            let Some(to) = layout::layout(alt) else {
                continue;
            };
            let Some(dst_model) = self.models.get(to.def.lang) else {
                continue;
            };
            for cand in self.readings(token, from, to, dst_model) {
                saw_convertible = true;
                let (converted, reading, penalty) = cand;
                let mut score_converted = reading.score - penalty;
                if reading.known.is_some()
                    && !source_knows
                    && typed_letters >= self.config.require_dict_below_len
                {
                    score_converted = score_converted.max(KNOWN_WORD_FLOOR - penalty);
                }
                let c = Candidate {
                    to_layout: to.def.id,
                    converted,
                    score_as_typed,
                    score_converted,
                    delta: score_converted - score_as_typed,
                    target_knows_word: reading.known.is_some(),
                    source_knows_word: source_knows,
                    target_typo_of: reading.typo_of,
                };
                if best.as_ref().is_none_or(|b| c.delta > b.delta) {
                    best = Some(c);
                }
            }
        }

        if !saw_convertible {
            return Verdict::Keep(Skip::Unconvertible);
        }
        let Some(best) = best else {
            return Verdict::Keep(Skip::NotConfident);
        };
        // A single letter is decided by a rule of its own: letter statistics
        // say nothing about one character, but whether it is a word does.
        if token.chars().filter(|c| c.is_alphabetic()).count() == 1 && typed_letters <= 1 {
            return self.one_letter(token, from, Box::new(best), hint);
        }
        let to = layout::layout(best.to_layout).expect("candidate layout exists");
        let target_known = self.models.get(to.def.lang).and_then(|m| {
            split_word(&best.converted).and_then(|(_, w, _)| self.read(m, w, Some(to)).known)
        });

        // -- thresholds -----------------------------------------------------
        let mut required = self.config.sensitivity.threshold();

        // Rewriting something that is already a valid word is the most
        // damaging mistake this app can make, so known words get a steep
        // surcharge — graded by how common the word actually is, so that
        // corpus-tail noise cannot veto an otherwise obvious correction.
        let surcharge = self.config.sensitivity.known_word_surcharge();
        let mut protection = match typed.known {
            Some(lp) if lp >= COMMON_WORD_FLOOR => surcharge,
            Some(_) => surcharge * RARE_WORD_PROTECTION,
            None => 0.0,
        };
        // A conversion that turns typed letters into punctuation (`шею` into
        // `in.`) is suspicious when the typed letters were a word already.
        let converted_letters = split_word(&best.converted)
            .map(|(_, w, _)| w.chars().filter(|c| c.is_alphabetic()).count())
            .unwrap_or(0);
        let loses_letters = converted_letters < typed_letters;
        // …unless the other reading is overwhelmingly the commoner word.
        let context_agrees_with_typed = hint.previous_lang == Some(from.def.lang);
        if let (Some(typed_lp), Some(target_lp), false) =
            (typed.known, target_known, context_agrees_with_typed)
        {
            if target_lp - typed_lp >= DOMINANT_READING
                && typed_lp <= DOMINATED_CEILING
                && !loses_letters
            {
                protection = 0.0;
                required = required.min(DOMINANT_READING);
            }
        }
        if source_knows && loses_letters {
            protection += LETTER_LOSS_PENALTY;
        }
        required += protection;

        // Neither reading is a known word: this is a guess from letter shape
        // alone, so demand much more before touching the user's text — unless
        // the reading is a real word with a single letter wrong, which is a
        // typing slip rather than nonsense.
        if !best.target_knows_word && best.target_typo_of.is_none() {
            required += self.config.sensitivity.unknown_target_surcharge();
        }
        let short = typed_letters < self.config.require_dict_below_len;
        if best.target_knows_word && !source_knows && !short {
            required -= KNOWN_TARGET_RELIEF;
        }

        // Latin-to-Latin pairs share an alphabet, so a "wrong layout" word
        // still looks like text and the evidence is inherently weaker.
        if from.def.latin_overlap && to.def.latin_overlap {
            required += 1.5;
        }

        // The previous word says which language this sentence is in.
        if let Some(lang) = hint.previous_lang {
            if lang == to.def.lang {
                required -= CONTEXT_BIAS;
            } else if lang == from.def.lang {
                required += CONTEXT_BIAS;
            }
        }

        // Both readings are a real word with a slip in it: `дле` (for `для`)
        // is `lkt` (for `let`) on the other keyboard. Then the commoner
        // word decides, and a tie goes to what was typed.
        if let (Some(typed_base), Some(target_base)) = (typed_slip_of, best.target_typo_of) {
            let target_is_slip = target_known.is_none_or(|lp| lp < RARE_FORM);
            if target_is_slip && typed_base + SLIP_TIE >= target_base {
                return Verdict::Keep(Skip::NotConfident);
            }
        }

        // Shouting is usually an acronym. Convert it only when the other
        // reading is a real word and the typed one is not anything at all.
        let len = typed_letters.max(1);
        if self.config.skip_all_caps
            && is_all_caps(token)
            && !(len >= 4 && best.target_knows_word && typed.known.is_none())
        {
            return Verdict::Keep(Skip::AllCaps);
        }

        // Short words carry little evidence: insist on a real, reasonably
        // common dictionary word. The corpus tail is full of two-letter noise.
        if short
            && !target_known.is_some_and(|lp| lp >= SHORT_WORD_FLOOR)
            && best.target_typo_of.is_none()
        {
            return Verdict::Keep(Skip::TooShort);
        }

        if best.delta >= required {
            Verdict::Switch(Box::new(best))
        } else if best.delta >= required - AMBIGUOUS_BAND {
            Verdict::Ambiguous(Box::new(best))
        } else {
            Verdict::Keep(Skip::NotConfident)
        }
    }

    /// Decide about a one-letter word.
    ///
    /// `z` is `я` typed on the wrong keyboard, and nobody writes a lone `z` in
    /// English — so it is converted. `a` and `I` are English words, `в`, `с`
    /// and `к` Russian ones, and those are left alone. A capital letter is
    /// usually a label or an initial (`B`, `C`, `R`), so it is converted only
    /// when the sentence around it is already in the other language — except
    /// `Z`, which is almost always `Я` starting a sentence.
    fn one_letter(
        &self,
        token: &str,
        from: &Layout,
        mut best: Box<Candidate>,
        hint: Hint<'_>,
    ) -> Verdict {
        let Some(typed) = token.chars().find(|c| c.is_alphabetic()) else {
            return Verdict::Keep(Skip::TooShort);
        };
        let Some(to) = layout::layout(best.to_layout) else {
            return Verdict::Keep(Skip::TooShort);
        };
        let Some((lead, word, trail)) = split_word(&best.converted)
            .map(|(l, w, t)| (l.to_string(), w.to_string(), t.to_string()))
        else {
            return Verdict::Keep(Skip::TooShort);
        };
        let Some(converted) = word.chars().next().filter(|_| word.chars().count() == 1) else {
            return Verdict::Keep(Skip::TooShort);
        };
        let is_word = |lang: &str, c: char| {
            let lower: String = c.to_lowercase().collect();
            ONE_LETTER_WORDS
                .iter()
                .any(|(l, words)| *l == lang && words.contains(&lower.as_str()))
        };
        if is_word(from.def.lang, typed) || !is_word(to.def.lang, converted) {
            return Verdict::Keep(Skip::TooShort);
        }
        if hint.previous_lang == Some(from.def.lang) {
            return Verdict::Keep(Skip::NotConfident);
        }
        let context_agrees = hint.previous_lang == Some(to.def.lang);
        if typed.is_uppercase() && !context_agrees && converted != 'Я' {
            return Verdict::Keep(Skip::AllCaps);
        }
        if self.config.is_exception(&typed.to_string()) {
            return Verdict::Keep(Skip::UserException);
        }
        // The English pronoun is always a capital.
        let shown = if to.def.lang == "en" && converted == 'i' {
            'I'
        } else {
            converted
        };
        best.converted = format!("{lead}{shown}{trail}");
        Verdict::Switch(best)
    }

    /// Every way `token` might read on `to`, as `(replacement, reading,
    /// penalty)`.
    ///
    /// The plain reading converts every keystroke. A second one keeps typed
    /// trailing punctuation as it is — `ghbdtn.` is far more likely `привет.`
    /// than `приветю` — but only when the plain reading is not a word, since
    /// `pyf.` really is `знаю`.
    fn readings(
        &self,
        token: &str,
        from: &Layout,
        to: &Layout,
        model: &Model,
    ) -> Vec<(String, Reading, f32)> {
        let mut out = Vec::new();
        let converted = layout::convert(token, from, to);
        let mut plain_known = false;
        // No language starts a word with `,` or `[`; a typed letter that comes
        // out as leading punctuation means the typed text was the word.
        let typed_lead = token.chars().take_while(|c| !c.is_alphabetic()).count();
        let lead_ok = |converted: &str| {
            converted.chars().take_while(|c| !c.is_alphabetic()).count() <= typed_lead
        };
        if converted != token && to.coverage(&converted) >= MIN_COVERAGE && lead_ok(&converted) {
            if let Some((_, word, _)) = split_word(&converted) {
                let reading = self.read(model, word, Some(to));
                plain_known = reading.known.is_some();
                out.push((converted.clone(), reading, 0.0));
            }
        }
        if plain_known {
            return out;
        }
        let chars: Vec<char> = token.chars().collect();
        for keep in 1..=2usize {
            if chars.len() <= keep + 1 {
                break;
            }
            let (head, tail) = chars.split_at(chars.len() - keep);
            if tail.iter().any(|c| c.is_alphanumeric()) {
                break;
            }
            let head: String = head.iter().collect();
            let tail: String = tail.iter().collect();
            let converted_head = layout::convert(&head, from, to);
            if converted_head == head || to.coverage(&converted_head) < MIN_COVERAGE {
                continue;
            }
            let replacement = format!("{converted_head}{tail}");
            if !lead_ok(&replacement) {
                continue;
            }
            if let Some((_, word, _)) = split_word(&replacement) {
                let reading = self.read(model, word, Some(to));
                if reading.known.is_some() || reading.typo_of.is_some() {
                    out.push((replacement, reading, LITERAL_TAIL_PENALTY));
                }
            }
        }
        out
    }

    /// Score a word (no surrounding punctuation) in one language.
    fn read(&self, model: &Model, word: &str, layout: Option<&Layout>) -> Reading {
        // Hyphenated compounds: every part has to stand up on its own.
        if word.contains('-') {
            let parts: Vec<Reading> = word
                .split('-')
                .filter(|p| !p.is_empty())
                .map(|p| self.read(model, p, layout))
                .collect();
            if parts.is_empty() {
                return Reading {
                    score: model.score(word),
                    known: None,
                    typo_of: None,
                };
            }
            let score = parts.iter().map(|r| r.score).sum::<f32>() / parts.len() as f32;
            let known = parts
                .iter()
                .map(|r| r.known)
                .collect::<Option<Vec<f32>>>()
                .map(|lps| lps.into_iter().fold(f32::INFINITY, f32::min));
            return Reading {
                score,
                known,
                typo_of: None,
            };
        }
        // English contractions are scored as their stem.
        if let Some(pos) = word.find(['\'', '’']) {
            let (stem, rest) = word.split_at(pos);
            let suffix = rest.trim_start_matches(['\'', '’']).to_lowercase();
            if model.lang == "en"
                && !stem.is_empty()
                && CONTRACTION_SUFFIXES.contains(&suffix.as_str())
            {
                return self.read(model, stem, layout);
            }
            return Reading {
                score: model.score(word),
                known: None,
                typo_of: None,
            };
        }

        let normalized = crate::model::normalize(word);
        let known = model.word_logp(&normalized);
        let mut score = model.score(word);
        // Not a word — but perhaps a real word with one letter wrong, which is
        // what an ordinary typing slip looks like.
        let len = normalized.chars().count();
        // A "word" this rare in the corpus is usually somebody's typo —
        // `когд`, `тперь` — so look for the real word next to it as well.
        let rare = known.is_none_or(|lp| lp < RARE_FORM);
        let typo_of = match (rare, layout) {
            (true, Some(layout)) if len >= MIN_TYPO_LEN => {
                // Three letters are one edit from a great many words, so only
                // the very commonest count: `vyz` is `мня`, a slip of `меня`.
                let floor = if len < 4 {
                    SHORT_TYPO_FLOOR
                } else {
                    TYPO_COMMON_FLOOR
                };
                model
                    .best_within_one_edit(&normalized, &layout.alphabet())
                    .filter(|logp| *logp >= floor)
            }
            _ => None,
        };
        // Score it as the word it is a slip of, discounted. Left as the
        // letter-shape estimate it would otherwise get, a mistyped real word
        // looks exactly like gibberish.
        let typo_of = typo_of.filter(|t| known.is_none_or(|k| t + TYPO_SCORE_PENALTY > k));
        if let Some(logp) = typo_of {
            score = score.max(logp + TYPO_SCORE_PENALTY);
        }
        Reading {
            score,
            known,
            typo_of,
        }
    }

    /// The layout `token` was actually typed on.
    ///
    /// Usually that is the active one. But the system's answer to "which
    /// keyboard is selected" lags a real switch by a few milliseconds, and a
    /// word typed right after one can arrive tagged with the old layout. The
    /// characters themselves settle it: Cyrillic letters did not come from an
    /// English keyboard.
    pub fn source_layout(&self, token: &str, active: &str) -> Option<&'static Layout> {
        let typed_by = |l: &Layout| {
            token
                .chars()
                .filter(|c| c.is_alphabetic())
                .all(|c| l.key_for(c).is_some() || l.dead_key_presses(c).is_some())
        };
        let current = layout::layout(active);
        if let Some(l) = current {
            if typed_by(l) {
                return Some(l);
            }
        }
        self.config
            .layouts
            .iter()
            .filter_map(|id| layout::layout(id))
            .find(|l| typed_by(l))
            .or(current)
    }

    /// Cheap rejections that need no models.
    ///
    /// The subtle part is punctuation. A word typed on the wrong layout is full
    /// of it — Russian `б` and `ю` live on the `,` and `.` keys, Ukrainian `ж`
    /// on `;` — so "letters only" would reject precisely the input this app
    /// exists to fix. Instead a non-letter is tolerated in mid-word when some
    /// enabled layout turns that physical key into a letter, and anywhere at
    /// the edges, where it is ordinary punctuation.
    fn prefilter(&self, token: &str, from: &Layout) -> Option<Skip> {
        if token
            .chars()
            .any(|c| c.is_numeric() || c.is_whitespace() || c.is_control())
        {
            return Some(Skip::NotAWord);
        }
        if token.chars().any(|c| STRUCTURAL.contains(&c)) {
            return Some(Skip::NotAWord);
        }
        if token.chars().filter(|c| *c == '/').count() > MAX_SLASHES {
            return Some(Skip::NotAWord);
        }
        // Command-line flags, hashtags, variables.
        if token.starts_with(['-', '#', '$']) {
            return Some(Skip::NotAWord);
        }

        let (_, core, _) = self.trim_affixes(token, from.def.id);
        let letters = core.chars().filter(|c| c.is_alphabetic()).count();
        if core.chars().count() < self.config.min_word_len || letters == 0 {
            return Some(Skip::TooShort);
        }
        let alternatives = self.config.alternatives(from.def.id);
        for c in core.chars() {
            if c.is_alphabetic() || c == '-' || c == '\'' {
                continue;
            }
            if !self.is_letter_capable(c, from, &alternatives) {
                return Some(Skip::NotAWord);
            }
        }
        if self.config.is_exception(core) || self.config.is_exception(token) {
            return Some(Skip::UserException);
        }
        None
    }

    /// True when the physical key behind `c` contributes to a letter on at
    /// least one of the layouts the user actually types in.
    ///
    /// Dead keys count. Greek marks stress on nearly every multisyllabic word,
    /// so `;` — which is the tonos key — appears inside the great majority of
    /// mistyped Greek words even though `΄` is a modifier symbol rather than a
    /// letter in its own right.
    fn is_letter_capable(&self, c: char, from: &Layout, alternatives: &[&str]) -> bool {
        let Some((idx, shifted)) = from.key_for(c) else {
            return false;
        };
        alternatives.iter().any(|alt| {
            layout::layout(alt).is_some_and(|to| {
                if to.is_dead_key(idx, shifted) {
                    return true;
                }
                let tok = to.token_at(idx, shifted);
                !tok.is_empty() && tok.chars().all(|ch| ch.is_alphabetic())
            })
        })
    }

    /// Split a typed token into `(prefix, core, suffix)`, where the affixes are
    /// punctuation that no enabled layout could turn into a letter.
    pub fn trim_affixes<'a>(&self, token: &'a str, active: &str) -> (&'a str, &'a str, &'a str) {
        let Some(from) = layout::layout(active) else {
            return ("", token, "");
        };
        let alternatives = self.config.alternatives(active);
        let is_affix =
            |c: char| !c.is_alphabetic() && !self.is_letter_capable(c, from, &alternatives);

        let start = token
            .char_indices()
            .find(|(_, c)| !is_affix(*c))
            .map(|(i, _)| i)
            .unwrap_or(token.len());
        let end = token
            .char_indices()
            .rev()
            .find(|(_, c)| !is_affix(*c))
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(start);
        (&token[..start], &token[start..end], &token[end..])
    }

    /// Convert `word` between two layouts unconditionally.
    pub fn force_convert(&self, word: &str, from: &str, to: &str) -> Option<String> {
        layout::convert_by_id(word, from, to)
    }

    /// How much better `token` reads on its best alternative layout than as
    /// typed, in log10 units, or `None` when it has no alternative reading.
    ///
    /// Positive means it looks mistyped. The manual shortcut uses this to find
    /// where a run of wrong-layout words begins.
    pub fn advantage(&self, token: &str, active: &str) -> Option<(f32, &'static str)> {
        let from = self.source_layout(token, active)?;
        let src = self.models.get(from.def.lang)?;
        let typed = src.score(token);
        let mut best: Option<(f32, &'static str)> = None;
        for alt in self.config.alternatives(from.def.id) {
            let Some(to) = layout::layout(alt) else {
                continue;
            };
            let Some(model) = self.models.get(to.def.lang) else {
                continue;
            };
            for (_, reading, penalty) in self.readings(token, from, to, model) {
                let delta = reading.score - penalty - typed;
                if best.is_none_or(|(b, _)| delta > b) {
                    best = Some((delta, to.def.id));
                }
            }
        }
        best
    }

    /// Best guess at which of the configured layouts `text` was *meant* for.
    pub fn best_target(&self, text: &str, active: &str) -> Option<&'static str> {
        let from = self.source_layout(text, active)?;
        let mut best: Option<(&'static str, f32)> = None;
        for alt in self.config.alternatives(from.def.id) {
            let Some(to) = layout::layout(alt) else {
                continue;
            };
            let Some(model) = self.models.get(to.def.lang) else {
                continue;
            };
            let converted = layout::convert(text, from, to);
            let words: Vec<&str> = converted.split_whitespace().collect();
            if words.is_empty() {
                continue;
            }
            let score = words
                .iter()
                .map(|w| match split_word(w) {
                    Some((_, word, _)) => model.score(word),
                    None => model.score(w),
                })
                .sum::<f32>()
                / words.len() as f32;
            if best.is_none_or(|(_, b)| score > b) {
                best = Some((to.def.id, score));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Convert a stretch of text — a selection, a whole phrase — to the layout
    /// it was meant for.
    ///
    /// Works out which layout typed it from the characters, so it does the
    /// right thing whichever way round the mistake was. Returns the converted
    /// text and the layout it is now in.
    pub fn convert_text(&self, text: &str, active: &str) -> Option<(String, &'static str)> {
        let from = self.text_layout(text, active)?;
        let target = self.best_target(text, from.def.id)?;
        let converted = layout::convert_by_id(text, from.def.id, target)?;
        (converted != text).then_some((converted, target))
    }

    /// Which enabled layout typed most of the letters in `text`.
    fn text_layout(&self, text: &str, active: &str) -> Option<&'static Layout> {
        let letters: Vec<char> = text.chars().filter(|c| c.is_alphabetic()).collect();
        if letters.is_empty() {
            return layout::layout(active);
        }
        let mut best: Option<(&'static Layout, usize)> = None;
        let mut ids: Vec<&str> = vec![active];
        ids.extend(self.config.layouts.iter().map(String::as_str));
        for id in ids {
            let Some(l) = layout::layout(id) else {
                continue;
            };
            let hits = letters.iter().filter(|c| l.key_for(**c).is_some()).count();
            if best.is_none_or(|(_, b)| hits > b) {
                best = Some((l, hits));
            }
        }
        best.map(|(l, _)| l)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_split_from_their_punctuation() {
        assert_eq!(split_word("привет,"), Some(("", "привет", ",")));
        assert_eq!(split_word("«да»"), Some(("«", "да", "»")));
        assert_eq!(split_word("don't"), Some(("", "don't", "")));
        assert_eq!(split_word("кто-то?"), Some(("", "кто-то", "?")));
        assert_eq!(split_word("a,b"), None);
        assert_eq!(split_word("..."), None);
    }

    #[test]
    fn shouting_needs_more_than_one_letter() {
        assert!(is_all_caps("HTTP"));
        assert!(!is_all_caps("I"));
        assert!(!is_all_caps("Http"));
    }
}
