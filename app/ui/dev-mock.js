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
      fix_case: true,
      fix_two_caps: false,
      correct_on_enter_tab: false,
      fix_typos: false,
      expand_abbreviations: !globalThis.location?.search?.includes("empty"),
      abbreviations: globalThis.location?.search?.includes("empty")
        ? []
        : [
            { short: "спс", full: "спасибо" },
            { short: "пжл", full: "пожалуйста" },
            { short: "мб", full: "может быть" },
            { short: "адр", full: "Москва, ул. Тверская, д. 7, кв. 12" },
          ],
      extended_dictionary: false,
      force_layout_1: { kind: "off" },
      force_layout_2: globalThis.location?.search?.includes("hotkeys")
        ? { kind: "tap", modifier: "fn" }
        : { kind: "off" },
      add_exception_key: { kind: "off" },
      menu_bar_icon: globalThis.location?.search?.includes("badge") ? "layout" : "logo",
      menu_bar_size: "medium",
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
    extended_words: 18068,
    check_updates: false,
    update: null,
    version: "0.2.0",
    // Follow the host so the dev server previews the right skin.
    platform: globalThis.location?.search?.includes("windows") || globalThis.navigator?.userAgent?.includes("Windows")
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

  // A release as the GitHub feed would describe it, for the update dialog.
  const UPDATE = {
    version: "0.3.0",
    current: "0.2.0",
    notes_ru: "- Расширенный словарь: имена, города, бренды и новые слова\n- Горячие клавиши «Всегда раскладка 1 / 2» и «В исключения», клавиша Fn\n- Сокращения: «спс» → «спасибо»\n- Исправление при Return и Tab, две заглавные, явные опечатки\n- Раскладка в строке меню, три размера\n- Перехват клавиатуры больше не отключается системой при нагрузке",
    notes_en: "- Extended dictionary: names, places, brands and new words\n- Hotkeys for layout 1 / 2 and for exceptions, the Fn key\n- Abbreviations\n- The keyboard tap no longer gets disabled under load",
    page: "https://github.com/Marmos1337/MarKey/releases/tag/v0.3.0",
    asset_name: "MarKey_0.3.0_aarch64.dmg",
    size: 9437184,
  };
  if (globalThis.location?.search?.includes("update")) state.update = UPDATE;
  const later = (ms, value) => new Promise((resolve) => setTimeout(() => resolve(value), ms));

  // The account, chosen from the URL so every state can be opened directly:
  //   ?account=signedOut|pending|signedIn|error
  //   &via=marhub|browser   where a pending sign-in is waiting
  //   &error=denied|network|rejected|server|expired|open|session|storage
  //   &sync=idle|syncing|error|refused|off
  //   &long=1     a very long name
  //   &avatar=1   a picture instead of initials
  //   &words=1    the exception words already go to the cloud
  //   &sets=0     no sets at all (the real app always names its sets)
  // From the console or a test: __markeyMock.set({...}), .emit(name), .merge(),
  // .mergeApp(id), .failNext(command, message).
  const query = new URLSearchParams(globalThis.location?.search ?? "");
  const startAs = query.get("account") ?? "signedOut";

  const initialsOf = (name) => {
    const [first, second] = name.split(/\s+/);
    return (second ? first[0] + second[0] : first.slice(0, 2)).toUpperCase();
  };

  const PROFILE = {
    name: query.has("long") ? "Александра-Мария Константиновна Преображенская-Верхнеднепровская" : "Иван Петров",
    avatar: query.has("avatar")
      ? "data:image/svg+xml," +
        encodeURIComponent('<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" fill="#6b7280"/><circle cx="32" cy="25" r="12" fill="#e5e7eb"/><rect x="12" y="42" width="40" height="30" rx="15" fill="#e5e7eb"/></svg>')
      : null,
  };

  const ACCOUNT = {
    accountName: "MarMic ID",
    status: startAs === "error" ? "signedOut" : startAs,
    via: startAs === "pending" ? query.get("via") ?? "marhub" : null,
    error: query.get("error") ?? (startAs === "error" ? "network" : null),
    name: startAs === "signedIn" ? PROFILE.name : null,
    avatar: startAs === "signedIn" ? PROFILE.avatar : null,
    syncWords: query.has("words"),
    syncState: query.get("sync") ?? "idle",
  };

  const minutesAgo = (n) => new Date(Date.now() - n * 60000).toISOString();

  /** What `get_account` returns right now, built the way account.rs builds it. */
  function accountView() {
    const { syncState, ...fields } = ACCOUNT;
    const signedIn = ACCOUNT.status === "signedIn";
    const sets = [{ key: "rules", title: "Правила исправления", count: 15, updatedAt: minutesAgo(12) }];
    if (ACCOUNT.syncWords) {
      sets.push({ key: "word-exceptions", title: "Слова-исключения", count: state.config.exceptions.length, updatedAt: minutesAgo(1600) });
    }
    sets.push({ key: "app-exceptions", title: "Приложения-исключения", count: 3, updatedAt: null });
    return structuredClone({
      ...fields,
      initials: ACCOUNT.name ? initialsOf(ACCOUNT.name) : null,
      sync: signedIn
        ? { state: syncState, sets: query.get("sets") === "0" ? [] : sets }
        : { state: "off", sets: [] },
    });
  }

  const listeners = new Map();
  const emit = (name) => {
    for (const handler of listeners.get(name) ?? []) handler({ event: name, payload: null });
  };

  function changeAccount(patch) {
    Object.assign(ACCOUNT, patch);
    emit("account-changed");
    return accountView();
  }

  let signInTimer = null;

  /** What a sign-in ends in here: confirmed in MarHub after a moment. */
  function finishSignIn() {
    changeAccount({ status: "signedIn", via: null, error: null, name: PROFILE.name, avatar: PROFILE.avatar, syncState: "syncing" });
    setTimeout(() => changeAccount({ syncState: "idle" }), 1200);
  }

  /** Command name -> what its next call should fail with. */
  const failures = new Map();

  window.__markeyMock = {
    set: changeAccount,
    emit,
    failNext: (command, message) => failures.set(command, message),
    // A cloud merge: the stored settings change while the window is open.
    merge: (word = "облако") => {
      state.config.exceptions = [...state.config.exceptions, word];
      emit("config-changed");
    },
    mergeApp: (id) => {
      state.config.excluded_apps = [...state.config.excluded_apps, id];
      emit("config-changed");
    },
  };

  const handlers = {
    get_state: () => structuredClone(state),
    list_apps: () => structuredClone(APPS),
    describe_app: () => null,
    app_icon: () => null,
    check_update: () => later(700, (state.update = UPDATE)),
    dismiss_update: () => {
      state.update = null;
    },
    skip_update: () => {
      state.update = null;
    },
    set_update_checks: ({ on }) => {
      state.check_updates = on;
    },
    download_update: () => later(1200, "/Users/you/Downloads/MarKey_0.3.0_aarch64.dmg"),
    install_update: () =>
      later(1200).then(() => {
        throw "В режиме разработки ничего не устанавливается.";
      }),
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
    get_account: () => accountView(),
    account_sign_in: () => {
      if (ACCOUNT.status === "signedIn") return accountView();
      clearTimeout(signInTimer);
      signInTimer = setTimeout(finishSignIn, 2400);
      return changeAccount({ status: "pending", via: query.get("via") ?? "marhub", error: null });
    },
    account_cancel: () => {
      clearTimeout(signInTimer);
      return changeAccount({ status: "signedOut", via: null, error: null });
    },
    account_sign_out: () => {
      clearTimeout(signInTimer);
      return changeAccount({ status: "signedOut", via: null, error: null, name: null, avatar: null });
    },
    set_cloud_words: ({ on }) => {
      if (on && ACCOUNT.status !== "signedIn") throw "signed out";
      setTimeout(() => changeAccount({ syncState: "idle" }), 900);
      return changeAccount({ syncWords: on, syncState: "syncing" });
    },
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
        if (failures.has(name)) {
          const message = failures.get(name);
          failures.delete(name);
          throw message;
        }
        return handler(args);
      },
    },
    event: {
      listen: async (name, handler) => {
        const handlers = listeners.get(name) ?? new Set();
        handlers.add(handler);
        listeners.set(name, handlers);
        return () => handlers.delete(handler);
      },
    },
  };
  console.info("MarKey UI running against the development mock.");
}
