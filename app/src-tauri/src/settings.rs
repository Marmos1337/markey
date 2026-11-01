//! Where MarKey's settings live on disk, and how they are loaded.

use crate::secrets::SecretStore;
use rekey_core::config::{Config, Modifier, Shortcut};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Bumped whenever loading needs to adjust settings written by an older build.
const SETTINGS_VERSION: u32 = 4;

/// The bundle identifier, which names the settings folder.
const APP_ID: &str = "com.marmos.markey";

/// Where the app kept its settings when it was still called Rekey.
const LEGACY_APP_ID: &str = "app.rekey.desktop";

/// Settings plus the things that are not part of the detection config.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub config: Config,
    /// Anthropic API key for the optional assist, held in memory only. It is
    /// kept in the Keychain / Credential Manager ([`crate::secrets`]) and never
    /// written to the settings file; it is still *read* from the file, so a key
    /// left there by an older build can be moved out.
    #[serde(skip_serializing)]
    pub api_key: String,
    pub launch_at_login: bool,
    /// Which build last wrote these settings; see [`SETTINGS_VERSION`].
    pub version: u32,
    /// Running totals across launches, for the settings window.
    pub totals: Totals,
}

/// How much MarKey has done, ever.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Totals {
    pub corrections: u64,
    pub undos: u64,
    pub manual: u64,
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

/// Where the settings file is, and where an older build may have left one.
#[derive(Debug, Clone)]
pub struct Paths {
    pub current: PathBuf,
    pub legacy: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths {
            current: config_path(),
            legacy: legacy_config_path(),
        }
    }

    fn tmp(&self) -> PathBuf {
        self.current.with_extension("json.tmp")
    }
}

impl Settings {
    /// Load settings, falling back to defaults for anything unreadable.
    ///
    /// A corrupt settings file must never stop the app from starting: the user
    /// would have no way to fix it from inside the UI.
    pub fn load() -> Settings {
        Settings::load_from(&Paths::system(), crate::secrets::system().as_ref())
    }

    /// [`Settings::load`] with explicit locations and key store, for tests.
    ///
    /// An API key found in a settings file (written by an older build) is
    /// moved into `secrets`, and every file that held it — the current one,
    /// the Rekey-era one, a stray temp file — is rewritten without it. If the
    /// store refuses, the key stays in memory for this session and the file is
    /// left alone but made private, so nothing is lost; the move is retried at
    /// the next launch.
    pub fn load_from(paths: &Paths, secrets: &dyn SecretStore) -> Settings {
        let mut path = paths.current.clone();
        let imported = !path.exists() && paths.legacy.exists();
        // First launch after the rename: carry the old settings over.
        if imported {
            log::info!("importing settings from the Rekey build");
            path = paths.legacy.clone();
        }
        let mut settings = Settings::read(&path);
        settings.migrate();
        make_private(&paths.current);

        let found = std::mem::take(&mut settings.api_key);
        let found = found.trim().to_string();
        if found.is_empty() {
            settings.api_key = match secrets.get() {
                Ok(key) => key.unwrap_or_default(),
                Err(e) => {
                    log::warn!("cannot read the API key: {e}");
                    String::new()
                }
            };
            // A temp file from an interrupted save may still hold an old key.
            scrub_file(&paths.tmp());
            return settings;
        }
        match secrets.set(&found) {
            Ok(()) => {
                log::info!("moved the API key out of the settings file into secure storage");
                settings.api_key = found;
                if let Err(e) = settings.save_to(paths) {
                    log::warn!("cannot rewrite settings without the key: {e}");
                }
                for stale in [&paths.legacy, &paths.tmp()] {
                    scrub_file(stale);
                }
            }
            Err(e) => {
                log::warn!("cannot move the API key into secure storage ({e}); keeping it for now");
                make_private(&path);
                settings.api_key = found;
            }
        }
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
                    modifier: Modifier::platform_default(),
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
        if self.version < 3
            && cfg!(windows)
            && self.config.shortcut
                == (Shortcut::Tap {
                    modifier: Modifier::RightCommand,
                })
        {
            // 0.1 offered right Command on Windows too, where it is the Windows
            // key and its tap opens Start.
            self.config.shortcut = Shortcut::Tap {
                modifier: Modifier::platform_default(),
            };
        }
        if self.version < 4 {
            // 0.3 made two behaviours settings of their own, off for new
            // installations. Whoever had them keeps them: Return and Tab
            // corrected words, and fix_case also fixed ПРивет.
            self.config.correct_on_enter_tab = true;
            self.config.fix_two_caps = self.config.fix_case;
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
        self.save_to(&Paths::system())
    }

    /// Never contains the API key (`skip_serializing`), and the file is 0600.
    pub fn save_to(&self, paths: &Paths) -> std::io::Result<()> {
        if let Some(dir) = paths.current.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        // Write beside and rename, so a crash mid-write never leaves a
        // truncated settings file behind.
        let tmp = paths.tmp();
        write_private(&tmp, &text)?;
        std::fs::rename(tmp, &paths.current)
    }
}

/// Rewrite a settings file without its `api_key`, or remove it when it is not
/// a settings file at all (a half-written temp file).
fn scrub_file(path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(serde_json::Value::Object(mut map)) => {
            if map.remove("api_key").is_some() {
                let clean = serde_json::to_string_pretty(&map).unwrap_or_default();
                if write_private(path, &clean).is_err() {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        _ => {
            if text.contains("api_key") {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Write `text` to a file only the user can read.
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    make_private(path);
    Ok(())
}

/// Settings written by older builds were 0644; tighten them.
fn make_private(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
    }
    #[cfg(not(unix))]
    let _ = path;
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
        let json = serde_json::to_string(&settings).unwrap();
        assert!(
            !json.contains("test-only-placeholder"),
            "the API key must never be serialized"
        );
        let loaded: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            loaded.config.excluded_apps,
            vec!["com.apple.TextEdit", "org.mozilla.firefox"]
        );
        assert_eq!(loaded.api_key, "");
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
    use crate::secrets::MemoryStore;

    fn temp_paths(name: &str) -> Paths {
        let dir =
            std::env::temp_dir().join(format!("markey-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old")).unwrap();
        Paths {
            current: dir.join("settings.json"),
            legacy: dir.join("old").join("settings.json"),
        }
    }

    /// Every file under the settings folder, as text.
    fn everything(paths: &Paths) -> String {
        let dir = paths.current.parent().unwrap();
        let mut all = String::new();
        for path in [paths.current.clone(), paths.legacy.clone(), paths.tmp()] {
            all += &std::fs::read_to_string(path).unwrap_or_default();
        }
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            if entry.path().is_file() {
                all += &std::fs::read_to_string(entry.path()).unwrap_or_default();
            }
        }
        all
    }

    const KEY: &str = "sk-ant-test-0000-plaintext";

    #[test]
    fn a_plaintext_key_moves_to_secure_storage_and_leaves_no_copy() {
        let paths = temp_paths("move");
        let old = format!(r#"{{"version":3,"api_key":"{KEY}","launch_at_login":true}}"#);
        std::fs::write(&paths.current, &old).unwrap();
        // A temp file from a save interrupted by an older build.
        std::fs::write(paths.tmp(), &old).unwrap();
        let store = MemoryStore::default();

        let settings = Settings::load_from(&paths, &store);
        assert_eq!(
            settings.api_key, KEY,
            "the key is still usable this session"
        );
        assert_eq!(store.get().unwrap().as_deref(), Some(KEY));
        assert!(
            !everything(&paths).contains(KEY),
            "a plaintext copy was left behind"
        );
        assert!(settings.launch_at_login, "other settings survive");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.current)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // Next launch reads it from the store, and saving never writes it.
        let again = Settings::load_from(&paths, &store);
        assert_eq!(again.api_key, KEY);
        again.save_to(&paths).unwrap();
        assert!(!everything(&paths).contains(KEY));
        let _ = std::fs::remove_dir_all(paths.current.parent().unwrap());
    }

    #[test]
    fn the_rekey_era_file_is_scrubbed_too() {
        let paths = temp_paths("legacy");
        std::fs::write(&paths.legacy, format!(r#"{{"api_key":"{KEY}"}}"#)).unwrap();
        let store = MemoryStore::default();
        let settings = Settings::load_from(&paths, &store);
        assert_eq!(settings.api_key, KEY);
        assert!(!everything(&paths).contains(KEY));
        let _ = std::fs::remove_dir_all(paths.current.parent().unwrap());
    }

    #[test]
    fn a_refusing_store_keeps_the_key_rather_than_losing_it() {
        let paths = temp_paths("refuse");
        std::fs::write(&paths.current, format!(r#"{{"api_key":"{KEY}"}}"#)).unwrap();
        let store = MemoryStore {
            fail: true,
            ..MemoryStore::default()
        };
        let settings = Settings::load_from(&paths, &store);
        assert_eq!(settings.api_key, KEY);
        // Left in place, to be moved next time, but private.
        assert!(std::fs::read_to_string(&paths.current)
            .unwrap()
            .contains(KEY));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.current)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(paths.current.parent().unwrap());
    }

    #[test]
    fn the_old_option_default_moves_to_right_command() {
        let mut s: Settings =
            serde_json::from_str(r#"{"config":{"shortcut":{"kind":"tap","modifier":"option"}}}"#)
                .unwrap();
        s.migrate();
        assert_eq!(
            s.config.shortcut,
            Shortcut::Tap {
                modifier: Modifier::platform_default()
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
        // 0.2 behaviour is kept for 0.2 users.
        let mut s: Settings =
            serde_json::from_str(r#"{"version":3,"config":{"fix_case":true}}"#).unwrap();
        s.migrate();
        assert!(s.config.correct_on_enter_tab && s.config.fix_two_caps);
        let mut s: Settings =
            serde_json::from_str(r#"{"version":3,"config":{"fix_case":false}}"#).unwrap();
        s.migrate();
        assert!(!s.config.fix_two_caps);
        // A new installation starts with them off.
        let fresh = Settings::read(std::path::Path::new("/nonexistent/markey/settings.json"));
        let mut fresh = fresh;
        fresh.migrate();
        assert!(!fresh.config.correct_on_enter_tab && !fresh.config.fix_two_caps);
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
