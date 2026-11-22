// The `e2e` feature of spelaion-desktop (src-tauri/src/webview_args.rs, SDB-155), run by
// `npm run build` with `node --test`. With it the app passes WebView2 the smoke test's switches,
// among them a debugging port that controls the page, so it must exist only in the builds made
// for the smoke. These tests read the sources and the workflows:
// - reading SPELAION_E2E_WEBVIEW2_ARGUMENTS is compiled only with the feature;
// - the feature is off by default, and no workflow but the Windows smoke turns it on (never the
//   release workflow);
// - the Windows smoke builds with it, and smoke.mjs asks for its switches through it.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import test from "node:test";

const desktop = resolve(import.meta.dirname, "..");
const root = resolve(desktop, "../..");
const workflows = join(root, ".github/workflows");
const read = (path) => readFileSync(path, "utf8");
const GATE = '#[cfg(feature = "e2e")]';

/** The attribute lines right above line `index`, skipping doc comments and other attributes. */
function attributesAbove(lines, index) {
  const found = [];
  for (let i = index - 1; i >= 0; i--) {
    const line = lines[i].trim();
    if (line.startsWith("///") || line.startsWith("//")) continue;
    if (line.startsWith("#[")) {
      found.push(line);
      continue;
    }
    break;
  }
  return found;
}

test("reading the variable is compiled only with the e2e feature", () => {
  const args = read(join(desktop, "src-tauri/src/webview_args.rs")).split("\n");
  for (const item of ["pub(crate) const SWITCH", "pub(crate) fn from_env"]) {
    const index = args.findIndex((line) => line.startsWith(item));
    assert.ok(index >= 0, `webview_args.rs has ${item}`);
    assert.ok(attributesAbove(args, index).includes(GATE), `${item} is behind ${GATE}`);
  }
  assert.equal(args.filter((line) => line.includes("std::env::var")).length, 1, "only from_env reads the environment");

  // Every use outside the module sits behind the gate too (a gated item used ungated would not
  // compile without the feature; this keeps a new reader of the variable from slipping in).
  const sources = join(desktop, "src-tauri/src");
  for (const name of readdirSync(sources, { recursive: true })) {
    if (!name.endsWith(".rs") || name.endsWith("webview_args.rs")) continue;
    const lines = read(join(sources, name)).split("\n");
    assert.ok(!lines.some((line) => line.includes("SPELAION_E2E_WEBVIEW2_ARGUMENTS")), `${name} names the variable itself`);
    lines.forEach((line, index) => {
      if (!/webview_args::(from_env|SWITCH)/.test(line)) return;
      const before = lines.slice(Math.max(0, index - 6), index).map((l) => l.trim());
      assert.ok(before.includes(GATE), `${name}:${index + 1} uses webview_args behind ${GATE}`);
    });
  }
});

test("the feature is off by default and only the Windows smoke turns it on", () => {
  const manifest = read(join(desktop, "src-tauri/Cargo.toml"));
  assert.match(manifest, /^e2e = \[\]$/m, "Cargo.toml declares e2e = []");
  const defaults = manifest.match(/^default = \[(.*)\]$/m);
  assert.ok(!defaults || !defaults[1].includes("e2e"), "e2e is not a default feature");
  const conf = read(join(desktop, "src-tauri/tauri.conf.json"));
  assert.ok(!/\be2e\b/.test(conf), "tauri.conf.json turns on no e2e feature");
  assert.ok(!/features[^\n]*e2e/.test(read(join(desktop, "package.json"))), "package.json builds without e2e");

  for (const name of readdirSync(workflows).filter((file) => file.endsWith(".yml"))) {
    const text = read(join(workflows, name));
    const mentions = /spelaion-desktop\/e2e|--features[^\n]*\be2e\b/.test(text);
    if (name === "smoke-windows.yml") continue;
    assert.ok(!mentions, `${name} must not build with the e2e feature`);
  }
});

test("the Windows smoke builds with the feature and hands its switches through it", () => {
  const smokeWorkflow = read(join(workflows, "smoke-windows.yml"));
  assert.match(smokeWorkflow, /npx tauri build --no-bundle -- [^\n]*--features spelaion-desktop\/e2e/);
  const smoke = read(join(desktop, "e2e/smoke.mjs"));
  assert.ok(smoke.includes("SPELAION_E2E_WEBVIEW2_ARGUMENTS: ["), "smoke.mjs passes its switches through the app");
  assert.ok(!/WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS:/.test(smoke), "smoke.mjs no longer relies on WebView2's own variable");
});
