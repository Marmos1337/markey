// MarKey runs as a menu bar / tray app; on Windows this stops a console window
// from appearing behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! MarKey — fixes text typed on the wrong keyboard layout.

mod assist;
mod service;
mod settings;

use rekey_core::config::Config;
use rekey_core::engine::Engine;
use rekey_core::model::Models;
use service::{Runtime, SharedEngine};
use settings::Settings;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, State};

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
    } else {
        "ready"
    };
    UiState {
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
    mut config: Config,
    api_key: Option<String>,
    launch_at_login: Option<bool>,
) -> Result<(), String> {
    config.excluded_apps = settings::normalize_exclusions(config.excluded_apps)?;
    if config.layouts.is_empty() {
        return Err("Choose at least one layout".into());
    }
    let snapshot = {
        let mut settings = state.settings.lock().map_err(|e| e.to_string())?;
        let mut next = settings.clone();
        next.config = config.clone();
        if let Some(key) = api_key {
            next.api_key = key;
        }
        if let Some(launch) = launch_at_login {
            next.launch_at_login = launch;
        }
        next.save().map_err(|e| e.to_string())?;
        *settings = next;
        settings.clone()
    };
    {
        let mut engine = state.engine.lock().map_err(|e| e.to_string())?;
        engine.set_config(config);
        engine.invalidate_caret();
        // The assist is rebuilt rather than toggled, so switching it off drops
        // the worker and its cache instead of leaving them idling.
        attach_assist(&mut engine, &snapshot);
    }
    service::refresh_system_layouts(&app);
    if let Some(launch) = launch_at_login {
        apply_launch_at_login(&app, launch);
    }
    refresh_tray(&app);
    Ok(())
}

/// Attach or remove the Claude assist to match the current settings.
///
/// Requires both the setting *and* a key: an assist that can only fail is
/// worse than none, because it looks enabled while doing nothing.
fn attach_assist(engine: &mut Engine, settings: &Settings) {
    if !settings.config.ai_assist {
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

fn apply_launch_at_login(app: &AppHandle, enabled: bool) {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled {
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
    let engine = state.engine.lock().expect("engine lock");
    let mut out = Vec::new();
    let mut changed = false;
    let mut previous_lang: Option<&'static str> = None;
    for token in text.split_whitespace() {
        let hint = rekey_core::detect::Hint { previous_lang };
        match engine.detector.evaluate_with(token, &layout, hint) {
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
    set_config(app, state, config, None, None)
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
    if let Ok(mut s) = settings.lock() {
        s.config = config;
        if let Err(e) = s.save() {
            log::warn!("cannot save settings: {e}");
        }
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
    store_config(app, &state.settings, config);
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

    // macOS tints a template icon to match the menu bar. Windows draws icons
    // as they are, and a black template would vanish on a dark taskbar.
    #[cfg(target_os = "macos")]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/trayTemplate@2x.png"))?;
    #[cfg(not(target_os = "macos"))]
    let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;
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

/// Send logs to a file as well as stderr.
///
/// A menu bar app launched from Finder has nowhere to write stderr, so the one
/// time the log matters — a user reporting that something misbehaved — there
/// is nothing to read. The file is small, truncated each launch, and lives
/// beside the settings. It never records what is typed: entries note
/// decisions and lengths, never the words.
fn init_logging() {
    use std::io::Write;

    let path = settings::config_dir().join("markey.log");
    let file = std::fs::create_dir_all(settings::config_dir())
        .ok()
        .and_then(|_| std::fs::File::create(&path).ok());

    let mut builder = env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,rekey=debug,rekey_core=debug,rekey_hook=debug"),
    );
    if let Some(file) = file {
        let file = std::sync::Mutex::new(file);
        builder.format(move |buf, record| {
            let line = format!(
                "[{} {} {}] {}",
                timestamp(),
                record.level(),
                record.target(),
                record.args()
            );
            if let Ok(mut file) = file.lock() {
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
            writeln!(buf, "{line}")
        });
    }
    builder.init();
    log::info!("logging to {}", path.display());
}

/// Seconds since the Unix epoch, with milliseconds.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

fn main() {
    init_logging();

    tauri::Builder::default()
        // Registered first, so a second launch exits before doing anything.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_settings(app);
        }))
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
            open_log
        ])
        .setup(|app| {
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

            app.manage(AppState {
                engine,
                settings,
                runtime,
                vibrancy: Mutex::new(vibrancy),
                icons: Mutex::new(HashMap::new()),
                flushed: Mutex::new(Default::default()),
            });
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
            tauri::RunEvent::Exit => flush_stats(app),
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rekey_core::config::{Modifier, Sensitivity, Shortcut};

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
