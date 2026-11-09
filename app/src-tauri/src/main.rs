// MarKey runs as a menu bar / tray app; on Windows this stops a console window
// from appearing behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! MarKey — fixes text typed on the wrong keyboard layout.

mod account;
mod assist;
mod cloud;
mod identity;
mod logging;
mod mar_report;
mod marhub_open;
mod private_files;
mod sealed;
mod secrets;
mod service;
mod settings;
#[cfg(test)]
mod testing;
mod tray_badge;
mod updater;

use rekey_core::config::Config;
use rekey_core::engine::Engine;
use rekey_core::model::{ExtendedWords, Models};
use service::{Runtime, SharedEngine};
use settings::Settings;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_deep_link::DeepLinkExt;

/// Everything the UI can read or change.
struct AppState {
    engine: SharedEngine,
    settings: Arc<Mutex<Settings>>,
    runtime: Arc<Runtime>,
    /// Whether the window material was applied, so the UI knows whether it can
    /// let the background show through.
    vibrancy: Mutex<bool>,
    /// Application icons already rendered, by bundle path.
    icons: Mutex<HashMap<String, Option<String>>>,
    /// The session counts already added to the saved totals.
    flushed: Mutex<rekey_core::engine::Stats>,
    /// The models as loaded, without the extended dictionary.
    base_models: Models,
    /// A newer version found by the last check, until the user acts on it.
    update: Mutex<Option<updater::Update>>,
    /// The extended dictionary, read from disk the first time it is used.
    extended: Mutex<Option<ExtendedWords>>,
    extended_dir: std::path::PathBuf,
    /// Taken around anything that replaces the configuration, so the cloud
    /// never writes back a merge of settings the user has just changed. Never
    /// held across a network call.
    config_gate: Mutex<()>,
    /// Moves with every change of the configuration, so a window that read an
    /// older copy cannot write it back over the change.
    config_revision: AtomicU64,
    account: Arc<account::Account>,
    cloud: Arc<cloud::Cloud>,
    reporter: mar_report::Reporter,
}

impl AppState {
    /// The models the engine should use for `config`.
    fn models_for(&self, config: &Config) -> Models {
        if !config.extended_dictionary {
            return self.base_models.clone();
        }
        let Ok(mut extended) = self.extended.lock() else {
            return self.base_models.clone();
        };
        let words = extended.get_or_insert_with(|| load_extended(&self.extended_dir));
        self.base_models.with_extended(words)
    }

    /// Words in the extended dictionary on disk (0 when it is missing).
    fn extended_size(&self) -> usize {
        self.extended
            .lock()
            .ok()
            .map(|mut e| {
                e.get_or_insert_with(|| load_extended(&self.extended_dir))
                    .len()
            })
            .unwrap_or(0)
    }
}

fn load_extended(dir: &std::path::Path) -> ExtendedWords {
    match ExtendedWords::load_dir(dir) {
        Ok(words) => {
            log::info!("extended dictionary: {} words", words.len());
            words
        }
        Err(e) => {
            log::warn!(
                "cannot read the extended dictionary in {}: {e}",
                dir.display()
            );
            ExtendedWords::default()
        }
    }
}

/// Add this session's new corrections to the totals kept across launches.
fn flush_stats(app: &AppHandle) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(stats) = state.engine.lock().map(|e| e.stats()) else {
        return;
    };
    let Ok(mut flushed) = state.flushed.lock() else {
        return;
    };
    if stats == *flushed {
        return;
    }
    if let Ok(mut settings) = state.settings.lock() {
        settings.totals.corrections += stats.corrections - flushed.corrections;
        settings.totals.undos += stats.undos - flushed.undos;
        settings.totals.manual += stats.manual - flushed.manual;
        if let Err(e) = settings.save() {
            log::warn!("cannot save settings: {e}");
        }
    }
    *flushed = stats;
}

/// A snapshot for the settings window.
#[derive(serde::Serialize)]
struct UiState {
    /// The detection settings, verbatim; the window sends the whole thing
    /// back when anything changes.
    config: Config,
    /// Which change of the configuration this copy reflects.
    config_revision: u64,
    /// One-word summary of whether corrections can happen right now.
    status: &'static str,
    has_api_key: bool,
    launch_at_login: bool,
    /// True when the assist is switched on *and* usable.
    assist_active: bool,
    assist_learned: usize,
    vibrancy: bool,
    corrections: u64,
    undos: u64,
    manual: u64,
    has_permission: bool,
    hook_running: bool,
    /// The keyboard filter can hold back keys (the best mode).
    can_hold: bool,
    available_layouts: Vec<LayoutInfo>,
    models_loaded: Vec<String>,
    /// Words in the bundled extended dictionary; 0 when it is missing.
    extended_words: usize,
    check_updates: bool,
    /// A newer version waiting in the update dialog.
    update: Option<updater::Update>,
    /// A one-off message from the keyboard side.
    notice: Option<String>,
    version: String,
    platform: String,
}

#[derive(serde::Serialize)]
struct LayoutInfo {
    id: String,
    name: String,
    language: String,
    /// Latin-script layouts share an alphabet with US QWERTY, so there is far
    /// less signal to work with. The UI says so rather than quietly
    /// underperforming.
    limited: bool,
}

fn layout_catalog() -> Vec<LayoutInfo> {
    rekey_core::layout::LAYOUTS
        .iter()
        .map(|d| LayoutInfo {
            id: d.id.to_string(),
            name: d.name.to_string(),
            language: d.lang.to_string(),
            limited: d.latin_overlap && d.id != "us",
        })
        .collect()
}

#[tauri::command]
fn get_state(state: State<'_, AppState>) -> UiState {
    // Before the configuration: a change in between then only makes a save
    // start over, instead of letting it overwrite that change.
    let config_revision = state.config_revision.load(Ordering::SeqCst);
    // Never hold engine and settings locks together in opposite orders.
    let (config, stats, assist_active, assist_learned, models_loaded) = {
        let engine = state.engine.lock().expect("engine lock");
        let mut langs: Vec<String> = engine
            .detector
            .models
            .langs()
            .iter()
            .map(|s| s.to_string())
            .collect();
        langs.sort();
        (
            engine.config().clone(),
            engine.stats(),
            engine.has_assist(),
            engine.assist_learned(),
            langs,
        )
    };
    let flushed = *state.flushed.lock().expect("stats lock");
    let settings = state.settings.lock().expect("settings lock");
    let permission = rekey_hook::platform::has_permission();
    let running = state.runtime.running.load(Ordering::Relaxed);
    let status = if !config.enabled {
        "paused"
    } else if !permission {
        "permission"
    } else if !running {
        "starting"
    } else if rekey_hook::platform::secure_input_active() {
        "secure_input"
    } else if state.runtime.tap_unstable() {
        "unstable"
    } else {
        "ready"
    };
    UiState {
        config_revision,
        status,
        has_api_key: !settings.api_key.trim().is_empty(),
        launch_at_login: settings.launch_at_login,
        assist_active,
        assist_learned,
        vibrancy: *state.vibrancy.lock().expect("vibrancy lock"),
        corrections: settings.totals.corrections + stats.corrections - flushed.corrections,
        undos: settings.totals.undos + stats.undos - flushed.undos,
        manual: settings.totals.manual + stats.manual - flushed.manual,
        has_permission: permission,
        hook_running: running,
        can_hold: state.runtime.can_hold.load(Ordering::Relaxed),
        available_layouts: layout_catalog(),
        models_loaded,
        extended_words: state.extended_size(),
        check_updates: settings.check_updates,
        update: state.update.lock().ok().and_then(|u| u.clone()),
        notice: state.runtime.notice.lock().ok().and_then(|mut n| n.take()),
        version: env!("CARGO_PKG_VERSION").to_string(),
        platform: std::env::consts::OS.to_string(),
        config,
    }
}

#[derive(serde::Serialize)]
struct AppChoice {
    name: String,
    bundle_id: String,
    /// Where the app is installed, for its icon. Empty when unknown.
    path: String,
    running: bool,
}

/// Installed and running applications, for the exclusion picker.
#[tauri::command]
fn list_apps() -> Vec<AppChoice> {
    #[cfg(target_os = "macos")]
    {
        let running = rekey_hook::platform::running_applications();
        let mut out: Vec<AppChoice> = rekey_hook::platform::installed_applications()
            .into_iter()
            .map(|a| AppChoice {
                running: running
                    .iter()
                    .any(|(_, id)| id.eq_ignore_ascii_case(&a.bundle_id)),
                name: a.name,
                bundle_id: a.bundle_id,
                path: a.path,
            })
            .collect();
        // Running apps that live somewhere unusual.
        for (name, id) in running {
            if !out.iter().any(|a| a.bundle_id.eq_ignore_ascii_case(&id)) {
                let path = rekey_hook::platform::application_path(&id).unwrap_or_default();
                out.push(AppChoice {
                    name,
                    bundle_id: id,
                    path,
                    running: true,
                });
            }
        }
        out.sort_by_key(|a| a.name.to_lowercase());
        out
    }
    #[cfg(target_os = "windows")]
    {
        // Windows identifies apps by executable; the picker offers whatever
        // has a window open right now.
        rekey_hook::platform::running_applications()
            .into_iter()
            .map(|(name, exe)| AppChoice {
                name,
                bundle_id: exe,
                path: String::new(),
                running: true,
            })
            .collect()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Vec::new()
    }
}

/// Name and install location for apps on the exclusion list that the picker
/// does not know, such as ones added by bundle id.
#[tauri::command]
fn describe_app(bundle_id: String) -> Option<AppChoice> {
    #[cfg(target_os = "macos")]
    {
        let path = rekey_hook::platform::application_path(&bundle_id)?;
        let name = std::path::Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| bundle_id.clone());
        Some(AppChoice {
            name,
            bundle_id,
            path,
            running: false,
        })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = bundle_id;
        None
    }
}

/// An application's icon as a `data:` URL, rendered once and cached.
#[tauri::command]
fn app_icon(state: State<'_, AppState>, path: String) -> Option<String> {
    if path.is_empty() || !path.ends_with(".app") {
        return None;
    }
    if let Some(cached) = state.icons.lock().ok()?.get(&path) {
        return cached.clone();
    }
    #[cfg(target_os = "macos")]
    let icon = rekey_hook::platform::application_icon_png(&path, 64.0)
        .map(|png| format!("data:image/png;base64,{}", base64(&png)));
    #[cfg(not(target_os = "macos"))]
    let icon: Option<String> = None;
    if let Ok(mut icons) = state.icons.lock() {
        icons.insert(path, icon.clone());
    }
    icon
}

/// Standard base64, for embedding icons. Small enough not to need a crate.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Settings changed in the UI. The whole config is replaced at once, which
/// keeps the UI and the engine from drifting apart field by field.
#[tauri::command]
fn set_config(
    app: AppHandle,
    state: State<'_, AppState>,
    config: Config,
    api_key: Option<String>,
    launch_at_login: Option<bool>,
    ai_consent: Option<bool>,
    base_revision: Option<u64>,
) -> Result<(), String> {
    {
        let _gate = state.config_gate.lock().map_err(|e| e.to_string())?;
        if base_revision.is_some_and(|b| b != state.config_revision.load(Ordering::SeqCst)) {
            return Err(STALE_CONFIG.to_string());
        }
        apply_config(&app, &state, config, api_key, launch_at_login, ai_consent)?;
        state.config_revision.fetch_add(1, Ordering::SeqCst);
    }
    state.cloud.changed();
    Ok(())
}

/// What `set_config` answers when the window read an older configuration; the
/// window reads again and repeats its change (save-queue.js).
const STALE_CONFIG: &str = "stale-config";

/// Validate a whole configuration and put it into effect: settings file,
/// engine, models and tray. The caller holds `config_gate`.
fn apply_config(
    app: &AppHandle,
    state: &AppState,
    mut config: Config,
    api_key: Option<String>,
    launch_at_login: Option<bool>,
    ai_consent: Option<bool>,
) -> Result<(), String> {
    config.excluded_apps = settings::normalize_exclusions(config.excluded_apps)?;
    config.validate()?;
    if config.layouts.is_empty() {
        return Err("Choose at least one layout".into());
    }
    let snapshot = {
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        let mut next = settings.clone();
        next.config = config.clone();
        if let Some(key) = api_key {
            let key = key.trim().to_string();
            // Into the Keychain / Credential Manager; the settings file never
            // holds it (see secrets.rs).
            if key.is_empty() {
                secrets::system().delete()?;
            } else {
                secrets::system().set(&key)?;
            }
            next.api_key = key;
        }
        if let Some(launch) = launch_at_login {
            next.launch_at_login = launch;
        }
        // Consent comes only from the dialog; switching the assist off
        // withdraws it, so turning it on again asks again.
        if let Some(consent) = ai_consent {
            next.ai_consent = consent;
        }
        if !config.ai_assist {
            next.ai_consent = false;
        }
        if config.ai_assist && !next.ai_consent {
            return Err("Сначала подтвердите отправку слов в Anthropic".into());
        }
        next.save().map_err(|e| e.to_string())?;
        *settings = next;
        settings.clone()
    };
    // Built outside the engine lock: attaching the dictionary copies the
    // models, and the keyboard hook needs the engine meanwhile.
    let extended_changed = state
        .engine
        .lock()
        .map(|e| e.config().extended_dictionary != config.extended_dictionary)
        .unwrap_or(false);
    let models = extended_changed.then(|| state.models_for(&config));
    {
        let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
        if let Some(models) = models {
            engine.detector.models = models;
        }
        engine.set_config(config);
        engine.invalidate_caret();
        // The assist is rebuilt rather than toggled, so switching it off drops
        // the worker and its cache instead of leaving them idling.
        attach_assist(&mut engine, &snapshot);
    }
    service::refresh_system_layouts(app);
    if let Some(launch) = launch_at_login {
        apply_launch_at_login(app, launch);
    }
    // Posted, because the cloud's worker comes through here too and the tray
    // asks AppKit which app is in front.
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || {
        refresh_tray(&handle);
        refresh_tray_icon(&handle);
    });
    Ok(())
}

// -- MarMic ID ---------------------------------------------------------------

fn account_view(state: &AppState) -> account::AccountView {
    let sync_words = state
        .settings
        .lock()
        .map(|s| s.cloud_sync_words)
        .unwrap_or(false);
    state.account.view(state.cloud.view(), sync_words)
}

#[tauri::command]
fn get_account(state: State<'_, AppState>) -> account::AccountView {
    account_view(&state)
}

/// «Войти». Off the main thread: opening MarHub waits for `open` to return.
#[tauri::command(async)]
fn account_sign_in(state: State<'_, AppState>) -> account::AccountView {
    state.account.sign_in();
    account_view(&state)
}

#[tauri::command]
fn account_cancel(state: State<'_, AppState>) -> account::AccountView {
    state.account.cancel();
    account_view(&state)
}

#[tauri::command(async)]
fn account_sign_out(state: State<'_, AppState>) -> account::AccountView {
    state.account.sign_out();
    account_view(&state)
}

/// Change the settings on a copy and keep it only once it is saved, so a
/// failed save cannot leave the app acting on something the file does not say.
fn commit_settings(
    current: &mut Settings,
    change: impl FnOnce(&mut Settings),
    save: impl FnOnce(&Settings) -> Result<(), String>,
) -> Result<(), String> {
    let mut next = current.clone();
    change(&mut next);
    save(&next)?;
    *current = next;
    Ok(())
}

/// Whether the exception words go to the cloud. Switching on fetches them
/// now; switching off leaves whatever is already there.
#[tauri::command]
fn set_cloud_words(
    app: AppHandle,
    state: State<'_, AppState>,
    on: bool,
) -> Result<account::AccountView, String> {
    // Without an account yet, the next sign-in, even a silent one, would
    // upload the list before anyone was asked in that account.
    if on && !state.account.is_signed_in() {
        return Err("signed out".to_string());
    }
    {
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        commit_settings(
            &mut settings,
            |next| next.cloud_sync_words = on,
            |next| next.save().map_err(|e| e.to_string()),
        )?;
    }
    if on {
        state.cloud.pull();
    }
    let _ = app.emit("account-changed", ());
    Ok(account_view(&state))
}

/// The real ways out of MarKey for sign-in links.
struct Opener(AppHandle);

impl account::LinkOpener for Opener {
    fn open_hub(&self, url: &str, silent: bool) -> bool {
        identity::hub_serves(&identity::base_url()) && marhub_open::open_hub(url, silent)
    }

    fn open_browser(&self, url: &str) -> bool {
        use tauri_plugin_opener::OpenerExt;
        self.0.opener().open_url(url, None::<&str>).is_ok()
    }
}

struct SettingsPrefs(Arc<Mutex<Settings>>);

impl account::Prefs for SettingsPrefs {
    fn signed_out_by_user(&self) -> bool {
        self.0.lock().map(|s| s.signed_out_by_user).unwrap_or(false)
    }

    fn set_signed_out_by_user(&self, value: bool) {
        let Ok(mut settings) = self.0.lock() else {
            return;
        };
        if settings.signed_out_by_user != value {
            settings.signed_out_by_user = value;
            if let Err(e) = settings.save() {
                log::warn!("cannot save settings: {e}");
            }
        }
    }
}

struct AccountHost(AppHandle);

impl account::AccountEvents for AccountHost {
    fn changed(&self) {
        let _ = self.0.emit("account-changed", ());
        if let Some(state) = self.0.try_state::<AppState>() {
            state.reporter.touch();
        }
    }

    fn signed_in(&self, interactive: bool) {
        if let Some(state) = self.0.try_state::<AppState>() {
            state.cloud.signed_in(interactive);
        }
        // The user asked and came back from MarHub or the browser. A silent
        // sign-in happens behind their back and must stay there.
        if interactive {
            let handle = self.0.clone();
            let _ = self.0.run_on_main_thread(move || show_settings(&handle));
        }
    }
}

struct CloudHost(AppHandle);

impl cloud::ConfigHost for CloudHost {
    fn config(&self) -> Option<Config> {
        let state = self.0.try_state::<AppState>()?;
        let engine = state.engine.lock().unwrap_or_else(|e| e.into_inner());
        Some(engine.config().clone())
    }

    fn apply(&self, change: &mut dyn FnMut(&mut Config) -> bool) -> Result<bool, String> {
        let state = self
            .0
            .try_state::<AppState>()
            .ok_or("the app is not ready")?;
        let _gate = state.config_gate.lock().map_err(|e| e.to_string())?;
        let mut config = state
            .engine
            .lock()
            .map_err(|e| e.to_string())?
            .config()
            .clone();
        if !change(&mut config) {
            return Ok(false);
        }
        apply_config(&self.0, &state, config, None, None, None)?;
        state.config_revision.fetch_add(1, Ordering::SeqCst);
        let _ = self.0.emit("config-changed", ());
        Ok(true)
    }

    fn sync_changed(&self) {
        let _ = self.0.emit("account-changed", ());
    }

    fn sync_words(&self) -> bool {
        self.0
            .try_state::<AppState>()
            .and_then(|state| state.settings.lock().ok().map(|s| s.cloud_sync_words))
            .unwrap_or(false)
    }

    fn set_sync_words(&self, on: bool) {
        let Some(state) = self.0.try_state::<AppState>() else {
            return;
        };
        {
            let Ok(mut settings) = state.settings.lock() else {
                return;
            };
            if settings.cloud_sync_words == on {
                return;
            }
            settings.cloud_sync_words = on;
            if let Err(e) = settings.save() {
                log::warn!("cannot save settings: {e}");
            }
        }
        let _ = self.0.emit("account-changed", ());
    }
}

/// What the report for MarHub should say right now.
fn report_snapshot(app: &AppHandle) -> Option<mar_report::Snapshot> {
    let state = app.try_state::<AppState>()?;
    let hook_running = state.runtime.running.load(Ordering::Relaxed);
    Some(mar_report::Snapshot {
        accessibility: rekey_hook::platform::has_permission(),
        input_monitoring: hook_running || rekey_hook::platform::has_input_monitoring(),
        signed_in: state.account.is_signed_in(),
    })
}

/// Links that reached MarKey, while running or at launch. A sign-in reply is
/// handled in the background and never brings up a window; only a bare
/// `markey://` does that.
fn handle_links(app: &AppHandle, links: Vec<String>) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    for raw in links {
        match identity::parse_link(&raw) {
            Some(identity::Link::Open) => {
                let handle = app.clone();
                let _ = app.run_on_main_thread(move || show_settings(&handle));
            }
            Some(reply) => state.account.accept(reply),
            None => log::info!("ignored a markey: link that is not valid"),
        }
    }
}

/// Attach or remove the Claude assist to match the current settings.
///
/// Requires both the setting *and* a key: an assist that can only fail is
/// worse than none, because it looks enabled while doing nothing.
fn attach_assist(engine: &mut Engine, settings: &Settings) {
    if !settings.config.ai_assist || !settings.ai_consent {
        engine.set_assist(None);
        return;
    }
    match assist::ClaudeAssist::start(&settings.api_key) {
        Some(assist) => {
            log::info!("Claude assist enabled for ambiguous words");
            engine.set_assist(Some(Arc::new(assist)));
        }
        None => {
            log::warn!("Claude assist is on but no API key is set; leaving it off");
            engine.set_assist(None);
        }
    }
}

/// The autostart plugin only opens the Run key, and a new Windows profile may
/// not have one yet: on Windows Server 2025 MarKey silently did not start at
/// login.
#[cfg(windows)]
fn ensure_run_key() {
    use windows::core::{w, PCWSTR};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE,
    };
    let mut key = HKEY::default();
    // SAFETY: the strings are static and `key` outlives the call; a key that
    // already exists is just opened.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut key,
            None,
        )
    };
    if status.is_ok() {
        // SAFETY: the key was opened just above and is closed once.
        let _ = unsafe { RegCloseKey(key) };
    } else {
        log::warn!("cannot create the autostart key: {status:?}");
    }
}

/// Is the setting on with no entry in the Run key? The uninstaller removes the
/// entry and keeps the settings, so after a reinstall the switch said "on" and
/// MarKey did not start at login. Task Manager's own off switch leaves the Run
/// value in place, so that choice is not undone here.
#[cfg(any(windows, test))]
fn login_entry_lost(wanted: bool, entry_present: bool) -> bool {
    wanted && !entry_present
}

#[cfg(windows)]
fn run_entry_present(app: &AppHandle) -> bool {
    use windows::core::{w, HSTRING, PCWSTR};
    use windows::Win32::Foundation::ERROR_SUCCESS;
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let name = HSTRING::from(app.package_info().name.as_str());
    // SAFETY: both strings outlive the call, and with no data pointer it only
    // reports whether the value is there.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Run"),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
    };
    status == ERROR_SUCCESS
}

fn apply_launch_at_login(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled {
        #[cfg(windows)]
        ensure_run_key();
        manager.enable()
    } else {
        manager.disable()
    };
    if let Err(e) = result {
        log::warn!("cannot change launch-at-login: {e}");
    }
}

/// Ask macOS for Accessibility access. The grant happens in System Settings;
/// the hook starts by itself once it does.
#[tauri::command]
fn request_permission() -> bool {
    rekey_hook::platform::request_permission()
}

#[tauri::command]
fn open_accessibility_settings(app: AppHandle) {
    use tauri_plugin_opener::OpenerExt;
    let _ = app.opener().open_url(
        "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
        None::<&str>,
    );
}

/// Try text against the current settings without typing it anywhere. Backs
/// the "try it" box in the settings window.
#[tauri::command]
fn preview(state: State<'_, AppState>, text: String, layout: String) -> PreviewResult {
    // A snapshot, so typing in the try box never holds the engine the
    // keyboard hook needs.
    let engine = state
        .engine
        .lock()
        .expect("engine lock")
        .detector_snapshot();
    let mut out = Vec::new();
    let mut changed = false;
    let mut previous_lang: Option<&'static str> = None;
    for token in text.split_whitespace() {
        let hint = rekey_core::detect::Hint { previous_lang };
        match engine.evaluate_with(token, &layout, hint) {
            rekey_core::detect::Verdict::Switch(c) => {
                previous_lang = rekey_core::layout::layout(c.to_layout).map(|l| l.def.lang);
                out.push(c.converted.clone());
                changed = true;
            }
            _ => {
                previous_lang = None;
                out.push(token.to_string());
            }
        }
    }
    PreviewResult {
        output: out.join(" "),
        changed,
    }
}

#[derive(serde::Serialize)]
struct PreviewResult {
    output: String,
    changed: bool,
}

/// Forget a word on the exception list.
#[tauri::command]
fn remove_exception(
    app: AppHandle,
    state: State<'_, AppState>,
    word: String,
) -> Result<(), String> {
    let mut config = state
        .engine
        .lock()
        .map_err(|e| e.to_string())?
        .config()
        .clone();
    config.exceptions.retain(|w| !w.eq_ignore_ascii_case(&word));
    set_config(app, state, config, None, None, None, None)
}

/// Look for a newer version. `manual` (the button) also shows a version the
/// user skipped before.
#[tauri::command(async)]
fn check_update(
    state: State<'_, AppState>,
    manual: bool,
) -> Result<Option<updater::Update>, String> {
    let found = updater::check(&updater::feed_url(), env!("CARGO_PKG_VERSION"))
        .map_err(|e| format!("Не удалось проверить обновления: {e}"))?;
    let skipped = state
        .settings
        .lock()
        .ok()
        .and_then(|s| s.skipped_version.clone());
    let found = found.filter(|u| manual || skipped.as_deref() != Some(u.version.as_str()));
    if let Ok(mut pending) = state.update.lock() {
        pending.clone_from(&found);
    }
    Ok(found)
}

/// «Позже»: close the dialog; the next check offers it again.
#[tauri::command]
fn dismiss_update(state: State<'_, AppState>) {
    if let Ok(mut pending) = state.update.lock() {
        *pending = None;
    }
}

/// «Пропустить»: never offer this version again by itself.
#[tauri::command]
fn skip_update(state: State<'_, AppState>, version: String) -> Result<(), String> {
    // Only the version actually on offer can be skipped.
    let pending = pending_update(&state)?;
    if pending.version != version {
        return Err("Это не та версия, что предлагается".into());
    }
    if let Ok(mut pending) = state.update.lock() {
        *pending = None;
    }
    let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
    settings.skipped_version = Some(version);
    settings.save().map_err(|e| e.to_string())
}

#[tauri::command]
fn set_update_checks(app: AppHandle, state: State<'_, AppState>, on: bool) -> Result<(), String> {
    {
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        settings.check_updates = on;
        settings.save().map_err(|e| e.to_string())?;
    }
    if on {
        spawn_update_checks(&app);
    }
    Ok(())
}

/// One download or install at a time: a second click waits for nothing and
/// is refused.
static UPDATE_BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct UpdateGuard;

impl UpdateGuard {
    fn take() -> Result<UpdateGuard, String> {
        UPDATE_BUSY
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .map(|_| UpdateGuard)
            .map_err(|_| "Обновление уже скачивается".to_string())
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        UPDATE_BUSY.store(false, Ordering::SeqCst);
    }
}

fn pending_update(state: &AppState) -> Result<updater::Update, String> {
    state
        .update
        .lock()
        .ok()
        .and_then(|u| u.clone())
        .ok_or_else(|| "Обновление не найдено — проверьте ещё раз".to_string())
}

/// «Скачать»: the verified installer into Downloads, shown in Finder.
#[tauri::command(async)]
fn download_update(app: AppHandle, state: State<'_, AppState>) -> Result<String, String> {
    use tauri_plugin_opener::OpenerExt;
    let _busy = UpdateGuard::take()?;
    let update = pending_update(&state)?;
    let dir = dirs::download_dir().unwrap_or_else(updater::cache_dir);
    let (path, _) = updater::download_verified(&update, &dir)?;
    let _ = app.opener().reveal_item_in_dir(&path);
    Ok(path.display().to_string())
}

/// «Установить и перезапустить»: download, verify, replace, relaunch.
#[tauri::command(async)]
fn install_update(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let _busy = UpdateGuard::take()?;
    let update = pending_update(&state)?;
    let (path, verified) = updater::download_verified(&update, &updater::cache_dir())?;
    updater::install(&path, &verified)?;
    log::info!("update {} installed; restarting", update.version);
    flush_stats(&app);
    app.exit(0);
    Ok(())
}

/// With update checks on: one check shortly after launch, then daily. A
/// newer version opens the settings window, where the dialog shows it.
fn spawn_update_checks(app: &AppHandle) {
    use std::sync::atomic::AtomicBool;
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    let handle = app.clone();
    std::thread::Builder::new()
        .name("markey-updates".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(15));
            while let Some(state) = handle.try_state::<AppState>() {
                let on = state
                    .settings
                    .lock()
                    .map(|s| s.check_updates)
                    .unwrap_or(false);
                if !on {
                    break;
                }
                match check_update(state, false) {
                    Ok(Some(update)) => {
                        log::info!("update {} is available", update.version);
                        let window = handle.clone();
                        let _ = handle.run_on_main_thread(move || show_settings(&window));
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("{e}"),
                }
                std::thread::sleep(std::time::Duration::from_secs(24 * 60 * 60));
            }
            RUNNING.store(false, Ordering::SeqCst);
        })
        .ok();
}

#[tauri::command]
fn open_log(app: AppHandle) {
    use tauri_plugin_opener::OpenerExt;
    let path = settings::config_dir().join("markey.log");
    let _ = app.opener().reveal_item_in_dir(path);
}

/// Give the window the system's own background material.
///
/// Returns whether it actually took effect. A compositor can refuse, so the
/// stylesheet only drops its opaque background when this succeeds.
fn apply_window_material(window: &tauri::WebviewWindow) -> bool {
    use tauri::window::{Effect, EffectState, EffectsBuilder};

    #[cfg(target_os = "macos")]
    let candidates = [Effect::Sidebar, Effect::UnderWindowBackground];
    #[cfg(target_os = "windows")]
    let candidates = [Effect::Mica, Effect::Acrylic];
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: [Effect; 0] = [];

    for effect in candidates {
        let effects = EffectsBuilder::new()
            .effect(effect)
            .state(EffectState::FollowsWindowActiveState)
            .build();
        if window.set_effects(effects).is_ok() {
            log::info!("window material: {effect:?}");
            return true;
        }
    }
    log::info!("no window material available; using a solid background");
    false
}

fn show_settings(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("settings") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Persist a configuration the engine changed on its own, and let the window
/// know.
fn store_config(app: &AppHandle, settings: &Arc<Mutex<Settings>>, config: Config) {
    let Some(state) = app.try_state::<AppState>() else {
        save_config(app, settings, config);
        return;
    };
    let Ok(_gate) = state.config_gate.lock() else {
        return;
    };
    // Read again under the gate: a cloud merge may have replaced the engine's
    // configuration since `config` was taken.
    let current = state
        .engine
        .lock()
        .map(|e| e.config().clone())
        .unwrap_or(config);
    save_config(app, settings, current);
}

/// Write `config` to the settings file. The caller holds `config_gate`, once
/// there is one.
fn save_config(app: &AppHandle, settings: &Arc<Mutex<Settings>>, config: Config) {
    if let Ok(mut s) = settings.lock() {
        s.config = config;
        if let Err(e) = s.save() {
            log::warn!("cannot save settings: {e}");
        }
    }
    if let Some(state) = app.try_state::<AppState>() {
        state.config_revision.fetch_add(1, Ordering::SeqCst);
        state.cloud.changed();
    }
    let handle = app.clone();
    let _ = app.run_on_main_thread(move || refresh_tray(&handle));
}

/// The tray items whose text or state follows the settings.
struct TrayItems {
    toggle: CheckMenuItem<tauri::Wry>,
    exclude: MenuItem<tauri::Wry>,
}

/// Bring the tray menu in line with the current settings and frontmost app.
fn refresh_tray(app: &AppHandle) {
    let (Some(items), Some(state)) = (app.try_state::<TrayItems>(), app.try_state::<AppState>())
    else {
        return;
    };
    let config = match state.engine.lock() {
        Ok(e) => e.config().clone(),
        Err(_) => return,
    };
    let _ = items.toggle.set_checked(config.enabled);
    let label = match frontmost_app_name(app) {
        Some((name, id)) if config.is_excluded_app(&id) => format!("Включить в «{name}»"),
        Some((name, _)) => format!("Не исправлять в «{name}»"),
        None => "Не исправлять в текущем приложении".to_string(),
    };
    let _ = items.exclude.set_text(label);
}

/// The menu bar icon: the MarKey keycap, or the current layout's badge.
///
/// # Must be called on the main thread
pub(crate) fn refresh_tray_icon(app: &AppHandle) {
    let (Some(tray), Some(state)) = (app.tray_by_id("markey"), app.try_state::<AppState>()) else {
        return;
    };
    let (icon, size) = match state.engine.lock() {
        Ok(e) => (e.config().menu_bar_icon, e.config().menu_bar_size),
        Err(_) => return,
    };
    let layout = state.runtime.layout.lock().ok().and_then(|l| l.clone());
    let template = cfg!(target_os = "macos");
    let image = match (icon, layout) {
        (rekey_core::config::MenuBarIcon::Layout, Some(layout))
            if !tray_badge::code_for(&layout).is_empty() =>
        {
            let (rgba, w, h) = tray_badge::render(&layout, size, template);
            tauri::image::Image::new_owned(rgba, w, h)
        }
        _ => match default_tray_icon() {
            Ok(image) => image,
            Err(_) => return,
        },
    };
    let _ = tray.set_icon(Some(image));
    let _ = tray.set_icon_as_template(template);
}

fn default_tray_icon() -> tauri::Result<tauri::image::Image<'static>> {
    // macOS tints a template icon to match the menu bar. Windows draws icons
    // as they are, and a black template would vanish on a dark taskbar.
    #[cfg(target_os = "macos")]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/trayTemplate@2x.png"))?;
    #[cfg(not(target_os = "macos"))]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
    Ok(icon)
}

/// The app the user was working in. MarKey never becomes frontmost itself —
/// it is a menu bar app — so this is the app behind the menu.
fn frontmost_app_name(app: &AppHandle) -> Option<(String, String)> {
    // The app the user was last working in: on Windows, clicking the tray
    // icon itself hands focus to the taskbar.
    let id = app
        .try_state::<AppState>()
        .and_then(|s| s.runtime.last_app.lock().ok().and_then(|l| l.clone()))
        .or_else(rekey_hook::platform::frontmost_app)?;
    if id.eq_ignore_ascii_case("com.marmos.markey") || id.eq_ignore_ascii_case("markey.exe") {
        return None;
    }
    #[cfg(target_os = "macos")]
    let name = rekey_hook::platform::application_path(&id)
        .and_then(|p| {
            std::path::Path::new(&p)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| id.clone());
    #[cfg(not(target_os = "macos"))]
    let name = id
        .strip_suffix(".exe")
        .or_else(|| id.strip_suffix(".EXE"))
        .unwrap_or(&id)
        .to_string();
    Some((name, id))
}

fn update_config(app: &AppHandle, change: impl FnOnce(&mut Config)) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let Ok(_gate) = state.config_gate.lock() else {
        return;
    };
    let config = {
        let Ok(mut engine) = state.engine.lock() else {
            return;
        };
        let mut config = engine.config().clone();
        change(&mut config);
        engine.set_config(config.clone());
        engine.invalidate_caret();
        config
    };
    save_config(app, &state.settings, config);
}

fn build_tray(app: &AppHandle, engine: SharedEngine) -> tauri::Result<()> {
    let enabled = engine.lock().expect("engine lock").config().enabled;

    let toggle = CheckMenuItem::with_id(
        app,
        "toggle",
        "Исправлять раскладку",
        true,
        enabled,
        None::<&str>,
    )?;
    let exclude = MenuItem::with_id(
        app,
        "exclude",
        "Не исправлять в текущем приложении",
        true,
        None::<&str>,
    )?;
    let settings_item =
        MenuItem::with_id(app, "settings", "Настройки…", true, Some("CmdOrCtrl+,"))?;
    let undo_item = MenuItem::with_id(
        app,
        "undo",
        "Отменить последнее исправление",
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Выйти из MarKey", true, Some("CmdOrCtrl+Q"))?;
    let menu = Menu::with_items(
        app,
        &[
            &toggle,
            &exclude,
            &PredefinedMenuItem::separator(app)?,
            &undo_item,
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;
    app.manage(TrayItems {
        toggle: toggle.clone(),
        exclude: exclude.clone(),
    });

    let icon = default_tray_icon()?;
    TrayIconBuilder::with_id("markey")
        .icon(icon)
        .icon_as_template(cfg!(target_os = "macos"))
        .tooltip("MarKey")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "toggle" => update_config(app, |c| c.enabled = !c.enabled),
            "exclude" => {
                if let Some((name, id)) = frontmost_app_name(app) {
                    log::info!("tray: toggling the exclusion of the frontmost app");
                    update_config(app, |c| {
                        if c.is_excluded_app(&id) {
                            c.excluded_apps.retain(|a| !a.eq_ignore_ascii_case(&id));
                        } else {
                            c.excluded_apps.push(id.clone());
                        }
                    });
                    let _ = name;
                }
            }
            "undo" => {
                if let Some(state) = app.try_state::<AppState>() {
                    let action = state
                        .engine
                        .lock()
                        .map(|mut e| e.undo())
                        .unwrap_or(rekey_core::engine::Action::None);
                    state.runtime.perform(action);
                }
            }
            "settings" => show_settings(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            match event {
                // The menu shows what it will act on before it opens.
                TrayIconEvent::Enter { .. } | TrayIconEvent::Move { .. } => {
                    refresh_tray(tray.app_handle())
                }
                TrayIconEvent::DoubleClick { .. } => show_settings(tray.app_handle()),
                _ => {}
            }
        })
        .build(app)?;
    Ok(())
}

fn main() {
    tauri::Builder::default()
        // Registered first, so a second launch exits before doing anything.
        // A launch carrying a link is that link arriving, which comes through
        // the deep-link plugin and decides for itself about the window.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            if !identity::carries_link(&args) {
                show_settings(app);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_state,
            list_apps,
            describe_app,
            app_icon,
            set_config,
            request_permission,
            open_accessibility_settings,
            preview,
            remove_exception,
            open_log,
            check_update,
            dismiss_update,
            skip_update,
            set_update_checks,
            download_update,
            install_update,
            get_account,
            account_sign_in,
            account_cancel,
            account_sign_out,
            set_cloud_words
        ])
        .setup(|app| {
            // Here, past the single-instance plugin: a second copy only passes
            // on its arguments and leaves, and starting the log rotates it.
            // Each markey:// link on Windows is such a copy.
            logging::init(settings::config_dir().join("markey.log"));

            // Tauri sets NSApplicationActivationPolicyRegular by default, which
            // overrides LSUIElement — so the app would appear in the Dock.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let handle = app.handle().clone();
            let first_run = !settings::config_path().exists();
            let mut settings = Settings::load();
            // A layout switcher is only useful if it is always there, so a new
            // installation starts at login; the setting turns it off.
            if first_run && !settings.launch_at_login {
                settings.launch_at_login = true;
                apply_launch_at_login(&handle, true);
            }
            #[cfg(windows)]
            if login_entry_lost(settings.launch_at_login, run_entry_present(&handle)) {
                log::info!("the autostart entry is gone while the setting is on; restoring it");
                apply_launch_at_login(&handle, true);
            }
            if let Err(e) = settings.save() {
                log::warn!("cannot save settings: {e}");
            }

            let dir = settings::models_dir(&handle);
            let models = Models::load_dir(&dir).unwrap_or_else(|e| {
                log::error!("cannot load language models from {}: {e}", dir.display());
                Models::new()
            });
            if models.is_empty() {
                log::error!(
                    "no language models found in {} — corrections are disabled",
                    dir.display()
                );
            }

            let extended_dir = settings::extended_dir(&handle);
            let base_models = models.clone();
            let mut extended_words = None;
            let models = if settings.config.extended_dictionary {
                let words = load_extended(&extended_dir);
                let with = models.with_extended(&words);
                extended_words = Some(words);
                with
            } else {
                models
            };
            let check_updates = settings.check_updates;
            let mut engine_inner = Engine::new(models, settings.config.clone());
            attach_assist(&mut engine_inner, &settings);
            let engine: SharedEngine = Arc::new(Mutex::new(engine_inner));
            let settings = Arc::new(Mutex::new(settings));

            let sink_handle = handle.clone();
            let sink_settings = settings.clone();
            let on_config: service::ConfigSink =
                Arc::new(move |config| store_config(&sink_handle, &sink_settings, config));
            // The hook waits for Accessibility rather than failing, so this
            // only errors if the platform layer itself is unavailable.
            let runtime = match service::spawn(&handle, engine.clone(), on_config) {
                Ok(runtime) => runtime,
                Err(e) => {
                    log::warn!("keyboard hook unavailable: {e}");
                    Arc::new(Runtime::default())
                }
            };

            let vibrancy = match handle.get_webview_window("settings") {
                Some(window) => apply_window_material(&window),
                None => false,
            };

            build_tray(&handle, engine.clone())?;

            let identity_base = identity::base_url();
            let account = Arc::new(account::Account::new(account::Deps {
                client: Arc::new(identity::Client::new(&identity_base)),
                store: sealed::system(),
                clock: Arc::new(account::SystemClock),
                opener: Arc::new(Opener(handle.clone())),
                prefs: Arc::new(SettingsPrefs(settings.clone())),
                events: Arc::new(AccountHost(handle.clone())),
            }));
            let cloud = cloud::Cloud::new(
                &identity_base,
                cloud::sync_file_path(),
                account.clone(),
                Arc::new(CloudHost(handle.clone())),
            );
            let report_handle = handle.clone();
            let reporter = mar_report::Reporter::spawn(move || report_snapshot(&report_handle));

            app.manage(AppState {
                engine,
                settings,
                runtime,
                vibrancy: Mutex::new(vibrancy),
                icons: Mutex::new(HashMap::new()),
                flushed: Mutex::new(Default::default()),
                base_models,
                extended: Mutex::new(extended_words),
                extended_dir,
                update: Mutex::new(None),
                config_gate: Mutex::new(()),
                config_revision: AtomicU64::new(0),
                account: account.clone(),
                cloud: cloud.clone(),
                reporter,
            });
            app.state::<AppState>().reporter.touch();

            let links = handle.clone();
            app.deep_link().on_open_url(move |event| {
                handle_links(
                    &links,
                    event.urls().iter().map(ToString::to_string).collect(),
                );
            });
            // Also covers a build that was not put in place by the installer.
            #[cfg(windows)]
            if let Err(e) = app.deep_link().register_all() {
                log::warn!("cannot register markey: links: {e}");
            }
            // The link MarKey was launched with. On macOS this arrives as an
            // event instead, moments later.
            match app.deep_link().get_current() {
                Ok(Some(current)) => {
                    handle_links(&handle, current.iter().map(ToString::to_string).collect())
                }
                Ok(None) => {}
                Err(_) => log::warn!("cannot read the link MarKey was launched with"),
            }

            cloud.spawn();
            if account.is_signed_in() {
                cloud.pull();
            }
            account.start();
            if check_updates {
                spawn_update_checks(&handle);
            }
            refresh_tray(&handle);

            // Keep the totals current without writing on every keystroke.
            let saver = handle.clone();
            std::thread::Builder::new()
                .name("markey-stats".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(60));
                    flush_stats(&saver);
                })
                .ok();

            // First run, or permission still missing: show the window so the
            // user is not left with a silent menu bar icon.
            if first_run || !rekey_hook::platform::has_permission() {
                show_settings(&handle);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the settings window hides it; the app lives in the tray.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("cannot start MarKey")
        .run(|app, event| match event {
            // This is a menu bar app that happens to have a settings window.
            tauri::RunEvent::ExitRequested { api, code, .. } if code.is_none() => {
                api.prevent_exit();
            }
            tauri::RunEvent::Exit => {
                flush_stats(app);
                if let (Some(state), Some(snapshot)) =
                    (app.try_state::<AppState>(), report_snapshot(app))
                {
                    state.reporter.write_final(snapshot);
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rekey_core::config::{Modifier, Sensitivity, Shortcut};

    #[test]
    fn a_lost_autostart_entry_is_restored_only_when_the_setting_is_on() {
        assert!(login_entry_lost(true, false), "removed by the uninstaller");
        assert!(!login_entry_lost(true, true));
        assert!(!login_entry_lost(false, false), "turned off on purpose");
        assert!(!login_entry_lost(false, true));
    }

    #[test]
    fn the_windows_installer_speaks_russian_first() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            conf["bundle"]["windows"]["nsis"]["languages"],
            serde_json::json!(["Russian", "English"])
        );
    }

    #[test]
    fn a_failed_save_leaves_the_words_switch_as_it_was() {
        let mut settings = Settings::default();
        let result = commit_settings(
            &mut settings,
            |next| next.cloud_sync_words = true,
            |_| Err("disk full".to_string()),
        );
        assert_eq!(result, Err("disk full".to_string()));
        assert!(!settings.cloud_sync_words);
    }

    #[test]
    fn a_saved_change_to_the_words_switch_takes_effect() {
        let mut settings = Settings::default();
        let seen = std::cell::Cell::new(false);
        let result = commit_settings(
            &mut settings,
            |next| next.cloud_sync_words = true,
            |next| {
                seen.set(next.cloud_sync_words);
                Ok(())
            },
        );
        assert_eq!(result, Ok(()));
        assert!(seen.get(), "the file gets the new value");
        assert!(settings.cloud_sync_words);
    }

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn every_shortcut_round_trips_through_json() {
        for modifier in [
            Modifier::Option,
            Modifier::RightOption,
            Modifier::RightCommand,
            Modifier::Command,
            Modifier::Shift,
            Modifier::RightShift,
            Modifier::Control,
            Modifier::RightControl,
        ] {
            for shortcut in [Shortcut::Tap { modifier }, Shortcut::DoubleTap { modifier }] {
                let json = serde_json::to_string(&shortcut).unwrap();
                assert_eq!(serde_json::from_str::<Shortcut>(&json).unwrap(), shortcut);
            }
        }
        let _ = Sensitivity::Balanced;
    }
}
