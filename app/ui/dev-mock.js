// Development stand-in for the Tauri bridge.
//
// Loaded before app.js and inert whenever real Tauri is present. It lets the
// settings window be opened in an ordinary browser — for styling, for checking
// light and dark rendering, and for capturing screenshots — without rebuilding
// the Rust side. The responses mirror the shapes returned by the `#[tauri::command]`
// functions in src/main.rs; if you change one there, change it here too.

/**
 * True only for a plain dev server on localhost.
 *
 * Tauri serves the UI from `tauri://localhost` on macOS and
 * `http://tauri.localhost` on Windows, so neither matches. Checking merely
 * that `window.__TAURI__` is missing is not enough: if the bridge ever fails
 * to inject, the app would quietly show invented statistics instead of
 * failing, which is far worse than showing nothing.
 */
function isLocalDevServer() {
  const httpish = location.protocol === "http:" || location.protocol === "https:";
  const local = location.hostname === "localhost" || location.hostname === "127.0.0.1";
  return httpish && local;
}

if (!window.__TAURI__ && isLocalDevServer()) {
  const LAYOUTS = [
    ["us", "English (US QWERTY)", "en", false],
    ["ru", "Russian (ЙЦУКЕН)", "ru", false],
    ["uk", "Ukrainian (ЙЦУКЕН)", "uk", false],
    ["he", "Hebrew", "he", false],
    ["ar", "Arabic (101)", "ar", false],
    ["el", "Greek", "el", false],
    ["de", "German (QWERTZ)", "de", true],
    ["fr", "French (AZERTY)", "fr", true],
    ["es", "Spanish (QWERTY)", "es", true],
    ["tr", "Turkish (Q)", "tr", true],
  ];

  // Enough of the real conversion to make the preview box honest in the demo.
  const SAMPLES = {
    "ghbdtn": "привет",
    "rfr": "как",
    "ltkf": "дела",
    "cgfcb,j": "спасибо",
    "yjhv": "норм",
    "руддщ": "hello",
    "ghbdtn,": "привет,",
    "ltkf?": "дела?",
    "z": "я",
    "nen": "тут",
  };

  const state = {
    config: {
      enabled: true,
      layouts: ["us", "ru"],
      sensitivity: "balanced",
      min_word_len: 2,
      require_dict_below_len: 4,
      exceptions: ["ghbdtn", "vs"],
      excluded_apps: [
        "com.apple.keychainaccess",
        "com.1password.1password",
        "com.apple.Terminal",
        "com.googlecode.iterm2",
        "com.microsoft.VSCode",
      ],
      skip_all_caps: true,
      ai_assist: false,
      shortcut: { kind: "tap", modifier: "right_command" },
      switch_layout: true,
      convert_selection: true,
      convert_word_before_caret: true,
      sound: false,
    },
    status: "ready",
    has_api_key: false,
    launch_at_login: true,
    assist_active: false,
    assist_learned: 0,
    vibrancy: false,
    corrections: 1284,
    undos: 3,
    manual: 41,
    has_permission: true,
    hook_running: true,
    can_hold: true,
    available_layouts: LAYOUTS.map(([id, name, language, limited]) => ({
      id, name, language, limited,
    })),
    models_loaded: ["ar", "de", "el", "en", "es", "fr", "he", "ru", "tr", "uk"],
    version: "0.2.0",
    // Follow the host so the dev server previews the right skin.
    platform: globalThis.navigator?.userAgent?.includes("Windows")
      ? "windows"
      : "macos",
  };

  const APPS = [
    ["Safari", "com.apple.Safari", true],
    ["Telegram", "ru.keepcoder.Telegram", true],
    ["Visual Studio Code", "com.microsoft.VSCode", true],
    ["Терминал", "com.apple.Terminal", false],
    ["Заметки", "com.apple.Notes", false],
    ["Slack", "com.tinyspeck.slackmacgap", false],
    ["iTerm", "com.googlecode.iterm2", false],
    ["Связка ключей", "com.apple.keychainaccess", false],
    ["1Password", "com.1password.1password", false],
    ["Firefox", "org.mozilla.firefox", false],
  ].map(([name, bundle_id, running]) => ({ name, bundle_id, path: "", running }));

  const handlers = {
    get_state: () => structuredClone(state),
    list_apps: () => structuredClone(APPS),
    describe_app: () => null,
    app_icon: () => null,
    set_config: ({ config, launchAtLogin, apiKey }) => {
      state.config = structuredClone(config);
      state.status = config.enabled ? "ready" : "paused";
      if (launchAtLogin !== undefined) state.launch_at_login = launchAtLogin;
      if (apiKey) state.has_api_key = true;
    },
    request_permission: () => true,
    open_accessibility_settings: () => {},
    open_log: () => {},
    remove_exception: () => {},
    preview: ({ text }) => {
      let changed = false;
      const output = text
        .split(/\s+/)
        .map((word) => {
          const fixed = SAMPLES[word.toLowerCase()];
          if (fixed) changed = true;
          return fixed ?? word;
        })
        .join(" ");
      return { output, changed };
    },
  };

  window.__TAURI__ = {
    core: {
      invoke: async (name, args = {}) => {
        const handler = handlers[name];
        if (!handler) throw new Error(`no mock for command ${name}`);
        return handler(args);
      },
    },
  };
  console.info("MarKey UI running against the development mock.");
}
