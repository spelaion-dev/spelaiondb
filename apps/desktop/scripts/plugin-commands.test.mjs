// The Plugins page's commands (src-tauri/src/plugins.rs, ADR-055) take no address, key or
// catalogue text: only a plugin's name, a version, a confirmation, the path of a file the user
// picked, or the channel on which an install's job ticket comes back (SDB-369). Read from the generated bindings, run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
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

const ALLOWED = new Set(["job", "name", "version", "confirmed", "path", "sha256"]);

// The one command that takes an address: what the user typed or pasted into the Plugins page to
// add someone else's plugin (ADR-055, point 5). It is read once by the core, HTTPS only.
const TAKES_AN_ADDRESS = "pluginsInspectAddress";

test("the Plugins page's commands are all there", () => {
  const all = commands();
  for (const name of [
    "pluginsOverview",
    "pluginsRefreshCatalogue",
    "pluginsInstall",
    // An install is stopped as any job is (SDB-369).
    "cancelJob",
    "pluginsInstallFile",
    "pluginsRemove",
    // Other people's plugins (ADR-055, point 5).
    "pluginsInspectFile",
    TAKES_AN_ADDRESS,
    "pluginsAddTheirs",
    "pluginsDiscardTheirs",
    "pluginsTrustAgain",
    "pluginsRemoveTheirs",
  ]) {
    assert.ok(all.has(name), `${name} is missing from bindings.ts`);
  }
});

test("no plugin command takes an address, a key or a catalogue", () => {
  const plugins = [...commands()].filter(([name]) => /plugin/i.test(name));
  assert.ok(plugins.length >= 6);
  for (const [name, args] of plugins) {
    for (const argument of args) {
      if (name === TAKES_AN_ADDRESS && argument === "address") continue;
      assert.ok(ALLOWED.has(argument), `${name} takes ${argument}`);
    }
  }
});

test("only the Plugins page's address field takes an address, and no other command anywhere", () => {
  const all = commands();
  assert.deepEqual(all.get(TAKES_AN_ADDRESS), ["job", "address"]);
  for (const [name, args] of all) {
    if (name === TAKES_AN_ADDRESS) continue;
    if (/plugin/i.test(name)) {
      assert.ok(!args.includes("address"), `${name} takes an address`);
    }
  }
});

test("no command anywhere takes something named like a catalogue, its key or its address", () => {
  for (const [name, args] of commands()) {
    for (const argument of args) {
      assert.doesNotMatch(argument, /catalog|signature|pubkey|publicKey|minisign/i, `${name} takes ${argument}`);
    }
    // An address argument on a plugin or update command, but the one above.
    if (/plugin|update/i.test(name) && name !== TAKES_AN_ADDRESS) {
      for (const argument of args) {
        assert.doesNotMatch(argument, /url|address|endpoint|key/i, `${name} takes ${argument}`);
      }
    }
  }
});

/** Every source file of the page, with its path from `src`. */
function sources(folder = new URL("../src/", import.meta.url), prefix = "") {
  const found = [];
  for (const entry of readdirSync(folder, { withFileTypes: true })) {
    if (entry.isDirectory()) found.push(...sources(new URL(`${entry.name}/`, folder), `${prefix}${entry.name}/`));
    else if (/\.(ts|tsx)$/.test(entry.name)) found.push([`${prefix}${entry.name}`, readFileSync(new URL(entry.name, folder), "utf8")]);
  }
  return found;
}

test("the address is read only from the Plugins page's field, never from a link or anything else", () => {
  const callers = sources()
    .filter(([path, text]) => path !== "bindings.ts" && text.includes(`${TAKES_AN_ADDRESS}(`))
    .map(([path]) => path);
  assert.deepEqual(callers, ["components/TheirPlugins.tsx"]);
});
