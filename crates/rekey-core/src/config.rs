//! User-tunable behaviour for the detection engine.

use serde::{Deserialize, Serialize};

/// How eager the detector should be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sensitivity {
    /// Only act on overwhelming evidence. Almost never wrong, misses more.
    Cautious,
    /// The default: acts when the other reading is clearly a real word.
    Balanced,
    /// Acts on weaker evidence. Catches more slang, occasionally overreaches.
    Eager,
}

impl Sensitivity {
    /// Minimum log10 score advantage the alternative reading must have.
    pub fn threshold(self) -> f32 {
        match self {
            Sensitivity::Cautious => 3.5,
            Sensitivity::Balanced => 2.0,
            Sensitivity::Eager => 1.0,
        }
    }

    /// Extra advantage demanded when the corrected reading is not a real word
    /// either.
    ///
    /// When neither spelling is in the dictionary the comparison is between two
    /// pieces of gibberish, decided on letter statistics alone. That is exactly
    /// the situation where the engine is most likely to be wrong and least
    /// likely to be useful, so it needs a much stronger signal — while still
    /// leaving room for genuinely new slang the corpus has never seen.
    pub fn unknown_target_surcharge(self) -> f32 {
        match self {
            Sensitivity::Cautious => 3.0,
            Sensitivity::Balanced => 2.5,
            Sensitivity::Eager => 1.5,
        }
    }

    /// Extra advantage demanded when the text as typed is *already* a real word
    /// in the active language. Converting those is how a switcher earns its
    /// reputation for being infuriating, so the bar is deliberately steep.
    pub fn known_word_surcharge(self) -> f32 {
        match self {
            Sensitivity::Cautious => 6.0,
            Sensitivity::Balanced => 4.5,
            Sensitivity::Eager => 3.0,
        }
    }
}

/// A modifier key that can be used as the manual shortcut.
///
/// Only modifiers are offered. They produce no text on their own, so they can
/// be overloaded without stealing a keystroke from the app underneath — and
/// the shortcut has to work in every application, which rules out anything an
/// app might already claim. The right-hand keys are offered on their own: they
/// are rarely part of a chord, so a tap of right Command is about the safest
/// trigger there is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    /// Either Option key.
    Option,
    /// Either Shift key.
    Shift,
    /// Either Control key.
    Control,
    /// Either Command key.
    Command,
    RightOption,
    RightShift,
    RightControl,
    RightCommand,
}

impl Modifier {
    /// Is this modifier currently held?
    pub fn is_held(self, m: crate::input::Modifiers) -> bool {
        match self {
            Modifier::Option => m.alt,
            Modifier::Shift => m.shift,
            Modifier::Control => m.control,
            Modifier::Command => m.meta,
            Modifier::RightOption => m.alt && m.right_alt,
            Modifier::RightShift => m.shift && m.right_shift,
            Modifier::RightControl => m.control && m.right_control,
            Modifier::RightCommand => m.meta && m.right_meta,
        }
    }

    /// Is any modifier *other* than this one held?
    pub fn others_held(self, m: crate::input::Modifiers) -> bool {
        // For a right-hand key, the same modifier held on the left is "another"
        // key; the flags cannot tell both-held apart from right-only, which is
        // the harmless direction to be wrong in.
        let family = |held: bool, right: bool, mine: bool, right_only: bool| -> bool {
            held && !(mine && (!right_only || right))
        };
        let (opt, sh, ctl, cmd) = match self {
            Modifier::Option => (Some(false), None, None, None),
            Modifier::RightOption => (Some(true), None, None, None),
            Modifier::Shift => (None, Some(false), None, None),
            Modifier::RightShift => (None, Some(true), None, None),
            Modifier::Control => (None, None, Some(false), None),
            Modifier::RightControl => (None, None, Some(true), None),
            Modifier::Command => (None, None, None, Some(false)),
            Modifier::RightCommand => (None, None, None, Some(true)),
        };
        family(m.alt, m.right_alt, opt.is_some(), opt.unwrap_or(false))
            || family(m.shift, m.right_shift, sh.is_some(), sh.unwrap_or(false))
            || family(
                m.control,
                m.right_control,
                ctl.is_some(),
                ctl.unwrap_or(false),
            )
            || family(m.meta, m.right_meta, cmd.is_some(), cmd.unwrap_or(false))
    }

    pub fn label(self) -> &'static str {
        match self {
            Modifier::Option => "Option",
            Modifier::Shift => "Shift",
            Modifier::Control => "Control",
            Modifier::Command => "Command",
            Modifier::RightOption => "Right Option",
            Modifier::RightShift => "Right Shift",
            Modifier::RightControl => "Right Control",
            Modifier::RightCommand => "Right Command",
        }
    }
}

/// How the manual "cycle the last word" shortcut is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Shortcut {
    /// No manual shortcut at all.
    Off,
    /// Press and release the modifier with nothing in between.
    Tap { modifier: Modifier },
    /// Two taps in quick succession.
    ///
    /// Worth choosing for a modifier used in ordinary chords: a single tap of
    /// Option sits awkwardly beside Option+Backspace and Option+Arrow, which
    /// many people use constantly.
    DoubleTap { modifier: Modifier },
}

impl Shortcut {
    pub fn modifier(self) -> Option<Modifier> {
        match self {
            Shortcut::Off => None,
            Shortcut::Tap { modifier } | Shortcut::DoubleTap { modifier } => Some(modifier),
        }
    }

    pub fn needs_two_taps(self) -> bool {
        matches!(self, Shortcut::DoubleTap { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Master switch.
    pub enabled: bool,
    /// Layout ids the user actually types in, e.g. `["us", "ru"]`.
    pub layouts: Vec<String>,
    pub sensitivity: Sensitivity,
    /// Words shorter than this are never converted automatically.
    pub min_word_len: usize,
    /// Below this length, only an exact dictionary hit will do; letter-shape
    /// evidence alone is too weak on very short words.
    pub require_dict_below_len: usize,
    /// Never touch words the user has corrected back by hand.
    pub exceptions: Vec<String>,
    /// Applications that Rekey ignores entirely (bundle id or executable name).
    pub excluded_apps: Vec<String>,
    /// Leave ALL-CAPS tokens alone; they are usually acronyms or constants.
    pub skip_all_caps: bool,
    /// Ask the optional AI assist about words the local model finds ambiguous.
    pub ai_assist: bool,
    /// How to trigger the manual shortcut: undo the last correction, or
    /// convert what was just typed (or the selection) by hand.
    pub shortcut: Shortcut,
    /// Switch the system keyboard to the corrected layout, so the next word
    /// comes out right by itself.
    pub switch_layout: bool,
    /// Let the shortcut convert selected text.
    pub convert_selection: bool,
    /// When the shortcut finds nothing typed to act on, select the word before
    /// the caret and convert that.
    pub convert_word_before_caret: bool,
    /// Play a quiet sound when a word is corrected automatically.
    pub sound: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            enabled: true,
            layouts: vec!["us".into(), "ru".into()],
            sensitivity: Sensitivity::Balanced,
            min_word_len: 2,
            require_dict_below_len: 4,
            exceptions: Vec::new(),
            excluded_apps: default_excluded_apps(),
            skip_all_caps: true,
            ai_assist: false,
            shortcut: Shortcut::Tap {
                modifier: Modifier::RightCommand,
            },
            switch_layout: true,
            convert_selection: true,
            convert_word_before_caret: true,
            sound: false,
        }
    }
}

/// Password managers and terminals, where silently rewriting keystrokes ranges
/// from annoying to genuinely destructive.
fn default_excluded_apps() -> Vec<String> {
    [
        "com.apple.keychainaccess",
        "com.1password.1password",
        "com.agilebits.onepassword7",
        "com.bitwarden.desktop",
        "com.apple.Terminal",
        "com.googlecode.iterm2",
        "dev.warp.Warp-Stable",
        "net.kovidgoyal.kitty",
        "com.mitchellh.ghostty",
        "io.alacritty",
        "1Password.exe",
        "KeePass.exe",
        "Bitwarden.exe",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

impl Config {
    /// Layout ids other than `active` that are worth testing as alternatives.
    pub fn alternatives(&self, active: &str) -> Vec<&str> {
        self.layouts
            .iter()
            .map(|s| s.as_str())
            .filter(|id| *id != active)
            .collect()
    }

    pub fn is_exception(&self, word: &str) -> bool {
        let w = word.to_lowercase();
        self.exceptions.iter().any(|e| e.to_lowercase() == w)
    }

    /// Remember that the user prefers this word exactly as they typed it.
    /// Returns true when the list changed.
    pub fn add_exception(&mut self, word: &str) -> bool {
        let key: String = word
            .trim_matches(|c: char| !c.is_alphabetic())
            .to_lowercase();
        if key.is_empty() || self.is_exception(&key) {
            return false;
        }
        self.exceptions.push(key);
        true
    }

    pub fn is_excluded_app(&self, app: &str) -> bool {
        self.excluded_apps
            .iter()
            .any(|a| a.eq_ignore_ascii_case(app))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Modifiers;

    #[test]
    fn right_command_is_not_left_command() {
        let left = Modifiers {
            meta: true,
            ..Default::default()
        };
        let right = Modifiers {
            meta: true,
            right_meta: true,
            ..Default::default()
        };
        assert!(!Modifier::RightCommand.is_held(left));
        assert!(Modifier::RightCommand.is_held(right));
        assert!(Modifier::Command.is_held(left) && Modifier::Command.is_held(right));
        // Left Command held while waiting for a right Command tap is a chord.
        assert!(Modifier::RightCommand.others_held(left));
        assert!(!Modifier::RightCommand.others_held(right));
        let with_shift = Modifiers {
            shift: true,
            ..right
        };
        assert!(Modifier::RightCommand.others_held(with_shift));
    }

    #[test]
    fn older_settings_still_load() {
        let old = r#"{"shortcut":{"kind":"tap","modifier":"option"},"layouts":["us","ru"]}"#;
        let config: Config = serde_json::from_str(old).unwrap();
        assert_eq!(
            config.shortcut,
            Shortcut::Tap {
                modifier: Modifier::Option
            }
        );
        assert!(config.switch_layout);
        let new = r#"{"shortcut":{"kind":"doubletap","modifier":"right_command"}}"#;
        let config: Config = serde_json::from_str(new).unwrap();
        assert_eq!(
            config.shortcut,
            Shortcut::DoubleTap {
                modifier: Modifier::RightCommand
            }
        );
    }
}
