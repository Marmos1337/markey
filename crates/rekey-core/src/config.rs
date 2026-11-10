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
    /// The Fn / 🌐 key of Mac keyboards. Windows never sees it: the key is
    /// handled inside the keyboard itself there.
    Fn,
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
            Modifier::Fn => m.function,
        }
    }

    /// Is any modifier *other* than this one held?
    pub fn others_held(self, m: crate::input::Modifiers) -> bool {
        if self == Modifier::Fn {
            return m.shift || m.control || m.alt || m.meta;
        }
        if m.function {
            return true;
        }
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
            Modifier::Fn => unreachable!("handled above"),
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

    /// The shortcut key a new installation starts with: right Command on a
    /// Mac, right Ctrl on Windows — where a tap of the Windows key opens Start.
    pub fn platform_default() -> Modifier {
        if cfg!(windows) {
            Modifier::RightControl
        } else {
            Modifier::RightCommand
        }
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
            Modifier::Fn => "Fn",
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

    /// Two hotkeys on the same key cannot be told apart reliably — a single
    /// tap would also arm a double tap — so one key serves one hotkey.
    pub fn collides_with(self, other: Shortcut) -> bool {
        matches!((self.modifier(), other.modifier()), (Some(a), Some(b)) if a == b)
    }
}

/// What a hotkey does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotkey {
    /// The main shortcut: undo, convert what was typed, or the selection.
    Main,
    /// Put what was just typed (or the selection) into the n-th layout of
    /// [`Config::layouts`] and switch the keyboard to it, whatever the
    /// detector thinks. «Всегда раскладка 1 / 2».
    ForceLayout(usize),
    /// Add the word at the caret to the exceptions.
    AddException,
}

/// A short form the user types and the text it stands for: `спс` → `спасибо`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abbreviation {
    pub short: String,
    pub full: String,
}

/// Limits on the abbreviation list, checked when settings are saved.
pub const MAX_ABBREVIATIONS: usize = 300;
pub const MAX_ABBREVIATION_SHORT: usize = 32;
pub const MAX_ABBREVIATION_FULL: usize = 500;

/// What the menu bar icon shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MenuBarIcon {
    /// The MarKey keycap.
    #[default]
    Logo,
    /// The current layout, as a badge with its code, centred.
    Layout,
}

/// How big the layout badge in the menu bar is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MenuBarSize {
    Small,
    #[default]
    Medium,
    Large,
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
    /// Fix `пРИВЕТ` (Caps Lock left on, which is then switched off).
    pub fix_case: bool,
    /// Fix `ПРивет` (Shift held a letter too long). Was part of `fix_case`
    /// until 0.3; settings from before keep what they had.
    pub fix_two_caps: bool,
    /// Also correct a word ended by Return or Tab (only where the key can be
    /// held back until the word is fixed).
    pub correct_on_enter_tab: bool,
    /// Fix obvious typos in real words: two letters swapped, one doubled.
    pub fix_typos: bool,
    /// Replace short forms with their full text.
    pub expand_abbreviations: bool,
    pub abbreviations: Vec<Abbreviation>,
    /// Use the extended dictionary (slang, names, brands) when it is present.
    pub extended_dictionary: bool,
    /// Hotkeys besides [`Config::shortcut`]. Off unless chosen.
    pub force_layout_1: Shortcut,
    pub force_layout_2: Shortcut,
    pub add_exception_key: Shortcut,
    pub menu_bar_icon: MenuBarIcon,
    pub menu_bar_size: MenuBarSize,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            enabled: true,
            layouts: vec!["us".into(), "ru".into()],
            sensitivity: Sensitivity::Balanced,
            min_word_len: 1,
            require_dict_below_len: 4,
            exceptions: Vec::new(),
            excluded_apps: default_excluded_apps(),
            skip_all_caps: true,
            ai_assist: false,
            shortcut: Shortcut::Tap {
                modifier: Modifier::platform_default(),
            },
            switch_layout: true,
            convert_selection: true,
            convert_word_before_caret: true,
            sound: false,
            fix_case: true,
            fix_two_caps: false,
            correct_on_enter_tab: false,
            fix_typos: false,
            expand_abbreviations: false,
            abbreviations: Vec::new(),
            extended_dictionary: false,
            force_layout_1: Shortcut::Off,
            force_layout_2: Shortcut::Off,
            add_exception_key: Shortcut::Off,
            menu_bar_icon: MenuBarIcon::Logo,
            menu_bar_size: MenuBarSize::Medium,
        }
    }
}

/// Common games where movement and interaction keys must remain unchanged.
pub const GAME_EXCLUSIONS: &[&str] = &[
    "cs2.exe",
    "csgo.exe",
    "dota2.exe",
    "VALORANT-Win64-Shipping.exe",
    "FortniteClient-Win64-Shipping.exe",
    "r5apex.exe",
    "Overwatch.exe",
    "League of Legends.exe",
    "RainbowSix.exe",
    "RainbowSix_Vulkan.exe",
    "RustClient.exe",
    "TslGame.exe",
    "GTA5.exe",
    "GTA5_Enhanced.exe",
];

/// Password managers, terminals and games must not receive text corrections.
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
        "KeePassXC.exe",
        "Bitwarden.exe",
        "WindowsTerminal.exe",
        "cmd.exe",
        "powershell.exe",
        "pwsh.exe",
        "conhost.exe",
        "putty.exe",
    ]
    .iter()
    .chain(GAME_EXCLUSIONS.iter())
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
        // ё and е are one letter as far as anyone typing is concerned.
        let w = crate::model::normalize(word);
        self.exceptions
            .iter()
            .any(|e| crate::model::normalize(e) == w)
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

    /// Every hotkey that is switched on, with what it does.
    pub fn hotkeys(&self) -> Vec<(Hotkey, Shortcut)> {
        [
            (Hotkey::Main, self.shortcut),
            (Hotkey::ForceLayout(0), self.force_layout_1),
            (Hotkey::ForceLayout(1), self.force_layout_2),
            (Hotkey::AddException, self.add_exception_key),
        ]
        .into_iter()
        .filter(|(_, s)| *s != Shortcut::Off)
        .collect()
    }

    /// Check what the settings window sent: hotkeys that collide, and the
    /// abbreviation list's size and contents. Trims abbreviations in place.
    pub fn validate(&mut self) -> Result<(), String> {
        let keys = self.hotkeys();
        for (i, (_, a)) in keys.iter().enumerate() {
            if keys[i + 1..].iter().any(|(_, b)| a.collides_with(*b)) {
                return Err("Две горячие клавиши совпадают".into());
            }
        }
        if self.abbreviations.len() > MAX_ABBREVIATIONS {
            return Err(format!("Не больше {MAX_ABBREVIATIONS} сокращений"));
        }
        let mut seen: Vec<String> = Vec::new();
        for a in &mut self.abbreviations {
            a.short = a.short.trim().to_string();
            a.full = a.full.trim().to_string();
            let short_len = a.short.chars().count();
            if short_len == 0
                || short_len > MAX_ABBREVIATION_SHORT
                || a.short.chars().any(char::is_whitespace)
            {
                return Err("Сокращение — одно слово без пробелов, до 32 символов".into());
            }
            if a.full.is_empty()
                || a.full.chars().count() > MAX_ABBREVIATION_FULL
                || a.full.chars().any(|c| c.is_control() && c != '\n')
            {
                return Err("Полный текст — до 500 символов".into());
            }
            let key = a.short.to_lowercase();
            if seen.contains(&key) {
                return Err(format!("Сокращение «{}» уже есть", a.short));
            }
            seen.push(key);
        }
        Ok(())
    }

    /// The full text for a typed short form, ignoring case.
    pub fn abbreviation(&self, short: &str) -> Option<&str> {
        let key = short.to_lowercase();
        self.abbreviations
            .iter()
            .find(|a| a.short.to_lowercase() == key)
            .map(|a| a.full.as_str())
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
    fn fn_is_its_own_modifier() {
        let fn_only = Modifiers {
            function: true,
            ..Default::default()
        };
        assert!(Modifier::Fn.is_held(fn_only));
        assert!(!Modifier::Fn.others_held(fn_only));
        let with_cmd = Modifiers {
            meta: true,
            ..fn_only
        };
        assert!(Modifier::Fn.others_held(with_cmd));
        // Fn held while waiting for a right Command tap makes it a chord.
        let right_cmd = Modifiers {
            meta: true,
            right_meta: true,
            function: true,
            ..Default::default()
        };
        assert!(Modifier::RightCommand.others_held(right_cmd));
        let json = serde_json::to_string(&Shortcut::Tap {
            modifier: Modifier::Fn,
        })
        .unwrap();
        assert_eq!(json, r#"{"kind":"tap","modifier":"fn"}"#);
    }

    #[test]
    fn colliding_hotkeys_and_bad_abbreviations_are_refused() {
        let mut c = Config {
            force_layout_1: Shortcut::Tap {
                modifier: Modifier::platform_default(),
            },
            ..Config::default()
        };
        assert!(c.validate().is_err(), "same key as the main shortcut");
        c.force_layout_1 = Shortcut::DoubleTap {
            modifier: Modifier::platform_default(),
        };
        assert!(c.validate().is_err(), "one key, one hotkey");
        c.force_layout_1 = Shortcut::Tap {
            modifier: Modifier::RightOption,
        };
        assert!(c.validate().is_ok());
        c.abbreviations = vec![Abbreviation {
            short: " спс ".into(),
            full: " спасибо ".into(),
        }];
        assert!(c.validate().is_ok());
        assert_eq!(c.abbreviations[0].short, "спс");
        assert_eq!(c.abbreviation("СПС"), Some("спасибо"));
        c.abbreviations.push(Abbreviation {
            short: "Спс".into(),
            full: "x".into(),
        });
        assert!(c.validate().is_err(), "duplicate short form");
        c.abbreviations.pop();
        c.abbreviations.push(Abbreviation {
            short: "two words".into(),
            full: "x".into(),
        });
        assert!(c.validate().is_err());
    }

    #[test]
    fn new_behaviours_are_off_by_default() {
        let c = Config::default();
        assert!(!c.fix_two_caps && !c.correct_on_enter_tab && !c.fix_typos);
        assert!(!c.expand_abbreviations && !c.extended_dictionary);
        assert_eq!(c.force_layout_1, Shortcut::Off);
        assert_eq!(c.force_layout_2, Shortcut::Off);
        assert_eq!(c.add_exception_key, Shortcut::Off);
        assert_eq!(c.menu_bar_icon, MenuBarIcon::Logo);
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
