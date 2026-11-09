// Verifies that the development mock can only ever activate against a local
// dev server — never inside the packaged app.
//
//   node app/ui/test/dev-mock.test.mjs
//
// This matters more than it looks. If the mock activated in the shipped app it
// would render invented statistics and a settings window whose controls change
// nothing, while looking completely normal.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";
import vm from "node:vm";
import assert from "node:assert/strict";

const here = dirname(fileURLToPath(import.meta.url));
const source = readFileSync(join(here, "../dev-mock.js"), "utf8");

/** Run dev-mock.js against a fake window and hand that window back. */
function load({ protocol, hostname, tauriPresent, search = "" }) {
  const window = tauriPresent ? { __TAURI__: { core: { invoke: () => {} } } } : {};
  const context = {
    window,
    location: { protocol, hostname, search },
    console: { info() {}, warn() {} },
    navigator: { userAgent: "test" },
    structuredClone: (v) => JSON.parse(JSON.stringify(v)),
    URLSearchParams,
    encodeURIComponent,
  };
  context.globalThis = context;
  vm.createContext(context);
  vm.runInContext(source, context);
  return window;
}

/** The mock announces itself by installing the bridge where there was none. */
function activates(options) {
  return !options.tauriPresent && Boolean(load(options).__TAURI__);
}

const cases = [
  // The only case that should activate.
  { name: "dev server on localhost", protocol: "http:", hostname: "localhost",
    tauriPresent: false, expect: true },
  { name: "dev server on 127.0.0.1", protocol: "http:", hostname: "127.0.0.1",
    tauriPresent: false, expect: true },

  // Packaged app, macOS: tauri://localhost
  { name: "packaged app (macOS)", protocol: "tauri:", hostname: "localhost",
    tauriPresent: false, expect: false },
  // Packaged app, Windows: http://tauri.localhost
  { name: "packaged app (Windows)", protocol: "http:", hostname: "tauri.localhost",
    tauriPresent: false, expect: false },
  // A real remote host must never get invented data either.
  { name: "remote host", protocol: "https:", hostname: "example.com",
    tauriPresent: false, expect: false },
  // With the real bridge present the mock must stand aside everywhere.
  { name: "dev server with real bridge", protocol: "http:", hostname: "localhost",
    tauriPresent: true, expect: false },
  // Asking for a mocked account in the URL changes nothing about any of that.
  { name: "packaged app, account in URL", protocol: "tauri:", hostname: "localhost",
    tauriPresent: false, search: "?account=signedIn", expect: false },
  { name: "remote host, account in URL", protocol: "https:", hostname: "example.com",
    tauriPresent: false, search: "?account=signedIn&long=1", expect: false },
];

let failures = 0;
for (const c of cases) {
  const got = activates(c);
  const ok = got === c.expect;
  if (!ok) failures++;
  console.log(`  ${ok ? "ok  " : "FAIL"} ${c.name.padEnd(30)} activates=${got} expected=${c.expect}`);
}

console.log(`\n${cases.length} cases, ${failures} failure(s)`);
assert.equal(failures, 0, "dev mock activated where it must not");

// The account a dev server gets is the one the URL asks for, shaped like
// account.rs answers.
async function accountFor(search) {
  const window = load({ protocol: "http:", hostname: "localhost", tauriPresent: false, search });
  return window.__TAURI__.core.invoke("get_account");
}

const FIELDS = ["accountName", "status", "via", "error", "name", "initials", "avatar", "syncWords", "sync"];

const plain = await accountFor("");
assert.deepEqual(Object.keys(plain).sort(), [...FIELDS].sort(), "account fields");
assert.equal(plain.status, "signedOut");
assert.deepEqual(plain.sync, { state: "off", sets: [] }, "signed out: nothing is synced");

const pending = await accountFor("?account=pending&via=browser");
assert.equal(pending.status, "pending");
assert.equal(pending.via, "browser");

const failed = await accountFor("?account=error&error=expired");
assert.equal(failed.status, "signedOut");
assert.equal(failed.error, "expired");

const signedIn = await accountFor("?account=signedIn&sync=refused&words=1");
assert.equal(signedIn.status, "signedIn");
assert.equal(signedIn.initials, "ИП");
assert.equal(signedIn.syncWords, true);
assert.equal(signedIn.sync.state, "refused");
assert.deepEqual(signedIn.sync.sets.map((set) => set.key), ["rules", "word-exceptions", "app-exceptions"]);

const windowWithEvents = load({ protocol: "http:", hostname: "localhost", tauriPresent: false });
let heard = 0;
await windowWithEvents.__TAURI__.event.listen("config-changed", () => heard++);
windowWithEvents.__markeyMock.emit("config-changed");
assert.equal(heard, 1, "the mock delivers Tauri events");

console.log("account mock: states follow the URL");
