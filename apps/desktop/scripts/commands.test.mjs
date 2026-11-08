// The command registry (src/commandRegistry.ts) against the menus, the locales and the shortcut
// map of docs/ui-guidelines.md, run by `npm run build` with `node --test` (SDB-18). Every menu
// item of the app's menus is a command of the registry, so the palette reaches all of them.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import {
  COMMANDS,
  globalCommandFor,
  matchesShortcut,
  menuCommands,
  paletteCommands,
  parseShortcut,
} from "../src/commandRegistry.ts";

const src = resolve(import.meta.dirname, "../src");
const read = (path) => readFileSync(resolve(src, path), "utf8");

/** The source of one function (or component) of a file, up to the next top-level one. */
function functionSource(file, name) {
  const code = read(file);
  const start = code.search(new RegExp(`^(export )?(default )?function ${name}\\b`, "m"));
  assert.ok(start >= 0, `${name} in ${file}`);
  const rest = code.slice(start + 1);
  const next = rest.search(/^(export )?(async )?function |^\/\*\*/m);
  return next < 0 ? code.slice(start) : code.slice(start, start + 1 + next);
}

// Each menu: where its items are written, and how a command is put in it there.
const MENUS = {
  tab: { file: "components/TabBar.tsx", fn: "TabMenu", pattern: /item\("([^"]+)"\)/g },
  file: { file: "components/FileMenu.tsx", fn: "FileMenu", pattern: /<CommandMenuItem id="([^"]+)"/g },
  activity: { file: "components/ActivityBar.tsx", fn: "ActivityBar", pattern: /<ActivityButton command="([^"]+)"/g },
  welcome: { file: "App.tsx", fn: "App", pattern: /<WelcomeButton command="([^"]+)"/g },
};

/** Menu items each menu may write without a command: rows that are data, listed by the palette. */
const DATA_ITEMS = { file: 1 }; // the recent files (CommandPalette.tsx lists them)

test("ids are unique and every command is listed in the palette, except the palette itself", () => {
  const ids = COMMANDS.map((c) => c.id);
  assert.equal(new Set(ids).size, ids.length);
  assert.deepEqual(
    COMMANDS.filter((c) => !paletteCommands().includes(c)).map((c) => c.id),
    ["palette.show"],
  );
});

test("every menu item is a command of the registry, and every command of a menu is in it", () => {
  for (const [menu, { file, fn, pattern }] of Object.entries(MENUS)) {
    const code = functionSource(file, fn);
    const used = [...code.matchAll(pattern)].map((m) => m[1]);
    const expected = menuCommands(menu).map((c) => c.id);
    assert.deepEqual([...used].sort(), [...expected].sort(), `${menu} menu`);
    for (const id of used) assert.ok(paletteCommands().some((c) => c.id === id), `${id} in the palette`);
    // No item written by hand beside them.
    const raw = (code.match(/role="menuitem"/g) ?? []).length;
    assert.equal(raw, DATA_ITEMS[menu] ?? 0, `${menu} menu writes ${raw} menu items by hand`);
  }
});

test("the palette lists the recent files the file menu shows", () => {
  const palette = read("components/CommandPalette.tsx");
  assert.match(palette, /recentFiles\(\)/);
  assert.match(palette, /openFilePath\(/);
});

test("every label exists in English and Spanish", () => {
  const locale = (lang, ns) => JSON.parse(read(`locales/${lang}/${ns}.json`));
  const lookup = (table, path) => path.split(".").reduce((value, part) => (value && typeof value === "object" ? value[part] : undefined), table);
  const labels = [
    ...COMMANDS.map((c) => c.label),
    ...COMMANDS.map((c) => `app:commands.categories.${c.category}`),
    // Labels that follow the state (commands.ts).
    "app:tabs.unpin",
    "app:commands.restoreEditor",
    "app:commands.saveQuery",
  ];
  for (const label of labels) {
    const [ns, key] = label.split(":");
    for (const lang of ["en", "es"]) {
      assert.equal(typeof lookup(locale(lang, ns), key), "string", `${lang} ${label}`);
    }
  }
  // Object actions in the palette (commands.ts).
  const actions = [...read("commands.ts").matchAll(/label: "([a-z]+:[A-Za-z.]+)"/g)].map((m) => m[1]);
  assert.ok(actions.length >= 10);
  for (const label of actions) {
    const [ns, key] = label.split(":");
    for (const lang of ["en", "es"]) assert.equal(typeof lookup(locale(lang, ns), key), "string", `${lang} ${label}`);
  }
});

test("every command has an action, and commands.ts binds no command the registry lacks", () => {
  const code = read("commands.ts");
  const body = code.slice(code.indexOf("const BINDINGS"), code.indexOf("export function commandEnabled"));
  const bound = [...body.matchAll(/^ {2}"([a-zA-Z.]+)": \{/gm)].map((m) => m[1]);
  assert.deepEqual([...bound].sort(), COMMANDS.map((c) => c.id).sort());
});

test("shortcuts are written as the guidelines say, unique, and in the shortcut map", () => {
  const docs = readFileSync(resolve(import.meta.dirname, "../../../docs/ui-guidelines.md"), "utf8");
  const row = (where) => docs.split("\n").find((line) => line.startsWith(`| ${where} |`)) ?? "";
  const seen = new Set();
  for (const command of COMMANDS) {
    if (!command.shortcut) {
      assert.equal(command.scope, undefined, `${command.id} has a scope but no shortcut`);
      continue;
    }
    assert.ok(parseShortcut(command.shortcut), `${command.id}: ${command.shortcut}`);
    assert.ok(command.scope, `${command.id} says who handles ${command.shortcut}`);
    assert.ok(!seen.has(command.shortcut), `${command.shortcut} used twice`);
    seen.add(command.shortcut);
    const where = command.scope === "global" ? "Anywhere" : "SQL editor";
    assert.ok(row(where).includes(command.shortcut), `${command.shortcut} (${command.id}) in the "${where}" row of the shortcut map`);
  }
});

test("shortcuts parse with modifiers in the order Ctrl, Shift, Alt", () => {
  assert.deepEqual(parseShortcut("Ctrl+Shift+PageDown"), { ctrl: true, shift: true, alt: false, key: "pagedown" });
  assert.deepEqual(parseShortcut("Shift+Alt+F"), { ctrl: false, shift: true, alt: true, key: "f" });
  assert.deepEqual(parseShortcut("Ctrl+,"), { ctrl: true, shift: false, alt: false, key: "," });
  assert.equal(parseShortcut("Shift+Ctrl+P"), null);
  assert.equal(parseShortcut("Ctrl+Ctrl+P"), null);
  assert.equal(parseShortcut("Ctrl+"), null);
  assert.equal(parseShortcut("Ctrl"), null);
});

const press = (key, mods = {}) => ({ key, ctrlKey: false, metaKey: false, shiftKey: false, altKey: false, ...mods });

test("a key press matches exactly its modifiers; Cmd counts as Ctrl", () => {
  assert.ok(matchesShortcut(press("t", { ctrlKey: true }), "Ctrl+T"));
  assert.ok(matchesShortcut(press("t", { metaKey: true }), "Ctrl+T"));
  assert.ok(matchesShortcut(press("P", { ctrlKey: true, shiftKey: true }), "Ctrl+Shift+P"));
  assert.ok(!matchesShortcut(press("t", { ctrlKey: true, shiftKey: true }), "Ctrl+T"));
  assert.ok(!matchesShortcut(press("t", { ctrlKey: true, altKey: true }), "Ctrl+T"));
  assert.ok(!matchesShortcut(press("t"), "Ctrl+T"));
  assert.ok(!matchesShortcut({ ctrlKey: true, metaKey: false, shiftKey: false, altKey: false }, "Ctrl+T"));
});

test("global shortcuts find their command; AltGr characters and editor keys do not", () => {
  assert.equal(globalCommandFor(press("P", { ctrlKey: true, shiftKey: true }))?.id, "palette.show");
  assert.equal(globalCommandFor(press("d", { ctrlKey: true, altKey: true }))?.id, "tab.duplicate");
  assert.equal(globalCommandFor(press("PageDown", { ctrlKey: true, shiftKey: true }))?.id, "tab.moveRight");
  assert.equal(globalCommandFor(press(",", { ctrlKey: true }))?.id, "view.settings");
  // AltGr+E on a Spanish keyboard is Ctrl+Alt with "€" as its key.
  assert.equal(globalCommandFor(press("€", { ctrlKey: true, altKey: true })), null);
  // Ctrl+Enter is the editor's (it runs only with the focus there).
  assert.equal(globalCommandFor(press("Enter", { ctrlKey: true })), null);
  // Ctrl+Tab is the tab bar's switcher.
  assert.equal(globalCommandFor(press("Tab", { ctrlKey: true })), null);
});
