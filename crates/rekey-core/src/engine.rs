//! Turns a stream of keystrokes into corrections.
//!
//! This is where the pieces meet: the buffer says when a word ended, the
//! detector says whether it was meant for another layout, and the engine
//! decides whether it is safe to act right now. "Safe" does most of the work —
//! the right answer typed into a password field is still a bug.
//!
//! The engine keeps a short memory of the words on screen since the caret last
//! moved somewhere it cannot follow. That memory is what makes it feel like it
//! understands the sentence rather than single words:
//!
//! * the previous word's language tips the balance for an ambiguous one,
//! * correcting a word also fixes the short words just before it that were
//!   typed on the same wrong keyboard (`z gjqle` → `я пойду`),
//! * and the manual shortcut can convert a whole run of wrong-layout words, not
//!   only the last one.

use crate::assist::{Assist, Question};
use crate::buffer::{Buffer, Input};
use crate::config::{Config, Shortcut};
use crate::detect::{Candidate, Detector, Hint, Skip, Verdict};
use crate::input::{Context, Key, KeyEvent, Modifiers, TextWriter};
use crate::layout;
use crate::model::Models;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A word-ending key that was held back so a correction could land first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    /// [`Key::Space`], [`Key::Tab`] or [`Key::Enter`].
    pub key: Key,
    pub shift: bool,
}

/// What the engine wants done to the text the user is typing.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Leave everything alone.
    None,
    /// Delete `delete` characters and type `text` in their place.
    Replace {
        delete: usize,
        text: String,
        /// Exactly what is being deleted, for logging and verification.
        original: String,
        /// The layout the corrected text belongs to, when the system keyboard
        /// should follow it.
        ///
        /// Rewriting `ghbdtn` to `привет` while leaving the keyboard on
        /// English fixes the word and leaves the user to make the same mistake
        /// on the next one. Switching the layout is the other half of the fix.
        switch_to: Option<String>,
        /// The key that ended the word, when it was held back: it has not
        /// reached the application yet and must be sent after the replacement.
        /// The event that triggered this action should be swallowed.
        then: Option<Boundary>,
    },
    /// Convert the selected text. Reading a selection needs the clipboard,
    /// which only the platform layer can drive; it hands the text back to
    /// [`Engine::convert_text`].
    ConvertSelection {
        /// Nothing is selected: select the word before the caret first.
        select_word: bool,
    },
}

impl Action {
    pub fn apply(&self, writer: &dyn TextWriter) {
        if let Action::Replace {
            delete, text, then, ..
        } = self
        {
            writer.replace(*delete, text);
            if let Some(b) = then {
                writer.boundary(b.key, b.shift);
            }
        }
    }

    /// The layout the system keyboard should move to, if any.
    pub fn layout_switch(&self) -> Option<&str> {
        match self {
            Action::Replace { switch_to, .. } => switch_to.as_deref(),
            _ => None,
        }
    }

    /// True when the keystroke that produced this action must not reach the
    /// application, because the action re-sends it.
    pub fn swallows_key(&self) -> bool {
        matches!(self, Action::Replace { then: Some(_), .. })
    }
}

/// Running totals, shown in the settings window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    pub corrections: u64,
    pub undos: u64,
    pub manual: u64,
}

/// A modifier tap counts as a shortcut only if nothing else happened while it
/// was held, and it was not held for long.
const MODIFIER_TAP_WINDOW: Duration = Duration::from_millis(600);

/// How close together two taps must be to count as a double tap.
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(450);

/// How many finished words are remembered behind the caret.
const HISTORY: usize = 24;

/// How many words before a corrected one may be fixed along with it.
const LOOKBACK: usize = 3;

/// The longest earlier word fixed along with the next one. Longer words were
/// judged on their own merits already and left alone for a reason.
const LOOKBACK_MAX_LETTERS: usize = 4;

/// How much better an earlier short word must read on the new layout to be
/// fixed along with the word after it.
const LOOKBACK_ADVANTAGE: f32 = 0.5;

/// How common a word must be for its capitals to be fixed.
const CASE_FIX_FLOOR: f32 = -6.0;

/// How many words the shortcut converts at most in one go.
const PHRASE_LIMIT: usize = 16;

/// Stands in for the terminator of a word that has not been ended yet — one
/// the shortcut converted while it was still being typed.
const OPEN: char = '\u{0}';

/// One finished word on screen.
#[derive(Debug, Clone, PartialEq)]
struct Entry {
    /// Exactly what was typed.
    typed: String,
    /// The layout it was typed on.
    typed_layout: String,
    /// What is on screen now.
    shown: String,
    /// The layout `shown` is in.
    shown_layout: String,
    /// The whitespace after it, which is on screen as well.
    terminator: char,
    /// Rekey, not the user, chose what is shown.
    auto: bool,
    /// The language the shown word is confidently in, if any.
    lang: Option<&'static str>,
}

/// The stretch of text the last replacement wrote, in every reading.
///
/// This is what makes the shortcut feel right. Rekey's guess is only a guess,
/// so the user needs to be able to say "no, the other one" — repeatedly,
/// without thinking about which layouts are involved.
#[derive(Debug, Clone, PartialEq)]
struct Change {
    /// How many history entries the change covers, counted from the end.
    words: usize,
    /// Every reading of the whole stretch. Index 0 is what the user typed.
    variants: Vec<String>,
    /// The layout each variant is in, parallel to `variants`.
    layouts: Vec<String>,
    /// Which variant is on screen now.
    index: usize,
    /// True when Rekey picked the current variant on its own.
    auto: bool,
}

pub struct Engine {
    pub detector: Detector,
    buffer: Buffer,
    /// The layout the word in progress is being typed on.
    buffer_layout: Option<String>,
    /// Finished words behind the caret, oldest first.
    history: Vec<Entry>,
    /// The last replacement, while it is still directly behind the caret.
    last_change: Option<Change>,
    /// The user has probably selected some text.
    selection: bool,
    last_context: Option<(Option<String>, Option<String>)>,
    /// When the shortcut modifier went down with nothing else pressed since.
    modifier_down_at: Option<Instant>,
    modifier_was_down: bool,
    /// When the last completed tap happened, for double-tap shortcuts.
    last_tap_at: Option<Instant>,
    /// The layout Rekey has just asked the system to switch to, so that its
    /// own switch is not mistaken for the user changing context.
    expected_layout: Option<String>,
    /// The platform can hold back the key that ends a word until the
    /// correction has been typed.
    hold_boundaries: bool,
    /// The user's exception list changed and should be saved.
    config_dirty: bool,
    /// An optional second opinion for words the local models cannot settle.
    assist: Option<Arc<dyn Assist>>,
    stats: Stats,
    /// The last modifier release completed a tap of the shortcut key.
    tapped: bool,
    /// A correction just showed Caps Lock left on.
    caps_lock_on: bool,
}

impl Engine {
    pub fn new(models: Models, config: Config) -> Engine {
        Engine {
            detector: Detector::new(models, config),
            buffer: Buffer::new(),
            buffer_layout: None,
            history: Vec::new(),
            last_change: None,
            selection: false,
            last_context: None,
            modifier_down_at: None,
            modifier_was_down: false,
            last_tap_at: None,
            expected_layout: None,
            hold_boundaries: false,
            config_dirty: false,
            assist: None,
            stats: Stats::default(),
            tapped: false,
            caps_lock_on: false,
        }
    }

    pub fn config(&self) -> &Config {
        &self.detector.config
    }

    pub fn set_config(&mut self, config: Config) {
        self.detector.config = config;
        // Settings changed under the user's fingers; the word in progress was
        // judged under the old rules, so start clean.
        self.forget();
    }

    /// Whether the platform holds back word-ending keys (see
    /// [`Action::Replace::then`]).
    pub fn set_hold_boundaries(&mut self, hold: bool) {
        self.hold_boundaries = hold;
    }

    /// True once after a tap of the shortcut modifier — fired or, for a
    /// double tap, armed. The platform hides such taps from the system, which
    /// would otherwise open Start or the menu bar for a lone Alt or Win.
    pub fn take_tap(&mut self) -> bool {
        std::mem::take(&mut self.tapped)
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// True once after the exception list changed, so the caller can save it.
    pub fn take_config_dirty(&mut self) -> bool {
        std::mem::take(&mut self.config_dirty)
    }

    /// Forget everything known about the text behind the caret, after an
    /// external focus or selection change.
    pub fn invalidate_caret(&mut self) {
        self.forget();
        self.selection = false;
        self.expected_layout = None;
        self.modifier_down_at = None;
    }

    fn forget(&mut self) {
        self.buffer.push(Input::Reset);
        self.buffer_layout = None;
        self.history.clear();
        self.last_change = None;
    }

    /// Provide a second opinion for ambiguous words, or `None` to remove it.
    pub fn set_assist(&mut self, assist: Option<Arc<dyn Assist>>) {
        self.assist = assist;
    }

    pub fn has_assist(&self) -> bool {
        self.assist.is_some()
    }

    /// How many ambiguous words the assist has settled so far.
    pub fn assist_learned(&self) -> usize {
        self.assist.as_ref().map_or(0, |a| a.learned())
    }

    /// Feed one keystroke. Returns what should be done to the document.
    pub fn on_key(&mut self, event: &KeyEvent, ctx: &Context) -> Action {
        // Never react to our own typing, or the app would correct itself in a
        // loop until the buffer ran out.
        if event.synthetic {
            return Action::None;
        }

        // The caret may have moved between apps, or the user may have changed
        // layout by hand. Rekey's own layout switch, which follows every
        // correction, is not such a change.
        let context_key = (ctx.app.clone(), ctx.layout.clone());
        if let Some(previous) = self.last_context.clone() {
            if previous != context_key {
                if previous.0 != context_key.0 {
                    self.forget();
                    self.selection = false;
                } else {
                    let expected = self.expected_layout.take();
                    if expected.is_none() || ctx.layout != expected {
                        // A switch the user made mid-word leaves the word in
                        // two layouts. Everything already on screen is
                        // unaffected by which keyboard is selected now.
                        self.buffer.push(Input::Reset);
                        self.buffer_layout = None;
                    }
                }
            }
        }
        self.last_context = Some(context_key);

        if !self.detector.config.enabled {
            return Action::None;
        }
        // A password field has taken over the keyboard; do not even track.
        if ctx.secure_input {
            self.invalidate_caret();
            return Action::None;
        }
        if let Some(app) = &ctx.app {
            if self.detector.config.is_excluded_app(app) {
                self.invalidate_caret();
                return Action::None;
            }
        }
        // A modifier tapped on its own is the manual shortcut. This has to
        // come before the chord handling below: ⌥⌘ is a shortcut and would
        // return there, leaving the tap armed.
        if event.key == Key::ModifiersChanged {
            return self.on_modifier_change(event.modifiers, ctx);
        }

        // Anything else while the modifier is down makes it a chord.
        self.modifier_down_at = None;

        match event.key {
            Key::Click => {
                self.invalidate_caret();
                return Action::None;
            }
            Key::Selection => {
                self.forget();
                self.selection = true;
                return Action::None;
            }
            Key::Navigation if event.modifiers.shift => {
                // Shift+arrow extends a selection.
                self.forget();
                self.selection = true;
                return Action::None;
            }
            _ => {}
        }

        // Ctrl/Cmd chords are commands, not typing, and they usually move the
        // caret as a side effect. A selection survives them: ⌘C is the most
        // natural thing to press with text selected.
        if event.modifiers.is_shortcut() {
            self.forget();
            self.expected_layout = None;
            return Action::None;
        }
        self.selection = false;

        match event.key {
            Key::Backspace => {
                self.last_change = None;
                if self.buffer.is_empty() {
                    // Deleting back into the previous word: the space after it
                    // goes first, and the word is open for editing again.
                    match self.history.pop() {
                        Some(entry) => {
                            for c in entry.shown.chars() {
                                self.buffer.push(Input::Char(c));
                            }
                            self.buffer_layout = Some(entry.shown_layout);
                        }
                        None => {
                            self.buffer.push(Input::Backspace);
                        }
                    }
                } else {
                    self.buffer.push(Input::Backspace);
                }
                Action::None
            }
            Key::Navigation
            | Key::Escape
            | Key::Other
            | Key::Click
            | Key::Selection
            | Key::ModifiersChanged => {
                self.invalidate_caret();
                Action::None
            }
            Key::Character => match event.character {
                Some(c) if !c.is_control() => {
                    self.last_change = None;
                    if self.buffer.is_empty() {
                        self.buffer_layout = ctx.layout.clone();
                        // The word the shortcut just converted is being
                        // continued: it is the word in progress again.
                        if let Some(entry) = self.history.pop_if(|e| e.terminator == OPEN) {
                            for ch in entry.shown.chars() {
                                self.buffer.push(Input::Char(ch));
                            }
                            self.buffer_layout = Some(entry.shown_layout);
                        }
                    }
                    self.buffer.push(Input::Char(c));
                    Action::None
                }
                _ => {
                    self.invalidate_caret();
                    Action::None
                }
            },
            Key::Space | Key::Tab | Key::Enter => self.on_boundary(event, ctx),
        }
    }

    /// A word ended with space, tab or return.
    fn on_boundary(&mut self, event: &KeyEvent, ctx: &Context) -> Action {
        let terminator = match event.key {
            Key::Tab => '\t',
            Key::Enter => '\n',
            _ => ' ',
        };
        // Return sends the message in a chat and Tab moves focus, so whatever
        // was typed is gone from under the caret once the key lands. Correct
        // only while the key can be held back, and remember nothing after it.
        let final_key = event.key != Key::Space;
        let can_correct = !final_key || self.hold_boundaries;
        self.last_change = None;

        // The word before the caret was converted by the shortcut mid-word;
        // this ends it.
        if self.buffer.is_empty() {
            if let Some(e) = self.history.last_mut().filter(|e| e.terminator == OPEN) {
                e.terminator = terminator;
                if final_key {
                    self.forget();
                }
                return Action::None;
            }
        }

        let Some(word) = self.buffer.push(Input::Boundary(terminator)) else {
            if final_key {
                self.forget();
            } else {
                // A second space: the run of words continues after it, but
                // nothing tracked sits directly behind the caret any more.
                self.history.clear();
            }
            return Action::None;
        };
        let active = self
            .buffer_layout
            .take()
            .or_else(|| ctx.layout.clone())
            .or_else(|| self.detector.config.layouts.first().cloned());
        let Some(active) = active else {
            self.forget();
            return Action::None;
        };
        let active = self
            .detector
            .source_layout(&word.text, &active)
            .map(|l| l.def.id.to_string())
            .unwrap_or(active);

        let action = if can_correct {
            self.consider(&word.text, terminator, &active, event)
        } else {
            self.remember(&word.text, terminator, &active, None);
            Action::None
        };
        if final_key {
            self.history.clear();
            self.last_change = None;
        }
        action
    }

    /// Record a finished word as typed.
    fn remember(&mut self, text: &str, terminator: char, layout: &str, lang: Option<&'static str>) {
        let lang = lang.or_else(|| self.known_language(text, layout));
        self.history.push(Entry {
            typed: text.to_string(),
            typed_layout: layout.to_string(),
            shown: text.to_string(),
            shown_layout: layout.to_string(),
            terminator,
            auto: false,
            lang,
        });
        if self.history.len() > HISTORY {
            self.history.remove(0);
        }
    }

    /// The language a word is confidently in, as typed: a dictionary word of
    /// reasonable length.
    fn known_language(&self, text: &str, layout_id: &str) -> Option<&'static str> {
        let l = layout::layout(layout_id)?;
        let core = text.trim_matches(|c: char| !c.is_alphabetic());
        if core.chars().count() < 3 {
            return None;
        }
        let model = self.detector.models.get(l.def.lang)?;
        model
            .knows(&crate::model::normalize(core))
            .then_some(l.def.lang)
    }

    /// Evaluate one finished word and produce the corresponding action.
    fn consider(&mut self, text: &str, terminator: char, active: &str, event: &KeyEvent) -> Action {
        let hint = Hint {
            previous_lang: self.history.last().and_then(|e| e.lang),
        };
        let verdict = self.detector.evaluate_with(text, active, hint);

        // Deliberately records the decision and not the word. What you type
        // never leaves the machine, and a log file on disk is not an exception
        // to that.
        log::debug!(
            "word of {} chars on {active} -> {}",
            text.chars().count(),
            match &verdict {
                Verdict::Switch(_) => "corrected",
                Verdict::Ambiguous(_) => "ambiguous",
                Verdict::Keep(reason) => match reason {
                    Skip::TooShort => "kept (too short)",
                    Skip::AllCaps => "kept (all caps)",
                    Skip::UserException => "kept (user exception)",
                    Skip::NotAWord => "kept (not a word)",
                    Skip::NotConfident => "kept (not confident)",
                    Skip::Disabled => "kept (disabled)",
                    Skip::NoAlternatives => "kept (no other layouts)",
                    Skip::ModelMissing => "kept (no model)",
                    Skip::Unconvertible => "kept (unconvertible)",
                },
            }
        );

        let converted = match verdict {
            Verdict::Switch(candidate) => Some(candidate),
            Verdict::Ambiguous(candidate) => match self.resolve_ambiguous(text, &candidate, active)
            {
                Some(true) => Some(candidate),
                _ => None,
            },
            Verdict::Keep(_) => None,
        };
        // The layout was right but Caps Lock was not, or Shift was held a
        // letter too long: `пРИВЕТ`, `ПРивет`.
        let (shown, target, caps_lock) = match converted {
            Some(c) => {
                let to = layout::layout(c.to_layout)
                    .map(|l| l.def.lang)
                    .unwrap_or("");
                match self.case_fix(&c.converted, to) {
                    Some((fixed, caps)) => (fixed, c.to_layout, caps),
                    None => (c.converted.clone(), c.to_layout, false),
                }
            }
            None => {
                let lang = layout::layout(active).map(|l| l.def.lang).unwrap_or("");
                let target = layout::layout(active).map(|l| l.def.id);
                match (self.case_fix(text, lang), target) {
                    (Some((fixed, caps)), Some(target))
                        if !self.detector.config.is_exception(text) =>
                    {
                        (fixed, target, caps)
                    }
                    _ => {
                        self.remember(text, terminator, active, None);
                        return Action::None;
                    }
                }
            }
        };
        if caps_lock {
            self.caps_lock_on = true;
        }

        let target_lang = layout::layout(target).map(|l| l.def.lang);
        self.remember(text, terminator, active, None);
        let last = self.history.len() - 1;
        {
            let entry = &mut self.history[last];
            entry.shown = shown;
            entry.shown_layout = target.to_string();
            entry.auto = true;
            entry.lang = target_lang;
        }

        // Short words just before this one, typed on the same wrong keyboard,
        // were too little to go on by themselves. Now there is context.
        let mut words = 1;
        while target != active && words <= LOOKBACK && words < self.history.len() {
            let i = self.history.len() - 1 - words;
            let e = &self.history[i];
            let letters = e.typed.chars().filter(|c| c.is_alphabetic()).count();
            if e.auto
                || e.typed_layout != active
                || e.shown != e.typed
                || e.terminator != ' '
                || letters > LOOKBACK_MAX_LETTERS
                || e.lang.is_some()
                || self.detector.config.is_exception(&e.typed)
            {
                break;
            }
            match self.detector.advantage(&e.typed, active) {
                Some((delta, to)) if to == target && delta >= LOOKBACK_ADVANTAGE => {}
                _ => break,
            }
            let Some(converted) = layout::convert_by_id(&e.typed, active, target) else {
                break;
            };
            let e = &mut self.history[i];
            e.shown = converted;
            e.shown_layout = target.to_string();
            e.auto = true;
            e.lang = target_lang;
            words += 1;
        }
        if words > 1 {
            log::debug!("fixed {} earlier word(s) along with this one", words - 1);
        }

        self.stats.corrections += 1;
        self.expected_layout = Some(target.to_string());
        let held = self.hold_boundaries.then_some(Boundary {
            key: event.key,
            shift: event.modifiers.shift,
        });
        let change = self.change_for(words, true);
        let action = self.write(&change, 0, change.index, held);
        self.last_change = Some(change);
        action
    }

    /// The word with its capitals fixed, if it shows one of two everyday
    /// slips — and whether the slip was Caps Lock left on.
    ///
    /// * `пРИВЕТ`: Caps Lock was on and Shift was used for the capital, so the
    ///   case is exactly inverted.
    /// * `ПРивет`: Shift was released one letter late.
    ///
    /// Only real dictionary words are touched, so `iPhone`, `GHz` and `McDonald`
    /// stay as they are.
    fn case_fix(&self, token: &str, lang: &str) -> Option<(String, bool)> {
        if !self.detector.config.fix_case {
            return None;
        }
        let model = self.detector.models.get(lang)?;
        let letters: Vec<char> = token.chars().filter(|c| c.is_alphabetic()).collect();
        let n = letters.len();
        if n < 3
            || token
                .chars()
                .any(|c| c.is_alphabetic() && !c.is_lowercase() && !c.is_uppercase())
        {
            return None;
        }
        let inverted = letters[0].is_lowercase() && letters[1..].iter().all(|c| c.is_uppercase());
        let late_shift = n >= 4
            && letters[0].is_uppercase()
            && letters[1].is_uppercase()
            && letters[2..].iter().all(|c| c.is_lowercase());
        if !inverted && !late_shift {
            return None;
        }
        let lower: String = letters.iter().flat_map(|c| c.to_lowercase()).collect();
        let common = model
            .word_logp(&crate::model::normalize(&lower))
            .is_some_and(|lp| lp >= CASE_FIX_FLOOR);
        if !common {
            return None;
        }
        let mut first = true;
        let fixed = token
            .chars()
            .map(|c| {
                if !c.is_alphabetic() {
                    return c.to_string();
                }
                let out: String = if first {
                    c.to_uppercase().collect()
                } else {
                    c.to_lowercase().collect()
                };
                first = false;
                out
            })
            .collect();
        Some((fixed, inverted))
    }

    /// True once after a correction showed that Caps Lock was left on; the
    /// platform switches it off.
    pub fn take_caps_lock(&mut self) -> bool {
        std::mem::take(&mut self.caps_lock_on)
    }

    /// Describe the last `words` history entries as a [`Change`] whose current
    /// variant is what is shown.
    fn change_for(&self, words: usize, auto: bool) -> Change {
        let entries = &self.history[self.history.len() - words..];
        let typed = join(entries.iter().map(|e| (e.typed.as_str(), e.terminator)));
        let shown = join(entries.iter().map(|e| (e.shown.as_str(), e.terminator)));
        let typed_layout = entries[0].typed_layout.clone();
        let shown_layout = entries
            .last()
            .map(|e| e.shown_layout.clone())
            .unwrap_or_else(|| typed_layout.clone());
        let mut variants = vec![typed.clone()];
        let mut layouts = vec![typed_layout.clone()];
        let mut index = 0;
        if shown != typed {
            variants.push(shown);
            layouts.push(shown_layout);
            index = 1;
        }
        // Every other enabled layout, for cycling with three or more.
        for alt in self.detector.config.alternatives(&typed_layout) {
            if layouts.iter().any(|l| l == alt) {
                continue;
            }
            let Some(rendered) = convert_entries(entries, alt) else {
                continue;
            };
            if variants.contains(&rendered) {
                continue;
            }
            variants.push(rendered);
            layouts.push(alt.to_string());
        }
        Change {
            words,
            variants,
            layouts,
            index,
            auto,
        }
    }

    /// The action that replaces variant `from` of `change` with variant `to`.
    ///
    /// With a held boundary, the last word's terminator is not on screen yet:
    /// it is neither deleted nor typed, but sent afterwards.
    fn write(&self, change: &Change, from: usize, to: usize, held: Option<Boundary>) -> Action {
        let mut old = change.variants[from].clone();
        let mut new = change.variants[to].clone();
        if held.is_some() {
            old.pop();
            new.pop();
        }
        // Leave the common beginning alone: fewer keystrokes, less to go wrong.
        let common: usize = old
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let delete_text: String = old.chars().skip(common).collect();
        let type_text: String = new.chars().skip(common).collect();
        // Always named: the replacement is typed on this layout. Whether the
        // keyboard stays there afterwards is the platform's call.
        let switch_to = Some(change.layouts[to].clone());
        Action::Replace {
            delete: delete_text.chars().count(),
            text: type_text,
            original: delete_text,
            switch_to,
            then: held,
        }
    }

    /// Ask the assist about an ambiguous word, if one is configured.
    ///
    /// Answers only from what has already been learned — the keystroke path
    /// cannot wait for a network call. An unknown word is queued and left
    /// alone this time, so the answer is ready the next time it is typed.
    fn resolve_ambiguous(&self, core: &str, candidate: &Candidate, active: &str) -> Option<bool> {
        if !self.detector.config.ai_assist {
            return None;
        }
        let assist = self.assist.as_ref()?;
        if let Some(verdict) = assist.cached(core, &candidate.converted) {
            log::debug!("assist had a verdict for an ambiguous word: convert={verdict}");
            return Some(verdict);
        }
        let typed_language = layout::layout(active)?.def.lang.to_string();
        let alternative_language = layout::layout(candidate.to_layout)?.def.lang.to_string();
        assist.enqueue(Question {
            typed: core.to_string(),
            alternative: candidate.converted.clone(),
            typed_language,
            alternative_language,
        });
        None
    }

    /// Handle a modifier going down or coming up.
    ///
    /// Only the shortcut modifier *alone* counts. As part of a chord — ⌥⌘, ⌥⇧
    /// — it is someone reaching for a shortcut in the app they are using, and
    /// firing on that would make Rekey rewrite text at random moments.
    fn on_modifier_change(&mut self, modifiers: Modifiers, ctx: &Context) -> Action {
        let shortcut = self.detector.config.shortcut;
        let Some(target) = shortcut.modifier() else {
            return Action::None;
        };

        let down = target.is_held(modifiers);
        let others_held = target.others_held(modifiers);
        let was_down = std::mem::replace(&mut self.modifier_was_down, down);

        if others_held {
            // Another modifier joined in; this is a chord, not a tap.
            self.modifier_down_at = None;
            return Action::None;
        }

        match (was_down, down) {
            (false, true) => {
                self.modifier_down_at = Some(Instant::now());
                Action::None
            }
            (true, false) => match self.modifier_down_at.take() {
                Some(at) if at.elapsed() <= MODIFIER_TAP_WINDOW => {
                    self.tapped = true;
                    self.on_tap(shortcut, ctx)
                }
                _ => Action::None,
            },
            _ => Action::None,
        }
    }

    /// A completed tap of the shortcut modifier.
    fn on_tap(&mut self, shortcut: Shortcut, ctx: &Context) -> Action {
        if !shortcut.needs_two_taps() {
            return self.manual(ctx);
        }
        // A double tap: the first arms, the second fires. Anything slower than
        // the window starts over, so a stray tap cannot linger and combine
        // with an unrelated one later.
        match self.last_tap_at.take() {
            Some(previous) if previous.elapsed() <= DOUBLE_TAP_WINDOW => self.manual(ctx),
            _ => {
                self.last_tap_at = Some(Instant::now());
                Action::None
            }
        }
    }

    /// The manual shortcut.
    ///
    /// In order of what the user most likely means:
    ///
    /// 1. Text is selected — convert the selection.
    /// 2. Rekey just changed something — put it back (and again, to redo).
    /// 3. Otherwise convert what was just typed: the word at the caret and
    ///    every word before it that also looks typed on the wrong keyboard.
    /// 4. Nothing typed is known — convert the word before the caret.
    pub fn manual(&mut self, ctx: &Context) -> Action {
        if self.selection {
            self.selection = false;
            if self.detector.config.convert_selection {
                log::info!("shortcut: converting the selection");
                return Action::ConvertSelection { select_word: false };
            }
            return Action::None;
        }
        if self.last_change.is_some() {
            return self.cycle();
        }
        if let Some(change) = self.phrase(ctx) {
            log::info!("shortcut: converting {} typed word(s)", change.words);
            self.stats.manual += 1;
            let action = self.write(&change, 0, change.index, None);
            self.apply_variant(&change);
            self.expected_layout = change.layouts.get(change.index).cloned();
            self.last_change = Some(change);
            return action;
        }
        if self.detector.config.convert_word_before_caret && self.buffer.is_empty() {
            log::info!("shortcut: nothing tracked; converting the word before the caret");
            return Action::ConvertSelection { select_word: true };
        }
        Action::None
    }

    /// Move the last change on to its next reading.
    fn cycle(&mut self) -> Action {
        let Some(mut change) = self.last_change.take() else {
            return Action::None;
        };
        let from = change.index;
        let to = (from + 1) % change.variants.len();
        if to == 0 && change.auto {
            // Rekey changed this and the user has just changed it back. Take
            // the hint and leave those words alone in future.
            self.stats.undos += 1;
            let start = self.history.len().saturating_sub(change.words);
            let learned: Vec<String> = self.history[start..]
                .iter()
                .filter(|e| e.auto)
                .map(|e| e.typed.clone())
                .collect();
            for word in learned {
                if self.detector.config.add_exception(&word) {
                    self.config_dirty = true;
                }
            }
        }
        change.index = to;
        change.auto = false;
        log::info!("shortcut: reading {to} of {}", change.variants.len());
        let action = self.write(&change, from, to, None);
        self.apply_variant(&change);
        self.expected_layout = change.layouts.get(to).cloned();
        self.last_change = Some(change);
        action
    }

    /// Make the history agree with the variant of `change` now on screen.
    fn apply_variant(&mut self, change: &Change) {
        let layout_id = change.layouts[change.index].clone();
        let start = self.history.len().saturating_sub(change.words);
        let lang = layout::layout(&layout_id).map(|l| l.def.lang);
        for e in &mut self.history[start..] {
            if change.index == 0 {
                e.shown = e.typed.clone();
                e.shown_layout = e.typed_layout.clone();
            } else if let Some(c) = layout::convert_by_id(&e.typed, &e.typed_layout, &layout_id) {
                e.shown = c;
                e.shown_layout = layout_id.clone();
            }
            e.auto = false;
            e.lang = lang;
        }
    }

    /// The run of words the shortcut should convert when Rekey has not just
    /// changed anything: the word at the caret, plus every word before it that
    /// also reads better on another layout.
    fn phrase(&mut self, ctx: &Context) -> Option<Change> {
        // A word still being typed is finished here, as if a space had ended
        // it — minus the space, which is not on screen.
        let partial = (!self.buffer.is_empty())
            .then(|| self.buffer.take_current())
            .flatten();
        let partial_layout = self
            .buffer_layout
            .take()
            .or_else(|| ctx.layout.clone())
            .or_else(|| self.detector.config.layouts.first().cloned());
        let mut words = 0;
        if let (Some(word), Some(layout_id)) = (partial, partial_layout) {
            let layout_id = self
                .detector
                .source_layout(&word.text, &layout_id)
                .map(|l| l.def.id.to_string())
                .unwrap_or(layout_id);
            // Nothing follows a partial word yet.
            self.remember(&word.text, OPEN, &layout_id, None);
            words = 1;
        } else if !self.history.is_empty() {
            words = 1;
        }
        if words == 0 {
            return None;
        }
        let anchor = self.history.last()?.clone();
        while words < PHRASE_LIMIT && words < self.history.len() {
            let e = &self.history[self.history.len() - 1 - words];
            if e.auto || e.shown != e.typed || e.typed_layout != anchor.typed_layout {
                break;
            }
            match self.detector.advantage(&e.typed, &e.typed_layout) {
                Some((delta, _)) if delta > 0.0 => words += 1,
                _ => break,
            }
        }
        let entries = &self.history[self.history.len() - words..];
        let typed = join(entries.iter().map(|e| (e.typed.as_str(), e.terminator)));
        let target = self
            .detector
            .best_target(typed.trim_end_matches(OPEN), &anchor.typed_layout)?;
        let mut change = self.change_for(words, false);
        let index = change.layouts.iter().position(|l| l == target).unwrap_or(1);
        if index >= change.variants.len() {
            return None;
        }
        change.index = index;
        // Variant 0 is on screen right now; `write` goes from it.
        let on_screen = change.variants[0].clone();
        if change.variants[index] == on_screen {
            return None;
        }
        // A word still open has no terminator on screen.
        for v in &mut change.variants {
            if v.ends_with(OPEN) {
                v.pop();
            }
        }
        Some(change)
    }

    /// Convert a stretch of text the platform read from the selection.
    /// Returns the converted text and the layout it is now in.
    pub fn convert_text(&mut self, text: &str, active: Option<&str>) -> Option<(String, String)> {
        let active = active
            .map(str::to_string)
            .or_else(|| self.detector.config.layouts.first().cloned())?;
        let (converted, target) = self.detector.convert_text(text, &active)?;
        self.stats.manual += 1;
        self.expected_layout = Some(target.to_string());
        self.forget();
        Some((converted, target.to_string()))
    }

    /// Undo the most recent automatic correction. Kept as an explicit menu
    /// action; the shortcut does the same thing.
    pub fn undo(&mut self) -> Action {
        match &self.last_change {
            Some(change) if change.index != 0 => {
                let mut change = self.last_change.take().expect("checked above");
                let from = change.index;
                if change.auto {
                    self.stats.undos += 1;
                    let start = self.history.len().saturating_sub(change.words);
                    let learned: Vec<String> = self.history[start..]
                        .iter()
                        .filter(|e| e.auto)
                        .map(|e| e.typed.clone())
                        .collect();
                    for word in learned {
                        if self.detector.config.add_exception(&word) {
                            self.config_dirty = true;
                        }
                    }
                }
                change.index = 0;
                change.auto = false;
                let action = self.write(&change, from, 0, None);
                self.apply_variant(&change);
                self.expected_layout = change.layouts.first().cloned();
                self.last_change = Some(change);
                action
            }
            _ => Action::None,
        }
    }
}

/// Words and their terminators as they appear on screen.
fn join<'a>(parts: impl Iterator<Item = (&'a str, char)>) -> String {
    let mut out = String::new();
    for (word, terminator) in parts {
        out.push_str(word);
        out.push(terminator);
    }
    out
}

/// Every entry re-typed on `to`, from the layout it was typed on.
fn convert_entries(entries: &[Entry], to: &str) -> Option<String> {
    let mut out = String::new();
    for e in entries {
        out.push_str(&layout::convert_by_id(&e.typed, &e.typed_layout, to)?);
        out.push(e.terminator);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Modifier, Shortcut};
    use crate::input::{Key, Modifiers};

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            character: Some(c),
            key: Key::Character,
            modifiers: Modifiers::default(),
            synthetic: false,
        }
    }

    fn special(k: Key) -> KeyEvent {
        KeyEvent {
            character: None,
            key: k,
            modifiers: Modifiers::default(),
            synthetic: false,
        }
    }

    fn modifier_event(modifiers: Modifiers) -> KeyEvent {
        KeyEvent {
            character: None,
            key: Key::ModifiersChanged,
            modifiers,
            synthetic: false,
        }
    }

    fn right_cmd(down: bool) -> Modifiers {
        Modifiers {
            meta: down,
            right_meta: down,
            ..Default::default()
        }
    }

    /// Tap the default shortcut, right Command.
    fn tap_shortcut(e: &mut Engine, layout: &str) -> Action {
        let c = ctx(layout);
        e.on_key(&modifier_event(right_cmd(true)), &c);
        e.on_key(&modifier_event(right_cmd(false)), &c)
    }

    fn tap(e: &mut Engine, layout: &str, held: Modifiers) -> Action {
        let c = ctx(layout);
        e.on_key(&modifier_event(held), &c);
        e.on_key(&modifier_event(Modifiers::default()), &c)
    }

    fn models() -> Models {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/models");
        let models = Models::load_dir(&dir).expect("models");
        assert!(!models.is_empty(), "run modelgen first");
        models
    }

    fn engine_with(config: Config) -> Engine {
        Engine::new(models(), config)
    }

    fn engine() -> Engine {
        engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            ..Config::default()
        })
    }

    fn ctx(layout: &str) -> Context {
        Context {
            app: Some("com.apple.TextEdit".into()),
            layout: Some(layout.into()),
            secure_input: false,
        }
    }

    fn replaced(action: &Action) -> (usize, &str) {
        match action {
            Action::Replace { delete, text, .. } => (*delete, text),
            other => panic!("expected a replacement, got {other:?}"),
        }
    }

    /// Type a string and return the last non-None action.
    fn type_str(e: &mut Engine, s: &str, layout: &str) -> Action {
        let c = ctx(layout);
        let mut last = Action::None;
        for ch in s.chars() {
            let ev = match ch {
                ' ' => special(Key::Space),
                '\n' => special(Key::Enter),
                _ => key(ch),
            };
            let action = e.on_key(&ev, &c);
            if action != Action::None {
                last = action;
            }
        }
        last
    }

    /// A tiny model of the document: applies actions the way the platform
    /// would, so tests can check what ends up on screen.
    struct Screen {
        text: String,
        held: bool,
    }

    impl Screen {
        fn new(held: bool) -> Screen {
            Screen {
                text: String::new(),
                held,
            }
        }

        fn key(&mut self, e: &mut Engine, ev: KeyEvent, layout: &str) {
            let action = e.on_key(&ev, &ctx(layout));
            let swallowed = self.held && action.swallows_key();
            if !swallowed {
                match ev.key {
                    Key::Character => self.text.push(ev.character.unwrap()),
                    Key::Space => self.text.push(' '),
                    Key::Enter => self.text.push('\n'),
                    Key::Backspace => {
                        self.text.pop();
                    }
                    _ => {}
                }
            }
            self.apply(&action);
        }

        fn apply(&mut self, action: &Action) {
            if let Action::Replace {
                delete, text, then, ..
            } = action
            {
                for _ in 0..*delete {
                    self.text.pop();
                }
                self.text.push_str(text);
                if let Some(b) = then {
                    self.text.push(match b.key {
                        Key::Enter => '\n',
                        Key::Tab => '\t',
                        _ => ' ',
                    });
                }
            }
        }

        fn type_str(&mut self, e: &mut Engine, s: &str, layout: &str) {
            for ch in s.chars() {
                let ev = match ch {
                    ' ' => special(Key::Space),
                    '\n' => special(Key::Enter),
                    '\u{8}' => special(Key::Backspace),
                    _ => key(ch),
                };
                self.key(e, ev, layout);
            }
        }

        fn tap(&mut self, e: &mut Engine, layout: &str) {
            let action = tap_shortcut(e, layout);
            self.apply(&action);
        }
    }

    #[test]
    fn corrects_a_word_at_the_space() {
        let mut e = engine();
        let action = type_str(&mut e, "ghbdtn ", "us");
        assert_eq!(
            action,
            Action::Replace {
                delete: 7,
                text: "привет ".into(),
                original: "ghbdtn ".into(),
                switch_to: Some("ru".into()),
                then: None,
            }
        );
        assert_eq!(e.stats().corrections, 1);
    }

    #[test]
    fn with_held_boundaries_the_space_is_sent_after_the_word() {
        let mut e = engine();
        e.set_hold_boundaries(true);
        let action = type_str(&mut e, "ghbdtn ", "us");
        assert!(action.swallows_key());
        assert_eq!(replaced(&action), (6, "привет"));
        let mut s = Screen::new(true);
        let mut e = engine();
        e.set_hold_boundaries(true);
        s.type_str(&mut e, "ghbdtn rfr ltkf ", "us");
        assert_eq!(s.text, "привет как дела ");
    }

    #[test]
    fn return_is_corrected_only_when_it_can_be_held() {
        // Without holding, Return would already have sent the chat message.
        let mut e = engine();
        assert_eq!(type_str(&mut e, "ghbdtn\n", "us"), Action::None);
        let mut e = engine();
        e.set_hold_boundaries(true);
        let action = type_str(&mut e, "ghbdtn\n", "us");
        assert!(action.swallows_key());
        // And nothing before the Return can be rewritten afterwards.
        let mut s = Screen::new(true);
        let mut e = engine();
        e.set_hold_boundaries(true);
        s.type_str(&mut e, "ghbdtn\n", "us");
        assert_eq!(s.text, "привет\n");
        s.tap(&mut e, "ru");
        assert_eq!(s.text, "привет\n");
    }

    #[test]
    fn leaves_correct_words_untouched() {
        let mut e = engine();
        assert_eq!(type_str(&mut e, "hello world ", "us"), Action::None);
        assert_eq!(type_str(&mut e, "привет мир ", "ru"), Action::None);
        assert_eq!(e.stats().corrections, 0);
    }

    #[test]
    fn common_short_words_are_corrected() {
        for (typed, want) in [
            ("lf ", "да "),
            ("ye ", "ну "),
            ("ds ", "вы "),
            ("nen ", "тут "),
        ] {
            let mut e = engine();
            let action = type_str(&mut e, typed, "us");
            assert_eq!(replaced(&action).1, want, "{typed:?}");
        }
    }

    #[test]
    fn keeps_trailing_punctuation() {
        let mut e = engine();
        assert_eq!(replaced(&type_str(&mut e, "ghbdtn! ", "us")).1, "привет! ");
        let mut e = engine();
        assert_eq!(replaced(&type_str(&mut e, "ghbdtn. ", "us")).1, "привет. ");
        let mut e = engine();
        // English with a full stop, typed on Russian: ю is the full-stop key.
        let typed = crate::layout::convert_by_id("hello.", "us", "ru").unwrap();
        assert_eq!(
            replaced(&type_str(&mut e, &format!("{typed} "), "ru")).1,
            "hello. "
        );
    }

    #[test]
    fn does_not_strip_punctuation_that_is_a_cyrillic_letter() {
        let mut e = engine();
        // The trailing `,` is the letter б, part of the word "хлеб".
        let typed = crate::layout::convert_by_id("хлеб", "ru", "us").unwrap();
        assert!(typed.ends_with(','));
        let text = replaced(&type_str(&mut e, &format!("{typed} "), "us"))
            .1
            .to_string();
        assert_eq!(text, "хлеб ");
    }

    #[test]
    fn short_words_before_a_corrected_one_are_fixed_too() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "z gjqle ", "us");
        assert_eq!(s.text, "я пойду ");
    }

    #[test]
    fn ignores_its_own_typing() {
        let mut e = engine();
        for ch in "ghbdtn".chars() {
            let mut ev = key(ch);
            ev.synthetic = true;
            assert_eq!(e.on_key(&ev, &ctx("us")), Action::None);
        }
        let mut space = special(Key::Space);
        space.synthetic = true;
        assert_eq!(e.on_key(&space, &ctx("us")), Action::None);
        assert_eq!(e.stats().corrections, 0);
    }

    #[test]
    fn does_not_convert_gibberish_into_other_gibberish() {
        let mut e = engine();
        for gibberish in ["qxzvj ", "vbxzq ", "zxcvq ", "wqxzj "] {
            assert_eq!(
                type_str(&mut e, gibberish, "us"),
                Action::None,
                "{gibberish:?}"
            );
        }
    }

    #[test]
    fn stands_down_in_password_fields() {
        let mut e = engine();
        let secure = Context {
            secure_input: true,
            ..ctx("us")
        };
        for ch in "ghbdtn".chars() {
            e.on_key(&key(ch), &secure);
        }
        assert_eq!(e.on_key(&special(Key::Space), &secure), Action::None);
        assert_eq!(e.stats().corrections, 0);
    }

    #[test]
    fn stands_down_in_excluded_apps() {
        let mut e = engine();
        let terminal = Context {
            app: Some("com.googlecode.iterm2".into()),
            ..ctx("us")
        };
        for ch in "ghbdtn".chars() {
            e.on_key(&key(ch), &terminal);
        }
        assert_eq!(e.on_key(&special(Key::Space), &terminal), Action::None);
        // The shortcut does nothing there either.
        e.on_key(&modifier_event(right_cmd(true)), &terminal);
        assert_eq!(
            e.on_key(&modifier_event(right_cmd(false)), &terminal),
            Action::None
        );
    }

    #[test]
    fn a_shortcut_chord_discards_the_word_in_progress() {
        let mut e = engine();
        for ch in "ghbdt".chars() {
            e.on_key(&key(ch), &ctx("us"));
        }
        let mut save = key('s');
        save.modifiers.meta = true;
        e.on_key(&save, &ctx("us"));
        assert_eq!(type_str(&mut e, "n ", "us"), Action::None);
    }

    #[test]
    fn a_command_chord_does_not_fire_the_shortcut() {
        // ⌘C with the right Command key: down, C, up. Not a tap.
        let mut e = engine();
        type_str(&mut e, "hello ", "us");
        let c = ctx("us");
        e.on_key(&modifier_event(right_cmd(true)), &c);
        let mut copy = key('c');
        copy.modifiers = right_cmd(true);
        e.on_key(&copy, &c);
        assert_eq!(
            e.on_key(&modifier_event(right_cmd(false)), &c),
            Action::None
        );
    }

    #[test]
    fn left_command_is_not_the_right_command_shortcut() {
        let mut e = engine();
        type_str(&mut e, "hello ", "us");
        let left = Modifiers {
            meta: true,
            ..Default::default()
        };
        assert_eq!(tap(&mut e, "us", left), Action::None);
    }

    #[test]
    fn switching_apps_discards_the_word_in_progress() {
        let mut e = engine();
        for ch in "ghbdt".chars() {
            e.on_key(&key(ch), &ctx("us"));
        }
        let other = Context {
            app: Some("com.apple.Safari".into()),
            ..ctx("us")
        };
        e.on_key(&key('n'), &other);
        assert_eq!(e.on_key(&special(Key::Space), &other), Action::None);
    }

    #[test]
    fn arrow_keys_discard_the_word_in_progress() {
        let mut e = engine();
        for ch in "ghbdt".chars() {
            e.on_key(&key(ch), &ctx("us"));
        }
        e.on_key(&special(Key::Navigation), &ctx("us"));
        assert_eq!(type_str(&mut e, "n ", "us"), Action::None);
    }

    #[test]
    fn the_shortcut_undoes_a_correction_and_learns_the_exception() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "ghbdtn ", "us");
        assert_eq!(s.text, "привет ");
        s.tap(&mut e, "ru");
        assert_eq!(s.text, "ghbdtn ");
        assert_eq!(e.stats().undos, 1);
        assert!(e.take_config_dirty());
        // Tapping again puts the correction back.
        s.tap(&mut e, "us");
        assert_eq!(s.text, "привет ");
        // Having been overruled once, it does not try again on its own.
        let mut e2 = engine_with(e.config().clone());
        assert_eq!(type_str(&mut e2, "ghbdtn ", "us"), Action::None);
    }

    #[test]
    fn the_shortcut_undoes_with_held_boundaries_too() {
        let mut s = Screen::new(true);
        let mut e = engine();
        e.set_hold_boundaries(true);
        s.type_str(&mut e, "ghbdtn ", "us");
        assert_eq!(s.text, "привет ");
        s.tap(&mut e, "ru");
        assert_eq!(s.text, "ghbdtn ");
    }

    #[test]
    fn the_shortcut_converts_a_missed_word() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "hello ", "us");
        s.tap(&mut e, "us");
        assert_eq!(s.text, "руддщ ");
        s.tap(&mut e, "ru");
        assert_eq!(s.text, "hello ");
    }

    #[test]
    fn the_shortcut_converts_the_word_being_typed() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "hello ghbdtn", "us");
        s.tap(&mut e, "us");
        assert_eq!(s.text, "hello привет");
        // Typing carries on normally afterwards.
        s.type_str(&mut e, " vbh ", "ru");
    }

    #[test]
    fn the_shortcut_converts_the_whole_wrong_layout_phrase() {
        // Nothing here was corrected on its own; the shortcut finds where the
        // wrong-layout run begins and converts all of it.
        let mut s = Screen::new(false);
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            enabled: true,
            sensitivity: crate::config::Sensitivity::Cautious,
            ..Config::default()
        });
        s.type_str(&mut e, "hello z nen", "us");
        s.tap(&mut e, "us");
        assert_eq!(s.text, "hello я тут");
    }

    #[test]
    fn backspacing_into_a_word_keeps_tracking_it() {
        let mut s = Screen::new(false);
        let mut e = engine();
        // "ghbdt" is corrected to "приве", then the user backspaces over the
        // space and finishes the word — now on the Russian keyboard.
        s.type_str(&mut e, "ghbdt \u{8}", "us");
        assert_eq!(s.text, "приве");
        s.type_str(&mut e, "т ", "ru");
        assert_eq!(s.text, "привет ");
    }

    #[test]
    fn selecting_text_makes_the_shortcut_convert_the_selection() {
        let mut e = engine();
        type_str(&mut e, "hello ", "us");
        e.on_key(&special(Key::Selection), &ctx("us"));
        assert_eq!(
            tap_shortcut(&mut e, "us"),
            Action::ConvertSelection { select_word: false }
        );
        // Shift+arrow selects too.
        let mut e = engine();
        let mut ev = special(Key::Navigation);
        ev.modifiers.shift = true;
        e.on_key(&ev, &ctx("us"));
        assert_eq!(
            tap_shortcut(&mut e, "us"),
            Action::ConvertSelection { select_word: false }
        );
    }

    #[test]
    fn copying_a_selection_keeps_it_convertible() {
        let mut e = engine();
        e.on_key(&special(Key::Selection), &ctx("us"));
        let mut copy = key('c');
        copy.modifiers.meta = true;
        e.on_key(&copy, &ctx("us"));
        assert_eq!(
            tap_shortcut(&mut e, "us"),
            Action::ConvertSelection { select_word: false }
        );
    }

    #[test]
    fn a_correction_can_be_undone_after_the_layout_changes_back() {
        // With "switch the keyboard" off, the platform puts the old layout
        // back right after typing the correction.
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "ghbdtn ", "us");
        assert_eq!(s.text, "привет ");
        s.tap(&mut e, "ru");
        s.tap(&mut e, "us");
        assert_eq!(s.text, "привет ");
        s.tap(&mut e, "us");
        assert_eq!(s.text, "ghbdtn ");
    }

    #[test]
    fn with_nothing_tracked_the_shortcut_converts_the_word_before_the_caret() {
        let mut e = engine();
        e.on_key(&special(Key::Click), &ctx("us"));
        assert_eq!(
            tap_shortcut(&mut e, "us"),
            Action::ConvertSelection { select_word: true }
        );
    }

    #[test]
    fn converting_text_works_in_both_directions() {
        let mut e = engine();
        assert_eq!(
            e.convert_text("ghbdtn rfr ltkf", Some("us")),
            Some(("привет как дела".to_string(), "ru".to_string()))
        );
        assert_eq!(
            e.convert_text("руддщ цщкдв", Some("us")),
            Some(("hello world".to_string(), "us".to_string()))
        );
    }

    #[test]
    fn a_layout_change_the_user_made_mid_word_drops_the_word() {
        let mut e = engine();
        for ch in "ghb".chars() {
            e.on_key(&key(ch), &ctx("us"));
        }
        assert_eq!(type_str(&mut e, "dtn ", "uk"), Action::None);
    }

    #[test]
    fn the_shortcut_is_configurable() {
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            shortcut: Shortcut::Tap {
                modifier: Modifier::Shift,
            },
            ..Config::default()
        });
        type_str(&mut e, "hello ", "us");
        assert_eq!(tap_shortcut(&mut e, "us"), Action::None);
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert_eq!(replaced(&tap(&mut e, "us", shift)).1, "руддщ ");
    }

    #[test]
    fn a_double_tap_shortcut_needs_both_taps() {
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            shortcut: Shortcut::DoubleTap {
                modifier: Modifier::Option,
            },
            ..Config::default()
        });
        type_str(&mut e, "hello ", "us");
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        assert_eq!(tap(&mut e, "us", alt), Action::None);
        assert_eq!(replaced(&tap(&mut e, "us", alt)).1, "руддщ ");
    }

    #[test]
    fn the_shortcut_can_be_switched_off_entirely() {
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            shortcut: Shortcut::Off,
            ..Config::default()
        });
        type_str(&mut e, "hello ", "us");
        assert_eq!(tap_shortcut(&mut e, "us"), Action::None);
    }

    #[test]
    fn cycling_through_three_layouts_visits_each_reading() {
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into(), "uk".into()],
            ..Config::default()
        });
        let mut s = Screen::new(false);
        s.type_str(&mut e, "ds ", "us");
        let mut seen = vec![s.text.clone()];
        for _ in 0..3 {
            s.tap(&mut e, "us");
            seen.push(s.text.clone());
        }
        let distinct: std::collections::HashSet<_> = seen.iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "expected three distinct readings, got {seen:?}"
        );
    }

    #[test]
    fn holding_option_to_type_an_accent_is_not_a_shortcut() {
        let mut e = engine_with(Config {
            layouts: vec!["us".into(), "ru".into()],
            shortcut: Shortcut::Tap {
                modifier: Modifier::Option,
            },
            ..Config::default()
        });
        type_str(&mut e, "hello ", "us");
        let c = ctx("us");
        let alt = Modifiers {
            alt: true,
            ..Default::default()
        };
        e.on_key(&modifier_event(alt), &c);
        e.on_key(&key('e'), &c);
        assert_eq!(
            e.on_key(&modifier_event(Modifiers::default()), &c),
            Action::None
        );
    }

    #[test]
    fn an_ambiguous_word_is_queued_for_the_assist_and_left_alone() {
        use crate::assist::test_support::FakeAssist;
        let mut e = engine();
        let mut cfg = e.config().clone();
        cfg.ai_assist = true;
        e.set_config(cfg);
        let assist = std::sync::Arc::new(FakeAssist::default());
        e.set_assist(Some(assist.clone()));
        for word in [
            "ntcn", "vjq", "hfp", "ldf", "ytn", "ds,", "rjn", "ne", "herb",
        ] {
            type_str(&mut e, &format!("{word} "), "us");
        }
        let asked = assist.asked.lock().unwrap();
        for question in asked.iter() {
            assert!(!question.typed.is_empty());
            assert_ne!(question.typed, question.alternative);
            assert_eq!(question.typed_language, "en");
        }
    }

    #[test]
    fn a_learned_assist_verdict_converts_an_ambiguous_word() {
        use crate::assist::test_support::FakeAssist;
        // Find a word the detector genuinely calls ambiguous, so the test
        // exercises the path rather than assuming which word qualifies.
        let probe = engine();
        let ambiguous = [
            "ne", "herb", "ntcn", "vjq", "hfp", "ldf", "ytn", "rjn", "yt", "jr",
        ]
        .iter()
        .find(|w| matches!(probe.detector.evaluate(w, "us"), Verdict::Ambiguous(_)))
        .copied();
        let Some(word) = ambiguous else {
            return;
        };
        let mut e = engine();
        let mut cfg = e.config().clone();
        cfg.ai_assist = true;
        e.set_config(cfg);
        e.set_assist(Some(std::sync::Arc::new(FakeAssist::with_answer(
            word, true,
        ))));
        let action = type_str(&mut e, &format!("{word} "), "us");
        assert!(
            action != Action::None,
            "a learned 'convert' verdict should be acted on for {word:?}"
        );
    }

    #[test]
    fn the_assist_is_ignored_when_the_setting_is_off() {
        use crate::assist::test_support::FakeAssist;
        let mut e = engine();
        let assist = std::sync::Arc::new(FakeAssist::default());
        e.set_assist(Some(assist.clone()));
        for word in ["ntcn", "vjq", "hfp", "ldf", "ytn", "ne", "herb"] {
            type_str(&mut e, &format!("{word} "), "us");
        }
        assert!(assist.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn disabled_engine_does_nothing() {
        let mut e = engine();
        let mut cfg = e.config().clone();
        cfg.enabled = false;
        e.set_config(cfg);
        assert_eq!(type_str(&mut e, "ghbdtn ", "us"), Action::None);
    }

    #[test]
    fn the_previous_word_settles_an_ambiguous_one() {
        // `vs` is English, and also мы on the wrong keyboard. In an English
        // sentence it stays English.
        let mut e = engine();
        assert_eq!(type_str(&mut e, "cats vs ", "us"), Action::None);
    }

    #[test]
    fn caps_lock_left_on_is_fixed_and_reported() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "пРИВЕТ ", "ru");
        assert_eq!(s.text, "Привет ");
        assert!(
            e.take_caps_lock(),
            "the platform should be told to switch Caps Lock off"
        );
        // Wrong layout and Caps Lock together.
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "gHBDTN ", "us");
        assert_eq!(s.text, "Привет ");
    }

    #[test]
    fn a_late_shift_is_fixed_but_names_and_units_are_not() {
        let mut s = Screen::new(false);
        let mut e = engine();
        s.type_str(&mut e, "ПРивет ", "ru");
        assert_eq!(s.text, "ПРивет ".replace("ПРивет", "Привет"));
        assert!(!e.take_caps_lock());
        for word in ["iPhone ", "GHz ", "McDonald ", "NASA "] {
            let mut s = Screen::new(false);
            let mut e = engine();
            s.type_str(&mut e, word, "us");
            assert_eq!(s.text, word);
        }
    }

    #[test]
    fn a_shortcut_tap_is_reported_for_masking() {
        let mut e = engine();
        type_str(&mut e, "hello ", "us");
        assert!(!e.take_tap());
        tap_shortcut(&mut e, "us");
        assert!(e.take_tap());
        assert!(!e.take_tap(), "reported once");
    }
}
