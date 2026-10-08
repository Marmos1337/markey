//! Where Rekey's settings live on disk, and how they are loaded.

use rekey_core::config::Config;
use std::path::PathBuf;

/// Settings plus the things that are not part of the detection config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub config: Config,
    /// Anthropic API key for the optional assist. Stored in the app's own
    /// config directory; empty unless the user pastes one in.
    pub api_key: String,
    pub launch_at_login: bool,
}

/// `~/Library/Application Support/app.rekey.desktop` (macOS) or
/// `%APPDATA%\app.rekey.desktop` (Windows).
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("app.rekey.desktop")
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
        let path = config_path();
        match std::fs::read_to_string(&path) {
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
            Err(_) => Settings::default(),
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let dir = config_dir();
        std::fs::create_dir_all(&dir)?;
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(config_path(), text)
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
        let mut config = Config::default();
        config.excluded_apps = vec!["com.apple.TextEdit".into(), "org.mozilla.firefox".into()];
        config.excluded_apps.retain(|id| id != "com.apple.TextEdit");
        assert!(!config.is_excluded_app("com.apple.TextEdit"));
        assert!(config.is_excluded_app("org.mozilla.firefox"));
    }
}
