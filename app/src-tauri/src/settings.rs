//! Where MarKey's settings live on disk, and how they are loaded.

use rekey_core::config::{Config, Modifier, Shortcut};
use std::path::PathBuf;

/// Bumped whenever loading needs to adjust settings written by an older build.
const SETTINGS_VERSION: u32 = 3;

/// The bundle identifier, which names the settings folder.
const APP_ID: &str = "com.marmos.markey";

/// Where the app kept its settings when it was still called Rekey.
const LEGACY_APP_ID: &str = "app.rekey.desktop";

/// Settings plus the things that are not part of the detection config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub config: Config,
    /// Anthropic API key for the optional assist. Stored in the app's own
    /// config directory; empty unless the user pastes one in.
    pub api_key: String,
    pub launch_at_login: bool,
    /// Which build last wrote these settings; see [`SETTINGS_VERSION`].
    pub version: u32,
}

/// `~/Library/Application Support/com.marmos.markey` (macOS) or
/// `%APPDATA%\com.marmos.markey` (Windows).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_ID)
}

fn legacy_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(LEGACY_APP_ID)
        .join("settings.json")
}

pub fn config_path() -> PathBuf {
    config_dir().join("settings.json")
}

impl Settings {
    /// Load settings, falling back to defaults for anything unreadable.
    ///
    /// A corrupt settings file must never stop the app from starting: the user
    /// would have no way to fix it from inside the UI.
    pub fn load() -> Settings {
        let mut path = config_path();
        // First launch after the rename: carry the old settings over.
        if !path.exists() && legacy_config_path().exists() {
            log::info!("importing settings from the Rekey build");
            path = legacy_config_path();
        }
        let mut settings = Settings::read(&path);
        settings.migrate();
        settings
    }

    /// Bring settings written by an older build up to date.
    fn migrate(&mut self) {
        if self.version < 2 {
            // Older builds defaulted to tapping Option, which collides with
            // ⌥⌫ and ⌥←. A tap of right Command is the new default; anyone who
            // picked something else keeps their choice.
            if self.config.shortcut
                == (Shortcut::Tap {
                    modifier: Modifier::Option,
                })
            {
                self.config.shortcut = Shortcut::Tap {
                    modifier: Modifier::RightCommand,
                };
            }
            for id in Config::default().excluded_apps {
                if !self.config.is_excluded_app(&id) {
                    self.config.excluded_apps.push(id);
                }
            }
        }
        if self.version < 3 && self.config.min_word_len == 2 {
            // One-letter words (я, а, и, в) are corrected since 0.2.
            self.config.min_word_len = 1;
        }
        self.version = SETTINGS_VERSION;
    }

    fn read(path: &std::path::Path) -> Settings {
        match std::fs::read_to_string(path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(settings) => settings,
                Err(e) => {
                    log::warn!(
                        "settings at {} are unreadable ({e}); using defaults",
                        path.display()
                    );
                    Settings::default()
                }
            },
            Err(_) => Settings {
                version: SETTINGS_VERSION,
                ..Settings::default()
            },
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let text = serde_json::to_string_pretty(self)?;
        // Write beside and rename, so a crash mid-write never leaves a
        // truncated settings file behind.
        let tmp = config_path().with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, config_path())
    }
}

/// Find the trained language models.
///
/// Tried in order: the bundled resource directory, then the source tree, so a
/// development build works without installing anything.
pub fn models_dir(app: &tauri::AppHandle) -> PathBuf {
    use tauri::Manager;
    if let Ok(resources) = app.path().resource_dir() {
        let bundled = resources.join("models");
        if bundled.is_dir() {
            return bundled;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../crates/rekey-core/data/models")
}

/// Canonical bundle IDs/executable IDs, preserving the user's existing list.
pub fn normalize_exclusions(ids: Vec<String>) -> Result<Vec<String>, String> {
    if ids.len() > 512 {
        return Err("Too many app exceptions".into());
    }
    let mut out = Vec::<String>::new();
    for id in ids {
        let id = id.trim();
        if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control) {
            return Err("Invalid app identifier".into());
        }
        if !out.iter().any(|old| old.eq_ignore_ascii_case(id)) {
            out.push(id.to_string());
        }
    }
    Ok(out)
}
#[cfg(test)]
mod exclusion_tests {
    use super::*;
    #[test]
    fn identifiers_round_trip_and_deduplicate_without_touching_other_settings() {
        let mut settings = Settings {
            api_key: "test-only-placeholder".into(),
            launch_at_login: false,
            ..Settings::default()
        };
        settings.config.excluded_apps = normalize_exclusions(vec![
            " com.apple.TextEdit ".into(),
            "COM.APPLE.TEXTEDIT".into(),
            "org.mozilla.firefox".into(),
        ])
        .unwrap();
        let loaded: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(
            loaded.config.excluded_apps,
            vec!["com.apple.TextEdit", "org.mozilla.firefox"]
        );
        assert_eq!(loaded.api_key, "test-only-placeholder");
        assert!(!loaded.launch_at_login);
        assert!(!loaded.config.ai_assist);
        assert!(loaded.config.is_excluded_app("COM.APPLE.TEXTEDIT"));
    }
    #[test]
    fn invalid_identifiers_are_rejected() {
        for id in ["", " ", "a\nb"] {
            assert!(normalize_exclusions(vec![id.into()]).is_err());
        }
        assert!(normalize_exclusions(vec!["x".repeat(257)]).is_err());
    }
    #[test]
    fn removing_exception_enables_only_selected_app() {
        let mut config = Config {
            excluded_apps: vec!["com.apple.TextEdit".into(), "org.mozilla.firefox".into()],
            ..Config::default()
        };
        config.excluded_apps.retain(|id| id != "com.apple.TextEdit");
        assert!(!config.is_excluded_app("com.apple.TextEdit"));
        assert!(config.is_excluded_app("org.mozilla.firefox"));
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn the_old_option_default_moves_to_right_command() {
        let mut s: Settings =
            serde_json::from_str(r#"{"config":{"shortcut":{"kind":"tap","modifier":"option"}}}"#)
                .unwrap();
        s.migrate();
        assert_eq!(
            s.config.shortcut,
            Shortcut::Tap {
                modifier: Modifier::RightCommand
            }
        );
        assert_eq!(s.version, SETTINGS_VERSION);
        // A deliberate choice is kept.
        let mut s: Settings = serde_json::from_str(
            r#"{"config":{"shortcut":{"kind":"doubletap","modifier":"shift"}}}"#,
        )
        .unwrap();
        s.migrate();
        assert_eq!(
            s.config.shortcut,
            Shortcut::DoubleTap {
                modifier: Modifier::Shift
            }
        );
        // And current settings are left alone.
        let mut s: Settings = serde_json::from_str(
            r#"{"version":2,"config":{"shortcut":{"kind":"tap","modifier":"option"}}}"#,
        )
        .unwrap();
        s.migrate();
        assert_eq!(
            s.config.shortcut,
            Shortcut::Tap {
                modifier: Modifier::Option
            }
        );
    }
}
