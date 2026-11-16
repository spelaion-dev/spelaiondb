// The Plugins page's commands (src-tauri/src/plugins.rs, ADR-055) take no address, key or
// catalogue text: only a plugin's name, a version, an id, a confirmation, or the path of a file
// the user picked. Read from the generated bindings, run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const bindings = readFileSync(new URL("../src/bindings.ts", import.meta.url), "utf8");

/** Every command as `name: (arguments) =>`, with its argument names. */
function commands() {
  const found = new Map();
  for (const match of bindings.matchAll(/^\t(\w+): (?:async )?\(([^)]*)\) =>/gm)) {
    const names = match[2]
      .split(",")
      .map((argument) => argument.trim().split(":")[0].trim())
      .filter(Boolean);
    found.set(match[1], names);
  }
  return found;
}

const ALLOWED = new Set(["install", "name", "version", "confirmed", "path"]);

test("the Plugins page's commands are all there", () => {
  const all = commands();
  for (const name of [
    "pluginsOverview",
    "pluginsRefreshCatalogue",
    "pluginsInstall",
    "pluginsCancelInstall",
    "pluginsInstallFile",
    "pluginsRemove",
  ]) {
    assert.ok(all.has(name), `${name} is missing from bindings.ts`);
  }
});

test("no plugin command takes an address, a key or a catalogue", () => {
  const plugins = [...commands()].filter(([name]) => /plugin/i.test(name));
  assert.ok(plugins.length >= 6);
  for (const [name, args] of plugins) {
    for (const argument of args) {
      assert.ok(ALLOWED.has(argument), `${name} takes ${argument}`);
    }
  }
});

test("no command anywhere takes something named like a catalogue, its key or its address", () => {
  for (const [name, args] of commands()) {
    for (const argument of args) {
      assert.doesNotMatch(argument, /catalog|signature|pubkey|publicKey|minisign/i, `${name} takes ${argument}`);
    }
    // An address argument on a plugin or update command.
    if (/plugin|update/i.test(name)) {
      for (const argument of args) {
        assert.doesNotMatch(argument, /url|address|endpoint|key/i, `${name} takes ${argument}`);
      }
    }
  }
});
