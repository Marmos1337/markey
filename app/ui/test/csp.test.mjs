// K-10: the settings window runs under a CSP without 'unsafe-inline'. Any
// style="" attribute or inline <script> would be blocked silently and the
// window would render wrong only in the packaged app. This keeps it so.
//
//   node --test "app/ui/test/*.test.mjs"

import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const ui = join(dirname(fileURLToPath(import.meta.url)), "..");
const read = (f) => readFileSync(join(ui, f), "utf8");

test("the CSP forbids inline styles and scripts", () => {
  const conf = JSON.parse(read("../src-tauri/tauri.conf.json"));
  const csp = conf.app.security.csp;
  assert.ok(!csp.includes("unsafe-inline"), csp);
  assert.match(csp, /default-src 'self'/);
});

test("the page has no inline styles or inline scripts", () => {
  const html = read("index.html");
  assert.doesNotMatch(html, /\sstyle\s*=/i, "style attribute in index.html");
  for (const tag of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
    assert.match(tag[1], /\bsrc=/, "inline <script> in index.html");
    assert.equal(tag[2].trim(), "");
  }
  assert.doesNotMatch(html, /\son[a-z]+\s*=/i, "inline event handler in index.html");
});

test("scripts never write style attributes into markup", () => {
  for (const file of readdirSync(ui).filter((f) => f.endsWith(".js"))) {
    const js = read(file);
    assert.doesNotMatch(js, /style\s*=\s*\\?["']/, `${file} writes a style attribute`);
    assert.doesNotMatch(js, /setAttribute\(\s*["']style/, `${file} sets a style attribute`);
  }
});

test("the menu bar preview uses the badge's own letters", () => {
  const rust = read("../src-tauri/src/tray_badge.rs");
  const js = read("app.js");
  const rows = (text) =>
    text
      .split(",")
      .map((b) => b.trim())
      .filter(Boolean)
      .map((b) => (b.startsWith("0b") ? parseInt(b.slice(2), 2) : parseInt(b, 10)));
  const rustGlyphs = Object.fromEntries(
    [...rust.matchAll(/'([A-Z])' => \[([^\]]+)\]/g)].map((m) => [m[1], rows(m[2])]),
  );
  const jsGlyphs = Object.fromEntries([...js.matchAll(/([A-Z]): \[([^\]]+)\]/g)].map((m) => [m[1], rows(m[2])]));
  assert.ok(Object.keys(rustGlyphs).length >= 10);
  assert.deepEqual(jsGlyphs, rustGlyphs);
});
