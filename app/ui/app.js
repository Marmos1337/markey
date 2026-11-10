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

const nav = $("nav");
const content = document.querySelector(".content");
const pageEls = [...document.querySelectorAll(".page")];
const navItems = [...document.querySelectorAll(".nav-item")];

// One title per page in the same spot, so the header cross-fades as the
// content does.
const titleEls = Object.entries(PAGE_TITLES).map(([name, text]) => {
  const layer = document.createElement("span");
  layer.className = "mk-layer";
  layer.dataset.page = name;
  layer.textContent = text;
  $("page-title").append(layer);
  return layer;
});

/** Pages that have been on screen; only their first showing cascades. */
const seenPages = new Set();
/** A first page reached while the window is still hidden in the tray. */
let cascadeOnReveal = null;

/** mar-motion.css caps a row's delay at 240 ms, which is step 6, and is done by 600. */
const CASCADE_LAST_STEP = 6;
const CASCADE_MS = 640;
// Not the excluded apps: that list is rebuilt the moment the app names arrive,
// so its rows would pop in ahead of their turn. It comes in with its card.
const CASCADE_ROWS = ":scope > .row, :scope > form, :scope > ul:not(#excluded-apps) > li, :scope > .list-toolbar";

/** Everything on a page that arrives on its own, in reading order. */
function cascadeTargets(pageEl) {
  const targets = [];
  for (const child of pageEl.children) {
    if (child.hidden) continue;
    const rows = child.matches(".group") ? [...child.querySelectorAll(CASCADE_ROWS)].filter((row) => !row.hidden) : [];
    if (rows.length) {
      // A card fades in with its first row and does not travel.
      targets.push({ el: child, name: "mk-cascade-surface" }, ...rows.map((el) => ({ el, name: "mar-cascade" })));
    } else {
      targets.push({ el: child, name: "mar-cascade" });
    }
  }
  return targets;
}

function playCascade(pageEl) {
  const targets = cascadeTargets(pageEl);
  let step = 0;
  for (const { el, name } of targets) {
    el.dataset.i = String(Math.min(step, CASCADE_LAST_STEP));
    el.classList.add(name);
    if (name === "mar-cascade") step += 1;
  }
  setTimeout(() => {
    for (const { el, name } of targets) {
      el.classList.remove(name);
      delete el.dataset.i;
    }
  }, CASCADE_MS);
}

function currentPageEl() {
  return pageEls.find((el) => el.dataset.page === page);
}

/** The line under the title appears once the current page is scrolled. */
function markScrolled() {
  content.classList.toggle("scrolled", (currentPageEl()?.scrollTop ?? 0) > 2);
}

/**
 * Only classes change here, the transitions do the rest. A page stays laid out
 * when it is not on screen, so it keeps its scroll position by itself, and
 * clicking faster than the fade just redirects it.
 */
function showPage(name) {
  page = name;
  const pageEl = currentPageEl();
  for (const el of pageEls) {
    const on = el === pageEl;
    el.classList.toggle("is-current", on);
    el.inert = !on;
  }
  for (const el of titleEls) el.classList.toggle("is-current", el.dataset.page === name);
  navItems.forEach((el, index) => {
    const on = el.dataset.page === name;
    el.classList.toggle("active", on);
    el.setAttribute("aria-current", on ? "page" : "false");
    if (on) nav.dataset.active = String(index);
  });
  // The first placement is not a journey: the highlight only travels from now on.
  if (!nav.classList.contains("is-live")) {
    requestAnimationFrame(() => requestAnimationFrame(() => nav.classList.add("is-live")));
  }
  markScrolled();
  if (!seenPages.has(name)) {
    seenPages.add(name);
    if (document.hidden) cascadeOnReveal = pageEl;
    else playCascade(pageEl);
  }
  try {
    localStorage.setItem("markey.page", name);
  } catch {
    // Storage can be unavailable; the page simply is not remembered.
  }
  if (name === "apps") loadApps();
}

for (const el of navItems) {
  el.addEventListener("click", () => showPage(el.dataset.page));
}

// The settings window is created hidden at launch; its first page should
// arrive when someone can see it.
document.addEventListener("visibilitychange", () => {
  if (document.hidden || !cascadeOnReveal) return;
  if (cascadeOnReveal === currentPageEl()) playCascade(cascadeOnReveal);
  cascadeOnReveal = null;
});

// -- motion helpers ---------------------------------------------------------

/** --mar-t-1 in mar-motion.css. */
const TEXT_FADE_MS = 160;
/** A little over the longest way out, in case no transition ever runs. */
const EXIT_SAFETY_MS = 480;
/** How long a message stays up. */
const TOAST_MS = 8000;

/** Change a line of text with a short fade; its space is already reserved. */
function swapText(el, text) {
  if (el.textContent === text) return;
  el.textContent = text;
  el.animate?.([{ opacity: 0 }, { opacity: 1 }], { duration: TEXT_FADE_MS, easing: "ease-out" });
}

const exits = new WeakMap();

function cancelExit(el) {
  const exit = exits.get(el);
  if (!exit) return;
  clearTimeout(exit.timer);
  el.removeEventListener("transitionend", exit.onEnd);
  exits.delete(el);
}

/** Bring up a .mar-sheet. It has to be drawn closed first, to have somewhere to rise from. */
function riseIn(el) {
  cancelExit(el);
  void el.offsetWidth;
  el.dataset.open = "";
}

/** Send a .mar-sheet back the way it came, then `done`. Asking twice changes nothing. */
function riseOut(el, done) {
  if (exits.has(el)) return;
  const finish = () => {
    cancelExit(el);
    done();
  };
  const onEnd = (event) => {
    if (event.target === el && event.propertyName === "opacity") finish();
  };
  el.addEventListener("transitionend", onEnd);
  exits.set(el, { onEnd, timer: setTimeout(finish, EXIT_SAFETY_MS) });
  delete el.dataset.open;
}

const sheets = [...document.querySelectorAll("dialog.sheet")];

/** The dimming follows whether any sheet is on its way in or staying. */
function syncScrim() {
  const scrim = $("sheet-scrim");
  if (sheets.some((dialog) => dialog.open && dialog.dataset.open !== undefined)) scrim.dataset.open = "";
  else delete scrim.dataset.open;
}

function openSheet(dialog) {
  closeAccountMenu();
  if (!dialog.open) dialog.showModal();
  riseIn(dialog);
  syncScrim();
}

/** Close a dialog the way it came. The real close() waits for the exit. */
function closeSheet(dialog, value) {
  if (!dialog.open) return;
  riseOut(dialog, () => {
    if (dialog.open) dialog.close(value);
  });
  syncScrim();
}

for (const dialog of sheets) {
  // <form method="dialog"> and Esc would close at once.
  dialog.querySelector('form[method="dialog"]')?.addEventListener("submit", (event) => {
    event.preventDefault();
    closeSheet(dialog, event.submitter?.value);
  });
  // The update dialog has its own Esc: see below.
  if (dialog.id !== "update-dialog") {
    dialog.addEventListener("cancel", (event) => {
      event.preventDefault();
      closeSheet(dialog);
    });
  }
  // Chromium lets a second Esc through whatever `cancel` said.
  dialog.addEventListener("close", () => {
    cancelExit(dialog);
    delete dialog.dataset.open;
    syncScrim();
  });
}

const toastTimers = new WeakMap();

function showToast(el, text) {
  el.querySelector(".toast-text").textContent = text;
  el.hidden = false;
  riseIn(el);
  clearTimeout(toastTimers.get(el));
  toastTimers.set(el, setTimeout(() => hideToast(el), TOAST_MS));
}

function hideToast(el) {
  clearTimeout(toastTimers.get(el));
  if (!el.hidden) riseOut(el, () => (el.hidden = true));
}

for (const toast of document.querySelectorAll(".toast")) {
  toast.querySelector(".toast-close").addEventListener("click", () => hideToast(toast));
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
  showNotice(fresh.notice);
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

const REBUILT_LISTS = "#extra-hotkeys, #layouts, #excluded-apps, #exceptions, #abbr-list, #picker-list";
const FOCUSABLE = "button, input, select, [tabindex]";

/** Rebuilding a list drops keyboard focus; put it back on the same place. */
function keepingFocus(rebuild) {
  const active = document.activeElement;
  const list = active?.closest?.(REBUILT_LISTS);
  const index = list ? [...list.querySelectorAll(FOCUSABLE)].indexOf(active) : -1;
  rebuild();
  if (index < 0 || document.contains(active)) return;
  const items = list.querySelectorAll(FOCUSABLE);
  items[Math.min(index, items.length - 1)]?.focus({ preventScroll: true });
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
  keepingFocus(() => {
    renderExtraHotkeys();
    renderLayouts();
    renderExcludedApps();
    renderWords();
    renderAbbreviations();
  });
  renderAdvanced();
  renderUpdates();
  updatePreview();
}

function renderStatus() {
  const code = state.status;
  const [title, detail] = STATUS[code] ?? STATUS.starting;
  $("hero").dataset.state = code;
  swapText($("hero-title"), title);
  swapText($("hero-detail"), detail);
  $("enabled").checked = state.config.enabled;
  $("permission").hidden = state.has_permission || state.platform !== "macos";
  const brand = $("brand-status");
  brand.dataset.state = code;
  swapText(brand.querySelector("span"), {
    ready: "Работает",
    paused: "На паузе",
    permission: "Нужен доступ",
    starting: "Запуск…",
    secure_input: "Ввод пароля",
    unstable: "Перехват сбоит",
  }[code] ?? "");
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

/** The key a tap of which never gets in the way on this system. */
function bestKey() {
  // Right Command on a Mac; right Ctrl on Windows, where a tap of the
  // Windows key belongs to the Start menu.
  return state.platform === "windows" ? "right_control" : "right_command";
}

/** Built once: later the same buttons change state, so the change can fade. */
function buildKeycaps(caps, keys) {
  caps.textContent = "";
  caps.dataset.count = String(keys.length);
  for (const key of keys) {
    const names = keyNames(key.id);
    const button = document.createElement("button");
    button.className = "keycap mar-hover mar-press";
    button.dataset.key = key.id;
    button.setAttribute("role", "radio");
    const symbol = document.createElement("span");
    symbol.className = "keycap-symbol";
    if (key.id === "fn") {
      // The 🌐 key, drawn like the other symbols rather than as an emoji.
      const globe = document.createElement("span");
      globe.dataset.kitIcon = "globe";
      symbol.append(globe);
    } else if (key.id === "off") {
      // «No key»: a struck-through circle, the same size as the key symbols.
      symbol.innerHTML =
        '<svg class="ds-icon" viewBox="0 0 24 24" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="m5.6 5.6 12.8 12.8"/></svg>';
    } else {
      symbol.textContent = names.symbol;
      // A word («Ctrl», «Alt» on Windows) is set smaller than a one-glyph
      // symbol, so it stays clear of the «лучший» badge in the corner.
      if ([...names.symbol].length > 1) symbol.classList.add("is-word");
    }
    const label = document.createElement("span");
    label.className = "keycap-label";
    label.textContent = names.label;
    button.append(symbol, label);
    if (key.id === bestKey()) {
      const tag = document.createElement("span");
      tag.className = "keycap-tag";
      tag.textContent = "лучший";
      button.append(tag);
    }
    button.addEventListener("click", () => {
      const next = key.id === "off" ? { kind: "off" } : { kind: shortcutParts().kind, modifier: key.id };
      save({ shortcut: next });
    });
    caps.append(button);
  }
}

function renderShortcut() {
  const { kind, modifier } = shortcutParts();
  const caps = $("keycaps");
  const keys = availableKeys();
  if (caps.children.length !== keys.length) buildKeycaps(caps, keys);
  const taken = takenKeys("shortcut");
  for (const button of caps.children) {
    const id = button.dataset.key;
    const { label } = keyNames(id);
    button.setAttribute("aria-checked", String(id === modifier));
    button.disabled = taken.has(id);
    button.title = taken.has(id)
      ? `${label}: уже занята другой горячей клавишей`
      : id === bestKey()
        ? `${label} — лучший выбор`
        : label;
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
/** The user just picked a key here: that rebuild is wanted, the dropdown has closed. */
let hotkeyPicked = false;
/** A rebuild held back for a focused dropdown, still to be done. */
let hotkeysOwed = false;

function renderExtraHotkeys() {
  const group = $("extra-hotkeys");
  // Nothing says whether a <select> has its list open, and a rebuild would shut it.
  if (group.contains(document.activeElement) && document.activeElement.matches("select") && !hotkeyPicked) {
    hotkeysOwed = true;
    return;
  }
  hotkeyPicked = false;
  hotkeysOwed = false;
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
      b.className = "mar-hover mar-press";
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
      hotkeyPicked = true;
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

$("extra-hotkeys").addEventListener("focusout", () => {
  // Once the browser has settled where focus went.
  setTimeout(() => {
    if (hotkeysOwed) keepingFocus(renderExtraHotkeys);
  });
});

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
  openSheet($("app-picker"));
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

// Enter here means «Добавить», not the sheet's «Готово».
$("app-id").addEventListener("keydown", (e) => {
  if (e.key !== "Enter") return;
  e.preventDefault();
  $("add-by-id").click();
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
    remove.className = "mar-hover mar-press";
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
    remove.className = "tool-btn mar-hover mar-press";
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
  openSheet(dialog);
});
$("ai-consent").addEventListener("close", () => {
  if ($("ai-consent").returnValue === "ok") {
    $("ai").checked = true;
    // The consent itself is stored on the Rust side; without it the assist
    // is refused there.
    save({ ai_assist: true }, { aiConsent: true });
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

// -- notices ------------------------------------------------------------------

/** A one-off message from the keyboard side, e.g. a selection too big. */
function showNotice(text) {
  if (!text) return;
  showToast($("notice"), text);
}

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

/** Cross-fade a .btn-swap between its idle and its working face. */
function busy(button, on) {
  const [idle, working] = button.children;
  idle.toggleAttribute("data-on", !on);
  working.toggleAttribute("data-on", on);
  idle.setAttribute("aria-hidden", String(on));
  working.setAttribute("aria-hidden", String(!on));
  if (on) button.setAttribute("aria-busy", "true");
  else button.removeAttribute("aria-busy");
}

$("check-now").addEventListener("click", async () => {
  const button = $("check-now");
  button.disabled = true;
  busy(button, true);
  try {
    const update = await invoke("check_update", { manual: true });
    if (update) {
      state.update = update;
      swapText($("update-status"), `Доступна версия ${update.version}.`);
      openUpdate(update);
    } else {
      swapText($("update-status"), "У вас последняя версия.");
    }
  } catch (error) {
    swapText($("update-status"), String(error));
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
  openSheet($("update-dialog"));
}

$("update-lang").addEventListener("click", (e) => {
  const value = e.target.closest("button")?.dataset.value;
  if (!value || !state.update) return;
  updateLang = value;
  setSegmented("update-lang", value);
  renderNotes(value === "en" ? state.update.notes_en : state.update.notes_ru || state.update.notes_en);
});

function closeUpdate() {
  closeSheet($("update-dialog"));
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
async function updateAction(id, command) {
  const ids = ["update-skip", "update-later", "update-download", "update-install"];
  const button = $(id);
  for (const other of ids) $(other).disabled = true;
  busy(button, true);
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
  const path = await updateAction("update-download", "download_update");
  if (path) $("update-error").textContent = "";
  if (path) swapText($("update-status"), `Скачано и проверено: ${path}`);
  if (path) closeUpdate();
});

$("update-install").addEventListener("click", () => updateAction("update-install", "install_update"));

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

// -- account ----------------------------------------------------------------

const ACCOUNT_ERRORS = {
  denied: "Вход отклонён",
  network: "Нет связи с сервером",
  rejected: "Сервер отклонил вход",
  server: "Сервер недоступен",
  expired: "Время входа истекло",
  open: "Не удалось открыть вход",
  session: "Сессия завершилась, войдите снова",
  storage: "Вход не сохранится на этом компьютере",
};

const SYNC_LINES = {
  idle: "Всё сохранено",
  syncing: "Сохраняем…",
  error: "Не сохранено, повторим позже",
  refused: "Список не помещается в облако",
};

const SET_UNITS = {
  rules: ["правило", "правила", "правил"],
  "word-exceptions": ["слово", "слова", "слов"],
  "app-exceptions": ["приложение", "приложения", "приложений"],
};

const WORDS_SET = "word-exceptions";

/** What to say when something the user pressed did not go through. */
const ACCOUNT_ACTIONS = {
  account_sign_in: "Не удалось начать вход",
  account_cancel: "Не удалось отменить вход",
  account_sign_out: "Не удалось выйти",
  set_cloud_words: "Не удалось сохранить настройку",
};

/** The one row that holds a control; every other row is built from the data. */
const wordsRow = $("account-sets").querySelector(`[data-key="${WORDS_SET}"]`);

/** Last snapshot from Rust; the corner and the menu only ever draw this. */
let account = null;
/** Answers can overtake each other; only the newest question counts. */
let accountAsked = 0;
let accountMenuOpen = false;

async function askAccount(command, args) {
  const asked = ++accountAsked;
  try {
    const view = await invoke(command, args);
    if (asked !== accountAsked) return;
    account = view;
    renderAccount();
  } catch (error) {
    console.error(`account: ${command} failed`, error);
    if (ACCOUNT_ACTIONS[command]) {
      const reason = error === "signed out" ? "сначала войдите" : error;
      showToast($("save-error"), `${ACCOUNT_ACTIONS[command]}: ${reason}`);
    }
    if (command !== "get_account") askAccount("get_account");
  }
}

/** The control of the state on show, for wherever focus has to land. */
function accountControl() {
  return $("account").querySelector(".mk-layer.is-current button");
}

function renderAccount() {
  const root = $("account");
  // Going inert would drop focus, so note it first and carry it across.
  const focused = root.contains(document.activeElement) ? document.activeElement : null;
  for (const layer of root.children) {
    const on = layer.dataset.accountView === account.status;
    layer.classList.toggle("is-current", on);
    layer.inert = !on;
  }
  if (focused && focused.closest(".mk-layer") !== root.querySelector(".mk-layer.is-current")) {
    accountControl()?.focus({ preventScroll: true });
  }

  // The layer's own fade covers this change; animating the label as well left
  // it hidden in Chromium 156.
  $("account-pending").textContent = account.via === "browser" ? "Ждём браузер…" : "Ждём MarHub…";

  const message = ACCOUNT_ERRORS[account.error] ?? "";
  const error = $("account-error");
  // The old text stays while it fades away.
  if (message) swapText(error, message);
  error.classList.toggle("is-on", Boolean(message));

  if (account.status !== "signedIn") {
    closeAccountMenu();
    return;
  }
  const name = account.name || account.accountName;
  paintAvatar($("account-avatar"));
  $("account-name").textContent = name;
  $("account-open").title = name;
  $("account-open").setAttribute("aria-label", `${name}: открыть меню аккаунта`);
  renderAccountMenu(name);
}

/** The picture, the initials on a coloured circle, or a person. */
function paintAvatar(slot) {
  const key = `${account.avatar}|${account.initials}`;
  if (slot.dataset.painted === key) return;
  slot.dataset.painted = key;
  slot.textContent = "";
  const fallback = () => {
    slot.textContent = "";
    if (account.initials) {
      slot.textContent = account.initials;
    } else {
      const icon = document.createElement("span");
      icon.dataset.kitIcon = "user";
      slot.append(icon);
    }
  };
  if (account.avatar?.startsWith("data:image/")) {
    const img = document.createElement("img");
    img.alt = "";
    img.addEventListener("error", fallback);
    img.src = account.avatar;
    slot.append(img);
  } else {
    fallback();
  }
}

/** When a set was last saved, the way a person would say it. */
function whenSaved(iso) {
  const then = new Date(iso);
  if (Number.isNaN(then.getTime())) return "";
  const now = new Date();
  const minutes = Math.floor((now - then) / 60000);
  if (minutes < 1) return "только что";
  if (minutes < 60) return `${minutes} ${plural(minutes, "минуту", "минуты", "минут")} назад`;
  const clock = then.toLocaleTimeString("ru-RU", { hour: "2-digit", minute: "2-digit" });
  const midnight = (date) => new Date(date.getFullYear(), date.getMonth(), date.getDate()).getTime();
  const days = Math.round((midnight(now) - midnight(then)) / 86400000);
  if (days === 0) return `сегодня в ${clock}`;
  if (days === 1) return `вчера в ${clock}`;
  if (days < 7) return `${days} ${plural(days, "день", "дня", "дней")} назад`;
  return then.toLocaleDateString("ru-RU", {
    day: "numeric",
    month: "long",
    ...(then.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
  });
}

function setDetail(set) {
  const units = SET_UNITS[set.key];
  const count = set.count === 0 ? "пусто" : units ? `${number.format(set.count)} ${plural(set.count, ...units)}` : number.format(set.count);
  if (set.updatedAt) {
    const saved = whenSaved(set.updatedAt);
    return saved ? `${count} · ${saved}` : count;
  }
  // «Not saved yet» is only true while a save is running or has just failed.
  const unsaved = !set.localOnly && ["syncing", "error"].includes(account.sync.state);
  return unsaved ? `${count} · ещё не сохранено` : count;
}

/**
 * The sets Rust reports. The words are missing while they stay on this
 * computer; their row is still shown, with the switch that sends them up.
 */
function cloudSets() {
  const sets = [...account.sync.sets];
  if (sets.length && !sets.some((set) => set.key === WORDS_SET)) {
    sets.splice(1, 0, {
      key: WORDS_SET,
      title: "Слова-исключения",
      count: state?.config.exceptions.length ?? 0,
      updatedAt: null,
      localOnly: true,
    });
  }
  return sets;
}

function setRow(set) {
  const li = document.createElement("li");
  li.className = "row account-set";
  li.dataset.key = set.key;
  const text = document.createElement("div");
  text.className = "row-text";
  const title = document.createElement("span");
  title.className = "row-title account-set-title";
  const detail = document.createElement("span");
  detail.className = "row-detail account-set-detail";
  text.append(title, detail);
  li.append(text);
  return li;
}

function skeletonRows() {
  return [0, 1, 2].map(() => {
    const li = document.createElement("li");
    li.className = "row row-column account-set account-set-skeleton";
    for (const size of ["ds-skeleton--line", "ds-skeleton--line ds-skeleton--line-sm"]) {
      const bar = document.createElement("span");
      bar.className = `ds-skeleton ${size}`;
      li.append(bar);
    }
    return li;
  });
}

function renderCloudSets() {
  const list = $("account-sets");
  const sets = cloudSets();
  if (!sets.length) {
    // Rust always names its sets; this is only for an answer without any.
    list.dataset.empty = account.sync.state;
    list.textContent = "";
    if (account.sync.state === "syncing") {
      list.append(...skeletonRows());
    } else {
      const li = document.createElement("li");
      li.className = "account-sets-empty";
      li.textContent = "Пока ничего не сохранено";
      list.append(li);
    }
    return;
  }
  if (list.dataset.empty) {
    delete list.dataset.empty;
    list.textContent = "";
  }
  const rows = new Map([...list.children].map((li) => [li.dataset.key, li]));
  sets.forEach((set, index) => {
    const li = rows.get(set.key) ?? (set.key === WORDS_SET ? wordsRow : setRow(set));
    rows.delete(set.key);
    if (list.children[index] !== li) list.insertBefore(li, list.children[index] ?? null);
    swapText(li.querySelector(".account-set-title"), set.title);
    swapText(li.querySelector(".account-set-detail"), setDetail(set));
    li.querySelector(".account-set-title").title = set.title;
  });
  for (const stale of rows.values()) stale.remove();
}

function renderAccountMenu(name) {
  paintAvatar($("account-menu-avatar"));
  const who = $("account-menu-name");
  who.textContent = name;
  who.title = name;
  $("account-menu-service").textContent = account.accountName;
  renderCloudSets();
  const line = $("account-sync");
  line.dataset.state = account.sync.state;
  swapText(line, SYNC_LINES[account.sync.state] ?? "");
  line.title = line.textContent;
  $("account-words").checked = account.syncWords;
}

function openAccountMenu() {
  if (accountMenuOpen || account?.status !== "signedIn") return;
  accountMenuOpen = true;
  renderAccount();
  const menu = $("account-menu");
  menu.inert = false;
  menu.classList.add("is-open");
  $("account-open").setAttribute("aria-expanded", "true");
  menu.focus({ preventScroll: true });
}

function closeAccountMenu() {
  if (!accountMenuOpen) return;
  accountMenuOpen = false;
  const menu = $("account-menu");
  const hadFocus = menu.contains(document.activeElement);
  menu.classList.remove("is-open");
  menu.inert = true;
  $("account-open").setAttribute("aria-expanded", "false");
  if (hadFocus) accountControl()?.focus({ preventScroll: true });
}

// Rust opens MarHub or the browser before it answers, and that is not instant:
// a second click in the meantime must not start a second attempt.
let signInAsked = false;
$("account-sign-in").addEventListener("click", async () => {
  if (signInAsked) return;
  signInAsked = true;
  await askAccount("account_sign_in");
  signInAsked = false;
});
$("account-cancel").addEventListener("click", () => askAccount("account_cancel"));
$("account-sign-out").addEventListener("click", () => askAccount("account_sign_out"));
$("account-open").addEventListener("click", () => (accountMenuOpen ? closeAccountMenu() : openAccountMenu()));
$("account-words").addEventListener("change", (e) => askAccount("set_cloud_words", { on: e.target.checked }));

// Clicks outside close it; a press on its own trigger is handled by the click.
document.addEventListener(
  "pointerdown",
  (event) => {
    if (accountMenuOpen && !$("account-menu").contains(event.target) && !$("account-open").contains(event.target)) {
      closeAccountMenu();
    }
  },
  true,
);

document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && accountMenuOpen) closeAccountMenu();
});

window.addEventListener("blur", closeAccountMenu);

// Tab goes round inside the menu instead of off into the hidden page behind.
$("account-menu").addEventListener("keydown", (event) => {
  if (event.key !== "Tab") return;
  const menu = event.currentTarget;
  const items = [...menu.querySelectorAll("button, input")];
  const first = items[0];
  const last = items[items.length - 1];
  const active = document.activeElement;
  if (event.shiftKey && (active === first || active === menu)) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && active === last) {
    event.preventDefault();
    first.focus();
  }
});

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
    hideToast($("save-error"));
  } catch (error) {
    console.error("could not save settings", error);
    showToast($("save-error"), `Не удалось сохранить настройку: ${error}`);
    refresh();
  }
}

const configWriter = RekeyConfigWriter({
  readState: () => invoke("get_state"),
  writeState: (config, extras, fresh) =>
    invoke("set_config", { config, ...extras, baseRevision: fresh.config_revision }),
  toConfig: (fresh) => fresh.config,
  onState: (fresh) => {
    state = fresh;
    render();
    redrawPicker();
    if (account?.status === "signedIn") renderCloudSets();
  },
});

/** The open picker follows the stored list, in place. */
function redrawPicker() {
  if (!$("app-picker").open) return;
  const list = $("picker-list");
  const top = list.scrollTop;
  keepingFocus(renderPicker);
  list.scrollTop = top;
}

/**
 * The configuration changed outside this window (a cloud merge). Every save
 * sends the whole config, so a stale window would undo the merge. It goes
 * through the write queue: an edit still on its way lands first and the window
 * ends up showing what is really stored.
 */
async function reloadConfig() {
  try {
    await configWriter.reload();
  } catch (error) {
    console.error("could not reload settings", error);
  }
}

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
  (e) => {
    if (e.target === currentPageEl()) markScrolled();
  },
  { capture: true, passive: true },
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
  // A failed first read still has to put a page on screen.
  refresh()
    .catch((error) => console.error("could not read the settings", error))
    .finally(() => showPage(PAGE_TITLES[initial] ? initial : "general"));
  setInterval(() => {
    if (!document.hidden && state) pollLiveFields();
  }, 2000);
  askAccount("get_account");
  for (const [event, handler] of [
    ["account-changed", () => askAccount("get_account")],
    ["config-changed", reloadConfig],
  ]) {
    bridge.event?.listen?.(event, handler).catch((error) => console.error(`cannot listen for ${event}`, error));
  }
}
