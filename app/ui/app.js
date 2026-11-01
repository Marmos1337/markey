// MarKey settings window.
//
// The window is a thin view over the engine: it reads one snapshot from Rust,
// and every change sends the whole config back. Keeping one source of truth on
// the Rust side avoids the UI and the engine drifting apart.

// Provided by Tauri (`withGlobalTauri`), or by dev-mock.js on a dev server.
const bridge = window.__TAURI__;
const invoke = bridge?.core?.invoke;

const $ = (id) => document.getElementById(id);

const PAGE_TITLES = {
  general: "Основное",
  shortcut: "Горячие клавиши",
  apps: "Исключения",
  languages: "Языки",
  words: "Слова",
  abbreviations: "Сокращения",
  advanced: "Дополнительно",
};

/** The modifiers on offer, right-hand keys first: they are rarely in chords. */
const KEYS = [
  { id: "right_command", symbol: "⌘", label: "Правый Command", short: "правый ⌘" },
  { id: "right_option", symbol: "⌥", label: "Правый Option", short: "правый ⌥" },
  { id: "right_shift", symbol: "⇧", label: "Правый Shift", short: "правый ⇧" },
  { id: "right_control", symbol: "⌃", label: "Правый Control", short: "правый ⌃" },
  { id: "option", symbol: "⌥", label: "Option", short: "⌥" },
  { id: "shift", symbol: "⇧", label: "Shift", short: "⇧" },
  { id: "control", symbol: "⌃", label: "Control", short: "⌃" },
  // The Fn / 🌐 key exists only on Mac keyboards.
  { id: "fn", symbol: "🌐", label: "Fn", short: "Fn", mac: true },
  { id: "off", symbol: "Выкл.", label: "Без клавиши", short: "" },
];

/** The keys this platform can use. */
function availableKeys() {
  return KEYS.filter((k) => !k.mac || state?.platform !== "windows");
}

/** The hotkeys besides the main one: config field, title, what it does. */
const EXTRA_HOTKEYS = [
  { field: "force_layout_1", layout: 0, detail: "Переводит набранное или выделенное в эту раскладку и переключает клавиатуру." },
  { field: "force_layout_2", layout: 1, detail: "То же для второй раскладки — когда MarKey угадал не ту." },
  { field: "add_exception_key", title: "Добавить слово в исключения", detail: "Слово у курсора больше не исправляется. Если MarKey его только что исправил — вернёт как было." },
];

const WINDOWS_NAMES = {
  right_command: ["⊞", "Правый Win", "правый Win"],
  right_option: ["Alt", "Правый Alt", "правый Alt"],
  right_shift: ["⇧", "Правый Shift", "правый Shift"],
  right_control: ["Ctrl", "Правый Ctrl", "правый Ctrl"],
  option: ["Alt", "Alt", "Alt"],
  shift: ["⇧", "Shift", "Shift"],
  control: ["Ctrl", "Ctrl", "Ctrl"],
};

const SENSITIVITY_HINTS = {
  cautious: "Только при подавляющей уверенности. Почти не ошибается, но пропускает больше.",
  balanced: "Исправляет, когда другое прочтение — явно настоящее слово. Рекомендуется.",
  eager: "Исправляет и при более слабых признаках. Ловит больше сленга, иногда перегибает.",
};

/** Native names for the layout list. */
const NATIVE = {
  us: ["Английский", "English"],
  ru: ["Русский", "Русский"],
  uk: ["Украинский", "Українська"],
  he: ["Иврит", "עברית"],
  ar: ["Арабский", "العربية"],
  el: ["Греческий", "Ελληνικά"],
  de: ["Немецкий", "Deutsch"],
  fr: ["Французский", "Français"],
  es: ["Испанский", "Español"],
  tr: ["Турецкий", "Türkçe"],
};

const STATUS = {
  ready: ["Исправление включено", "Работает во всех приложениях, кроме исключённых."],
  paused: ["Исправление на паузе", "MarKey ничего не исправляет. Включите переключателем справа."],
  permission: ["Нужен доступ", "Без Универсального доступа MarKey не видит клавиатуру."],
  starting: ["Запускается…", "Подключение к клавиатуре."],
  secure_input: ["Поле пароля", "Пока активен ввод пароля, MarKey ничего не трогает."],
  unstable: ["Система отключает перехват", "macOS несколько раз за минуту выключала перехват клавиатуры. MarKey включает его снова; подробности — в журнале."],
};

/** Last snapshot from Rust; the basis for every save. */
let state = null;
let page = "general";
let apps = [];
let appsLoaded = false;
let selectedApp = null;

function keyNames(id) {
  const key = KEYS.find((k) => k.id === id);
  if (state?.platform === "windows" && WINDOWS_NAMES[id]) {
    const [symbol, label, short] = WINDOWS_NAMES[id];
    return { ...key, symbol, label, short };
  }
  return key;
}

function plural(n, one, few, many) {
  const mod10 = n % 10;
  const mod100 = n % 100;
  if (mod10 === 1 && mod100 !== 11) return one;
  if (mod10 >= 2 && mod10 <= 4 && (mod100 < 12 || mod100 > 14)) return few;
  return many;
}

const number = new Intl.NumberFormat("ru-RU");

// -- navigation -------------------------------------------------------------

function showPage(name) {
  page = name;
  for (const el of document.querySelectorAll(".page")) {
    el.hidden = el.dataset.page !== name;
  }
  for (const el of document.querySelectorAll(".nav-item")) {
    el.classList.toggle("active", el.dataset.page === name);
    el.setAttribute("aria-current", el.dataset.page === name ? "page" : "false");
  }
  $("page-title").textContent = PAGE_TITLES[name] ?? "";
  document.querySelector(".pages").scrollTop = 0;
  try {
    localStorage.setItem("markey.page", name);
  } catch {
    // Storage can be unavailable; the page simply is not remembered.
  }
  if (name === "apps") loadApps();
}

for (const el of document.querySelectorAll(".nav-item")) {
  el.addEventListener("click", () => showPage(el.dataset.page));
}

// -- state ------------------------------------------------------------------

async function refresh() {
  state = await invoke("get_state");
  render();
}

/**
 * Refresh only what changes on its own while the user types elsewhere. The
 * window never rebuilds its controls on a timer: that would clobber an edit
 * in progress.
 */
async function pollLiveFields() {
  const fresh = await invoke("get_state");
  for (const key of ["status", "corrections", "undos", "manual", "has_permission", "hook_running", "can_hold", "update"]) {
    state[key] = fresh[key];
  }
  if (state.update && state.update.version !== shownUpdate) openUpdate(state.update);
  $("update-dot").hidden = !state.update;
  if (fresh.config.enabled !== state.config.enabled || fresh.config.excluded_apps.length !== state.config.excluded_apps.length || fresh.config.exceptions.length !== state.config.exceptions.length) {
    state = fresh;
    render();
    return;
  }
  renderStatus();
  renderStats();
}

/** Where the API key is kept, in words. */
function keyStore() {
  return state?.platform === "windows" ? "в Диспетчере учётных данных" : "в Связке ключей";
}

/** Words that differ between the Mac and Windows. */
function renderPlatformText() {
  const windows = state.platform === "windows";
  for (const el of document.querySelectorAll(".key-store")) el.textContent = keyStore();
  $("open-log-label").textContent = windows ? "Показать в Проводнике" : "Показать в Finder";
  $("copy-chord").textContent = windows ? "Ctrl+C" : "⌘C";
  $("apps-tray-hint").textContent = windows
    ? "значка MarKey в области уведомлений"
    : "меню MarKey в строке меню";
}

function render() {
  const root = document.documentElement;
  root.dataset.platform = state.platform;
  renderPlatformText();
  root.dataset.vibrancy = state.vibrancy ? "on" : "off";
  $("version").textContent = `MarKey ${state.version}`;
  $("no-models").hidden = state.models_loaded.length > 0;
  renderStatus();
  renderStats();
  renderShortcut();
  renderExtraHotkeys();
  renderLayouts();
  renderExcludedApps();
  renderWords();
  renderAbbreviations();
  renderAdvanced();
  renderUpdates();
  updatePreview();
}

function renderStatus() {
  const code = state.status;
  const [title, detail] = STATUS[code] ?? STATUS.starting;
  $("hero").dataset.state = code;
  $("hero-title").textContent = title;
  $("hero-detail").textContent = detail;
  $("enabled").checked = state.config.enabled;
  $("permission").hidden = state.has_permission || state.platform !== "macos";
  const brand = $("brand-status");
  brand.dataset.state = code;
  brand.querySelector("span").textContent = {
    ready: "Работает",
    paused: "На паузе",
    permission: "Нужен доступ",
    starting: "Запуск…",
    secure_input: "Ввод пароля",
    unstable: "Перехват сбоит",
  }[code] ?? "";
}

function renderStats() {
  $("stat-auto").textContent = number.format(state.corrections);
  $("stat-manual").textContent = number.format(state.manual);
  $("stat-undo").textContent = number.format(state.undos);
}

// -- shortcut ---------------------------------------------------------------

function shortcutParts() {
  const s = state.config.shortcut;
  if (!s || s.kind === "off") return { kind: "tap", modifier: "off" };
  return { kind: s.kind, modifier: s.modifier };
}

function renderShortcut() {
  const { kind, modifier } = shortcutParts();
  const caps = $("keycaps");
  caps.textContent = "";
  const keys = availableKeys();
  caps.dataset.count = String(keys.length);
  const taken = takenKeys("shortcut");
  for (const key of keys) {
    const names = keyNames(key.id);
    const button = document.createElement("button");
    button.className = "keycap" + (key.id === "off" ? " off" : "");
    button.setAttribute("role", "radio");
    button.setAttribute("aria-checked", String(key.id === modifier));
    button.title = names.label;
    const symbol = document.createElement("span");
    symbol.className = "keycap-symbol";
    if (key.id === "fn") {
      // The 🌐 key, drawn like the other symbols rather than as an emoji.
      const globe = document.createElement("span");
      globe.dataset.kitIcon = "globe";
      symbol.append(globe);
    } else {
      symbol.textContent = names.symbol;
    }
    const label = document.createElement("span");
    label.className = "keycap-label";
    label.textContent = names.label;
    button.append(symbol, label);
    // Right Command on a Mac; right Ctrl on Windows, where a tap of the
    // Windows key belongs to the Start menu.
    const best = state.platform === "windows" ? "right_control" : "right_command";
    if (key.id === best) {
      const tag = document.createElement("span");
      tag.className = "keycap-tag";
      tag.textContent = "лучший выбор";
      button.append(tag);
    }
    if (taken.has(key.id)) {
      button.disabled = true;
      button.title = `${names.label}: уже занята другой горячей клавишей`;
    }
    button.addEventListener("click", () => {
      const next = key.id === "off" ? { kind: "off" } : { kind: shortcutParts().kind, modifier: key.id };
      save({ shortcut: next });
    });
    caps.append(button);
  }
  $("fn-note").hidden = !usesFn();
  setSegmented("tap-kind", kind);
  $("tap-kind").classList.toggle("disabled", modifier === "off");
  for (const b of $("tap-kind").querySelectorAll("button")) b.disabled = modifier === "off";

  const names = modifier === "off" ? null : keyNames(modifier);
  $("tap-hint").textContent = !names
    ? "Клавиша выключена — MarKey исправляет только сам."
    : kind === "doubletap"
      ? `Два быстрых нажатия ${names.short}. Удобно, если эта клавиша часто нужна в сочетаниях.`
      : `Одно нажатие ${names.short}, без других клавиш.`;
  const text = names ? names.short : "клавишу (сейчас выключена)";
  for (const el of document.querySelectorAll(".shortcut-name")) {
    el.textContent = names ? (kind === "doubletap" ? `${text} дважды` : text) : "клавишу";
  }
  $("convert-selection").checked = state.config.convert_selection;
  $("convert-word").checked = state.config.convert_word_before_caret;
}

function setSegmented(id, value) {
  const root = typeof id === "string" ? $(id) : id;
  for (const b of root.querySelectorAll("button")) {
    b.setAttribute("aria-checked", String(b.dataset.value === value));
  }
}

const HOTKEY_FIELDS = ["shortcut", "force_layout_1", "force_layout_2", "add_exception_key"];

/** Keys used by the hotkeys other than `field`: one key, one hotkey. */
function takenKeys(field) {
  const taken = new Set();
  for (const other of HOTKEY_FIELDS) {
    const s = state.config[other];
    if (other !== field && s && s.kind !== "off") taken.add(s.modifier);
  }
  return taken;
}

function usesFn() {
  return HOTKEY_FIELDS.some((f) => state.config[f]?.modifier === "fn" && state.config[f]?.kind !== "off");
}

function layoutName(index) {
  const id = state.config.layouts[index];
  return id ? NATIVE[id]?.[0] ?? id : null;
}

/** «Всегда раскладка 1 / 2» and «В исключения»: a key menu and a tap kind. */
function renderExtraHotkeys() {
  const group = $("extra-hotkeys");
  group.textContent = "";
  for (const hotkey of EXTRA_HOTKEYS) {
    const current = state.config[hotkey.field] ?? { kind: "off" };
    const row = document.createElement("div");
    row.className = "row row-column hotkey-row";
    const line = document.createElement("div");
    line.className = "hotkey-line";
    const title = document.createElement("span");
    title.className = "row-title";
    if (hotkey.layout !== undefined) {
      const name = layoutName(hotkey.layout);
      title.textContent = `Всегда раскладка ${hotkey.layout + 1}${name ? ` — ${name}` : ""}`;
    } else {
      title.textContent = hotkey.title;
    }
    const detail = document.createElement("span");
    detail.className = "row-detail";
    detail.textContent = hotkey.layout !== undefined && !layoutName(hotkey.layout)
      ? "Нужна вторая раскладка: отметьте её в «Языках»."
      : hotkey.detail;

    const select = document.createElement("select");
    select.className = "select";
    select.setAttribute("aria-label", `${title.textContent}: клавиша`);
    const taken = takenKeys(hotkey.field);
    for (const key of availableKeys()) {
      const option = document.createElement("option");
      option.value = key.id;
      option.textContent = key.id === "off" ? "Нет" : keyNames(key.id).label;
      option.disabled = taken.has(key.id);
      select.append(option);
    }
    select.value = current.kind === "off" ? "off" : current.modifier;
    const unusable = hotkey.layout !== undefined && !layoutName(hotkey.layout);
    select.disabled = unusable;

    const kind = document.createElement("div");
    kind.className = "segmented segmented-small";
    kind.setAttribute("role", "radiogroup");
    kind.setAttribute("aria-label", `${title.textContent}: нажатие`);
    for (const [value, label] of [["tap", "Одно"], ["doubletap", "Двойное"]]) {
      const b = document.createElement("button");
      b.dataset.value = value;
      b.setAttribute("role", "radio");
      b.textContent = label;
      b.disabled = current.kind === "off" || unusable;
      kind.append(b);
    }
    setSegmented(kind, current.kind === "off" ? "tap" : current.kind);

    const store = (modifier, tapKind) =>
      save({ [hotkey.field]: modifier === "off" ? { kind: "off" } : { kind: tapKind, modifier } });
    select.addEventListener("change", () => {
      const k = state.config[hotkey.field];
      store(select.value, k && k.kind !== "off" ? k.kind : "tap");
    });
    kind.addEventListener("click", (e) => {
      const value = e.target.closest("button")?.dataset.value;
      const k = state.config[hotkey.field];
      if (value && k && k.kind !== "off") store(k.modifier, value);
    });
    line.append(title, select, kind);
    row.append(line, detail);
    group.append(row);
  }
}

$("tap-kind").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  const { modifier } = shortcutParts();
  if (!value || modifier === "off") return;
  save({ shortcut: { kind: value, modifier } });
});

$("convert-selection").addEventListener("change", (e) => save({ convert_selection: e.target.checked }));
$("convert-word").addEventListener("change", (e) => save({ convert_word_before_caret: e.target.checked }));

// -- apps -------------------------------------------------------------------

const iconCache = new Map();

function iconFor(app, size = 28) {
  const wrap = document.createElement("span");
  const fallback = () => {
    wrap.className = "app-icon app-icon-fallback";
    wrap.textContent = (app.name || app.bundle_id || "?").trim().charAt(0).toUpperCase();
  };
  fallback();
  if (!app.path) return wrap;
  const cached = iconCache.get(app.path);
  const show = (url) => {
    if (!url) return;
    const img = document.createElement("img");
    img.className = "app-icon";
    img.width = size;
    img.height = size;
    img.alt = "";
    img.src = url;
    wrap.replaceWith(img);
  };
  if (cached !== undefined) {
    queueMicrotask(() => show(cached));
  } else {
    invoke("app_icon", { path: app.path })
      .then((url) => {
        iconCache.set(app.path, url ?? null);
        show(url);
      })
      .catch(() => iconCache.set(app.path, null));
  }
  return wrap;
}

async function loadApps(force = false) {
  if (appsLoaded && !force) return;
  try {
    apps = await invoke("list_apps");
    appsLoaded = true;
  } catch {
    apps = [];
  }
  renderExcludedApps();
  if ($("app-picker").open) renderPicker();
}

const described = new Map();

function appInfo(id) {
  const lower = id.toLowerCase();
  return apps.find((a) => a.bundle_id.toLowerCase() === lower) ?? described.get(lower) ?? null;
}

function renderExcludedApps() {
  const list = $("excluded-apps");
  list.textContent = "";
  const ids = state.config.excluded_apps;
  if (!ids.length) {
    const empty = document.createElement("li");
    empty.className = "app-empty";
    empty.textContent = "Список пуст — MarKey работает во всех приложениях.";
    list.append(empty);
  }
  const rows = ids.map((id) => ({ id, info: appInfo(id) }));
  rows.sort((a, b) => (a.info?.name ?? a.id).localeCompare(b.info?.name ?? b.id, "ru"));
  for (const { id, info } of rows) {
    const li = document.createElement("li");
    li.className = "app-item" + (selectedApp === id ? " selected" : "");
    li.tabIndex = 0;
    const names = document.createElement("span");
    names.className = "app-names";
    const name = document.createElement("span");
    name.className = "app-name";
    name.textContent = info?.name ?? id;
    const sub = document.createElement("span");
    sub.className = "app-id";
    sub.textContent = id;
    names.append(name, sub);
    li.append(iconFor(info ?? { name: id }), names);
    li.addEventListener("click", () => {
      selectedApp = id;
      renderExcludedApps();
    });
    li.addEventListener("keydown", (e) => {
      if (e.key === "Backspace" || e.key === "Delete") removeApp(id);
    });
    list.append(li);
    if (!info && appsLoaded && !described.has(id.toLowerCase())) {
      described.set(id.toLowerCase(), null);
      invoke("describe_app", { bundleId: id })
        .then((d) => {
          if (d) {
            described.set(id.toLowerCase(), d);
            renderExcludedApps();
          }
        })
        .catch(() => {});
    }
  }
  $("remove-app").disabled = !selectedApp || !ids.includes(selectedApp);
  const n = ids.length;
  $("apps-footnote").textContent = n ? `${n} ${plural(n, "приложение", "приложения", "приложений")} в списке.` : "";
}

function removeApp(id) {
  if (selectedApp === id) selectedApp = null;
  save((current) => ({ excluded_apps: current.excluded_apps.filter((v) => v.toLowerCase() !== id.toLowerCase()) }));
}

function toggleApp(id) {
  save((current) => {
    const has = current.excluded_apps.some((v) => v.toLowerCase() === id.toLowerCase());
    return {
      excluded_apps: has ? current.excluded_apps.filter((v) => v.toLowerCase() !== id.toLowerCase()) : [...current.excluded_apps, id],
    };
  });
}

$("remove-app").addEventListener("click", () => selectedApp && removeApp(selectedApp));

function openPicker() {
  $("app-search").value = "";
  $("app-error").hidden = true;
  renderPicker();
  $("app-picker").showModal();
  $("app-search").focus();
  loadApps(true);
}

$("add-app").addEventListener("click", openPicker);
$("add-app-text").addEventListener("click", openPicker);

function renderPicker() {
  const query = $("app-search").value.trim().toLowerCase();
  const list = $("picker-list");
  list.textContent = "";
  const matches = apps.filter((a) => !query || a.name.toLowerCase().includes(query) || a.bundle_id.toLowerCase().includes(query));
  if (!appsLoaded) {
    const li = document.createElement("li");
    li.className = "app-empty";
    li.textContent = "Загружаю список приложений…";
    list.append(li);
    return;
  }
  if (!matches.length) {
    const li = document.createElement("li");
    li.className = "app-empty";
    li.textContent = "Ничего не найдено. Можно указать идентификатор вручную.";
    list.append(li);
    return;
  }
  const excluded = new Set(state.config.excluded_apps.map((v) => v.toLowerCase()));
  for (const app of matches) {
    const li = document.createElement("li");
    li.className = "app-item";
    li.setAttribute("role", "checkbox");
    li.setAttribute("aria-checked", String(excluded.has(app.bundle_id.toLowerCase())));
    li.tabIndex = 0;
    const check = document.createElement("span");
    check.className = "picker-check";
    check.textContent = "✓";
    const names = document.createElement("span");
    names.className = "app-names";
    const name = document.createElement("span");
    name.className = "app-name";
    name.textContent = app.name;
    const sub = document.createElement("span");
    sub.className = "app-id";
    sub.textContent = app.bundle_id;
    names.append(name, sub);
    li.append(iconFor(app), names);
    if (app.running) {
      const dot = document.createElement("span");
      dot.className = "running-dot";
      dot.textContent = "● запущено";
      li.append(dot);
    }
    li.append(check);
    const act = () => {
      li.setAttribute("aria-checked", String(li.getAttribute("aria-checked") !== "true"));
      toggleApp(app.bundle_id);
    };
    li.addEventListener("click", act);
    li.addEventListener("keydown", (e) => {
      if (e.key === " " || e.key === "Enter") {
        e.preventDefault();
        act();
      }
    });
    list.append(li);
  }
}

$("app-search").addEventListener("input", renderPicker);
$("app-search").addEventListener("keydown", (e) => {
  // Enter in the search box must not close the sheet.
  if (e.key === "Enter") e.preventDefault();
});

$("add-by-id").addEventListener("click", async () => {
  const id = $("app-id").value.trim();
  if (!/^[A-Za-z0-9][A-Za-z0-9.\-_]{1,255}$/.test(id)) {
    $("app-error").hidden = false;
    $("app-error").textContent = "Введите идентификатор вида com.example.app.";
    return;
  }
  $("app-error").hidden = true;
  await save((current) => ({
    excluded_apps: current.excluded_apps.some((v) => v.toLowerCase() === id.toLowerCase()) ? current.excluded_apps : [...current.excluded_apps, id],
  }));
  $("app-id").value = "";
});

// -- layouts ----------------------------------------------------------------

function renderLayouts() {
  const list = $("layouts");
  list.textContent = "";
  for (const layout of state.available_layouts) {
    const li = document.createElement("li");
    const label = document.createElement("label");
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = state.config.layouts.includes(layout.id);
    box.addEventListener("change", () => {
      const next = new Set(state.config.layouts);
      box.checked ? next.add(layout.id) : next.delete(layout.id);
      if (next.size === 0) {
        box.checked = true;
        return;
      }
      save({ layouts: state.available_layouts.map((l) => l.id).filter((id) => next.has(id)) });
    });
    const [ru, native] = NATIVE[layout.id] ?? [layout.name, ""];
    const name = document.createElement("span");
    name.className = "name";
    name.textContent = ru;
    label.append(box, name);
    if (layout.limited) {
      const badge = document.createElement("span");
      badge.className = "badge";
      badge.textContent = "ограниченно";
      label.append(badge);
    }
    const nat = document.createElement("span");
    nat.className = "native";
    nat.textContent = native;
    label.append(nat);
    li.append(label);
    list.append(li);
  }

  const select = $("try-layout");
  const previous = select.value;
  select.textContent = "";
  for (const id of state.config.layouts) {
    const option = document.createElement("option");
    option.value = id;
    option.textContent = NATIVE[id]?.[0] ?? id;
    select.append(option);
  }
  select.value = state.config.layouts.includes(previous) ? previous : state.config.layouts[0] ?? "";
}

// -- words ------------------------------------------------------------------

function renderWords() {
  const list = $("exceptions");
  list.textContent = "";
  for (const word of state.config.exceptions) {
    const li = document.createElement("li");
    li.textContent = word;
    const remove = document.createElement("button");
    const x = document.createElement("span");
    x.dataset.kitIcon = "x";
    remove.append(x);
    remove.title = "Убрать";
    remove.setAttribute("aria-label", `Убрать ${word}`);
    remove.addEventListener("click", () => save((c) => ({ exceptions: c.exceptions.filter((w) => w !== word) })));
    li.append(remove);
    list.append(li);
  }
  $("exceptions-empty").hidden = state.config.exceptions.length > 0;
  $("extended").checked = state.config.extended_dictionary;
  const words = state.extended_words ?? 0;
  $("extended").disabled = words === 0 && !state.config.extended_dictionary;
  if (words === 0) {
    $("extended-detail").textContent = "Словарь не найден в этой сборке — переустановите MarKey.";
  }
}

$("extended").addEventListener("change", (e) => save({ extended_dictionary: e.target.checked }));

// -- abbreviations ----------------------------------------------------------

function renderAbbreviations() {
  const list = $("abbr-list");
  list.textContent = "";
  const items = state.config.abbreviations ?? [];
  for (const item of items) {
    const li = document.createElement("li");
    const from = document.createElement("span");
    from.className = "abbr-from";
    from.textContent = item.short;
    const arrow = document.createElement("span");
    arrow.className = "abbr-arrow";
    const icon = document.createElement("span");
    icon.dataset.kitIcon = "arrow";
    arrow.append(icon);
    const to = document.createElement("span");
    to.className = "abbr-to";
    to.textContent = item.full;
    to.title = item.full;
    const remove = document.createElement("button");
    remove.className = "tool-btn";
    remove.setAttribute("aria-label", `Убрать ${item.short}`);
    remove.title = "Убрать";
    const trash = document.createElement("span");
    trash.dataset.kitIcon = "trash";
    remove.append(trash);
    remove.addEventListener("click", () =>
      save((c) => ({ abbreviations: c.abbreviations.filter((a) => a.short !== item.short) })),
    );
    li.append(from, arrow, to, remove);
    list.append(li);
  }
  $("abbr-empty").hidden = items.length > 0;
  $("abbr-enabled").checked = state.config.expand_abbreviations;
  const n = items.length;
  $("abbr-count").textContent = n
    ? `${n} ${plural(n, "сокращение", "сокращения", "сокращений")} в списке.`
    : "Список пуст — добавьте сокращения ниже.";
}

$("abbr-enabled").addEventListener("change", (e) => save({ expand_abbreviations: e.target.checked }));

$("abbr-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const short = $("abbr-short").value.trim();
  const full = $("abbr-full").value.trim();
  const error = (text, field) => {
    $("abbr-error").textContent = text;
    for (const id of ["abbr-short", "abbr-full"]) $(id).removeAttribute("aria-invalid");
    if (field) $(field).setAttribute("aria-invalid", "true");
  };
  if (!short || /\s/.test(short)) return error("Сокращение — одно слово без пробелов.", "abbr-short");
  if (!full) return error("Напишите, на что его заменять.", "abbr-full");
  if ((state.config.abbreviations ?? []).some((a) => a.short.toLowerCase() === short.toLowerCase())) {
    return error(`«${short}» уже есть в списке.`, "abbr-short");
  }
  error("");
  await save((c) => ({ abbreviations: [...(c.abbreviations ?? []), { short, full }] }));
  $("abbr-short").value = "";
  $("abbr-full").value = "";
  $("abbr-short").focus();
});

function addWord() {
  const word = $("word-input").value.trim().toLowerCase();
  if (!word || /\s/.test(word)) return;
  $("word-input").value = "";
  save((c) => ({ exceptions: c.exceptions.includes(word) ? c.exceptions : [...c.exceptions, word] }));
}

$("word-add").addEventListener("click", addWord);
$("word-input").addEventListener("keydown", (e) => {
  if (e.key === "Enter") addWord();
});

// -- advanced ---------------------------------------------------------------

function renderAdvanced() {
  const c = state.config;
  setSegmented("sensitivity", c.sensitivity);
  $("sensitivity-hint").textContent = SENSITIVITY_HINTS[c.sensitivity] ?? "";
  $("switch-layout").checked = c.switch_layout;
  $("skip-caps").checked = c.skip_all_caps;
  $("sound").checked = c.sound;
  $("fix-case").checked = c.fix_case;
  $("two-caps").checked = c.fix_two_caps;
  $("fix-typos").checked = c.fix_typos;
  $("enter-tab").checked = c.correct_on_enter_tab;
  setSegmented("menu-icon", c.menu_bar_icon);
  setSegmented("menu-size", c.menu_bar_size);
  for (const b of $("menu-size").querySelectorAll("button")) b.disabled = c.menu_bar_icon !== "layout";
  drawMenuPreview();
  $("launch").checked = state.launch_at_login;
  $("ai").checked = c.ai_assist;
  $("ai-key-row").hidden = !c.ai_assist;
  // Never echo a stored key back into the DOM; show that one exists instead.
  $("api-key").placeholder = state.has_api_key ? `ключ сохранён ${keyStore()} — введите новый, чтобы заменить` : "sk-ant-…";
  $("ai-status").textContent = !c.ai_assist
    ? ""
    : !state.assist_active
      ? "Без API-ключа не работает."
      : state.assist_learned > 0
        ? `Решено спорных слов: ${state.assist_learned}.`
        : "Ждёт слова, которое локальная модель не смогла решить.";
  $("diag-mode").textContent = !state.hook_running
    ? "не запущен"
    : state.can_hold
      ? "активный фильтр (пробел придерживается до исправления)"
      : "только наблюдение";
  $("diag-models").textContent = state.models_loaded.length ? state.models_loaded.join(", ") : "не загружены";
}

$("sensitivity").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  if (value) save({ sensitivity: value });
});
$("switch-layout").addEventListener("change", (e) => save({ switch_layout: e.target.checked }));
$("skip-caps").addEventListener("change", (e) => save({ skip_all_caps: e.target.checked }));
$("sound").addEventListener("change", (e) => save({ sound: e.target.checked }));
$("fix-case").addEventListener("change", (e) => save({ fix_case: e.target.checked }));
$("two-caps").addEventListener("change", (e) => save({ fix_two_caps: e.target.checked }));
$("fix-typos").addEventListener("change", (e) => save({ fix_typos: e.target.checked }));
$("enter-tab").addEventListener("change", (e) => save({ correct_on_enter_tab: e.target.checked }));
$("menu-icon").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  if (value) save({ menu_bar_icon: value });
});
$("menu-size").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  if (value && state.config.menu_bar_icon === "layout") save({ menu_bar_size: value });
});

// The same 5x7 letters as the real badge (src-tauri/src/tray_badge.rs), so
// the preview is what the menu bar will show.
const BADGE_GLYPHS = {
  A: [14, 17, 17, 31, 17, 17, 17], D: [30, 17, 17, 17, 17, 17, 30], E: [31, 16, 16, 30, 16, 16, 31],
  F: [31, 16, 16, 30, 16, 16, 16], H: [17, 17, 17, 31, 17, 17, 17], L: [16, 16, 16, 16, 16, 16, 31],
  N: [17, 25, 21, 19, 17, 17, 17], R: [30, 17, 17, 30, 20, 18, 17], S: [15, 16, 16, 14, 1, 1, 30],
  T: [31, 4, 4, 4, 4, 4, 4], U: [17, 17, 17, 17, 17, 17, 14],
};
const BADGE_CODES = { us: "EN", ru: "RU", uk: "UA", he: "HE", ar: "AR", el: "EL", de: "DE", fr: "FR", es: "ES", tr: "TR" };
const BADGE_METRICS = { small: [2, 24], medium: [3, 32], large: [4, 40] };

function drawMenuPreview() {
  const canvas = $("menu-preview");
  const ctx = canvas.getContext?.("2d");
  if (!ctx || !state) return;
  const c = state.config;
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = getComputedStyle(canvas.parentElement).color;
  const H = canvas.height;
  if (c.menu_bar_icon !== "layout") {
    // The keycap logo, drawn simply: a rounded key outline.
    ctx.lineWidth = 3;
    ctx.strokeStyle = ctx.fillStyle;
    ctx.beginPath();
    ctx.roundRect(canvas.width / 2 - 15, H / 2 - 15, 30, 30, 7);
    ctx.stroke();
    return;
  }
  const [dot, chipH] = BADGE_METRICS[c.menu_bar_size] ?? BADGE_METRICS.medium;
  const code = [...(BADGE_CODES[c.layouts[0]] ?? "EN")];
  const textW = code.length * 5 * dot + (code.length - 1) * dot;
  const padX = (chipH - 7 * dot) / 2 + dot;
  const chipW = Math.max(textW + 2 * padX, chipH);
  const x0 = Math.round((canvas.width - chipW) / 2);
  const y0 = Math.round((H - chipH) / 2);
  ctx.beginPath();
  ctx.roundRect(x0, y0, chipW, chipH, Math.round(chipH * 0.28));
  ctx.fill();
  ctx.globalCompositeOperation = "destination-out";
  const tx = x0 + (chipW - textW) / 2;
  const ty = y0 + (chipH - 7 * dot) / 2;
  code.forEach((ch, n) => {
    (BADGE_GLYPHS[ch] ?? []).forEach((bits, row) => {
      for (let col = 0; col < 5; col++) {
        if (bits & (16 >> col)) ctx.fillRect(tx + n * 6 * dot + col * dot, ty + row * dot, dot, dot);
      }
    });
  });
  ctx.globalCompositeOperation = "source-over";
}
// Switching the assist on sends words to a third party, so it is asked
// about every time, in plain words; switching it off is immediate.
$("ai").addEventListener("change", (e) => {
  if (!e.target.checked) {
    save({ ai_assist: false });
    return;
  }
  e.target.checked = false;
  const dialog = $("ai-consent");
  dialog.returnValue = "";
  dialog.showModal();
});
$("ai-consent").addEventListener("close", () => {
  if ($("ai-consent").returnValue === "ok") {
    $("ai").checked = true;
    save({ ai_assist: true });
  }
});
$("launch").addEventListener("change", (e) => save({}, { launchAtLogin: e.target.checked }));
$("api-key").addEventListener("change", (e) => {
  const key = e.target.value.trim();
  if (!key) return;
  e.target.value = "";
  save({}, { apiKey: key });
});
$("open-log").addEventListener("click", () => invoke("open_log"));

// -- updates ------------------------------------------------------------------

/** A version already shown this session, so a poll does not reopen it. */
let shownUpdate = null;
let updateLang = "ru";

function renderUpdates() {
  $("check-updates").checked = state.check_updates;
  $("update-version").textContent = `MarKey ${state.version}`;
  $("update-dot").hidden = !state.update;
  if (state.update && state.update.version !== shownUpdate) openUpdate(state.update);
}

$("check-updates").addEventListener("change", async (e) => {
  try {
    await invoke("set_update_checks", { on: e.target.checked });
    state.check_updates = e.target.checked;
  } catch (error) {
    e.target.checked = !e.target.checked;
    $("update-status").textContent = String(error);
  }
});

function busy(button, on, label) {
  if (on) {
    button.dataset.label = button.lastChild.textContent;
    button.lastChild.textContent = label;
    button.setAttribute("aria-busy", "true");
  } else if (button.dataset.label) {
    button.lastChild.textContent = button.dataset.label;
    button.removeAttribute("aria-busy");
  }
}

$("check-now").addEventListener("click", async () => {
  const button = $("check-now");
  button.disabled = true;
  busy(button, true, "Проверяю…");
  try {
    const update = await invoke("check_update", { manual: true });
    if (update) {
      state.update = update;
      $("update-status").textContent = `Доступна версия ${update.version}.`;
      openUpdate(update);
    } else {
      $("update-status").textContent = "У вас последняя версия.";
    }
  } catch (error) {
    $("update-status").textContent = String(error);
  } finally {
    busy(button, false);
    button.disabled = false;
  }
});

/** Release notes as text: "- " lines become a list, the rest paragraphs. */
function renderNotes(text) {
  const box = $("update-notes");
  box.textContent = "";
  if (!text.trim()) {
    const p = document.createElement("p");
    p.className = "update-notes-empty";
    p.textContent = "Описание изменений не приложено к релизу.";
    box.append(p);
    return;
  }
  let list = null;
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line) {
      list = null;
      continue;
    }
    const item = line.match(/^[-*•]\s+(.*)$/);
    if (item) {
      if (!list) {
        list = document.createElement("ul");
        box.append(list);
      }
      const li = document.createElement("li");
      li.textContent = item[1].replace(/\*\*/g, "");
      list.append(li);
    } else {
      list = null;
      const p = document.createElement("p");
      p.textContent = line.replace(/^#+\s*/, "").replace(/\*\*/g, "");
      box.append(p);
    }
  }
  box.scrollTop = 0;
}

function openUpdate(update) {
  shownUpdate = update.version;
  $("update-sub").textContent = `MarKey ${update.version} — у вас ${update.current}`;
  const hasEn = Boolean(update.notes_en);
  $("update-lang").hidden = !hasEn;
  updateLang = "ru";
  setSegmented("update-lang", updateLang);
  renderNotes(update.notes_ru || update.notes_en);
  $("update-error").textContent = "";
  for (const id of ["update-skip", "update-later", "update-download", "update-install"]) $(id).disabled = false;
  const dialog = $("update-dialog");
  if (!dialog.open) dialog.showModal();
}

$("update-lang").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  if (!value || !state.update) return;
  updateLang = value;
  setSegmented("update-lang", value);
  renderNotes(value === "en" ? state.update.notes_en : state.update.notes_ru || state.update.notes_en);
});

function closeUpdate() {
  $("update-dialog").close();
}

$("update-later").addEventListener("click", async () => {
  await invoke("dismiss_update").catch(() => {});
  state.update = null;
  $("update-dot").hidden = true;
  closeUpdate();
});

$("update-skip").addEventListener("click", async () => {
  const version = state.update?.version;
  if (version) await invoke("skip_update", { version }).catch(() => {});
  state.update = null;
  $("update-dot").hidden = true;
  closeUpdate();
});

/** Run a long dialog action with its button busy and the others locked. */
async function updateAction(id, label, command) {
  const ids = ["update-skip", "update-later", "update-download", "update-install"];
  const button = $(id);
  for (const other of ids) $(other).disabled = true;
  busy(button, true, label);
  $("update-error").textContent = "";
  try {
    return await invoke(command);
  } catch (error) {
    $("update-error").textContent = String(error);
    return null;
  } finally {
    busy(button, false);
    for (const other of ids) $(other).disabled = false;
  }
}

$("update-download").addEventListener("click", async () => {
  const path = await updateAction("update-download", "Скачиваю и проверяю…", "download_update");
  if (path) $("update-error").textContent = "";
  if (path) $("update-status").textContent = `Скачано и проверено: ${path}`;
  if (path) closeUpdate();
});

$("update-install").addEventListener("click", () =>
  updateAction("update-install", "Скачиваю и проверяю…", "install_update"),
);

// Esc is «Позже», not a silent close that leaves the badge.
$("update-dialog").addEventListener("cancel", (e) => {
  e.preventDefault();
  $("update-later").click();
});

// -- general ----------------------------------------------------------------

$("enabled").addEventListener("change", (e) => save({ enabled: e.target.checked }));

$("grant").addEventListener("click", async () => {
  await invoke("request_permission");
  setTimeout(refresh, 600);
});

$("open-settings").addEventListener("click", () => invoke("open_accessibility_settings"));

let previewTimer = null;

function updatePreview() {
  clearTimeout(previewTimer);
  previewTimer = setTimeout(async () => {
    const text = $("try-input").value || $("try-input").placeholder;
    const layout = $("try-layout").value;
    if (!layout) return;
    const result = await invoke("preview", { text, layout });
    const output = $("try-output");
    output.textContent = result.output;
    output.classList.toggle("unchanged", !result.changed);
  }, 90);
}

$("try-input").addEventListener("input", updatePreview);
$("try-layout").addEventListener("change", updatePreview);

// -- saving -----------------------------------------------------------------

/**
 * Send the current config back to Rust with `changes` applied. A failure is
 * shown rather than swallowed: otherwise the control moves, nothing is
 * written, and the window looks like it worked.
 */
async function save(changes = {}, extras = {}) {
  if (!state) return;
  // Patches computed from the current settings see the config, not the whole
  // window snapshot.
  const patch = typeof changes === "function" ? (fresh) => changes(fresh.config) : changes;
  try {
    await configWriter.enqueue(patch, extras);
    $("save-error").hidden = true;
  } catch (error) {
    console.error("could not save settings", error);
    const banner = $("save-error");
    banner.hidden = false;
    banner.textContent = `Не удалось сохранить настройку: ${error}`;
    refresh();
  }
}

const configWriter = RekeyConfigWriter({
  readState: () => invoke("get_state"),
  writeState: (config, extras) => invoke("set_config", { config, ...extras }),
  toConfig: (fresh) => fresh.config,
  onState: (fresh) => {
    state = fresh;
    render();
  },
});

// -- startup ----------------------------------------------------------------

// WebKit draws a real system switch for <input type="checkbox" switch>.
if ("switch" in document.createElement("input")) {
  document.documentElement.dataset.nativeSwitch = "on";
}

// The preview is drawn in the menu bar's ink colour, which follows the theme.
window.matchMedia?.("(prefers-color-scheme: dark)").addEventListener?.("change", () => {
  if (state) requestAnimationFrame(drawMenuPreview);
});

// Separate the title strip from the content once something scrolls under it.
document.querySelector(".pages").addEventListener(
  "scroll",
  (e) => document.querySelector(".content").classList.toggle("scrolled", e.target.scrollTop > 2),
  { passive: true },
);

// An overlaid title bar leaves the system nothing to grab, so the drag areas
// ask the window to start dragging itself.
for (const el of document.querySelectorAll("[data-tauri-drag-region]")) {
  el.addEventListener("mousedown", async (event) => {
    if (event.button !== 0 || event.target.closest("button, input, select")) return;
    try {
      await bridge?.window?.getCurrentWindow?.().startDragging();
    } catch {
      // Dev mock, or no window API: nothing to drag.
    }
  });
}

if (!invoke) {
  // Never fall back to plausible-looking placeholder data: an app that reads
  // your keyboard has to be honest about not working.
  document.body.innerHTML =
    '<div class="banner banner-error banner-standalone"><div class="banner-body">' +
    "<strong>MarKey не может связаться со своим ядром.</strong>" +
    "<p>Окно настроек не может ничего показать или изменить. Переустановите MarKey.</p></div></div>";
} else {
  let initial = "general";
  try {
    initial = localStorage.getItem("markey.page") || "general";
  } catch {
    // Not remembered.
  }
  refresh().then(() => showPage(PAGE_TITLES[initial] ? initial : "general"));
  setInterval(() => {
    if (!document.hidden && state) pollLiveFields();
  }, 2000);
}
