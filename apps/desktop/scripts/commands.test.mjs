// The command registry (src/commandRegistry.ts) against the menus, the locales and the shortcut
// map of docs/ui-guidelines.md, run by `npm run build` with `node --test` (SDB-18). Every menu
// item of the app's menus is a command of the registry, so the palette reaches all of them.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import {
  COMMAND_CATEGORIES,
  commandKeys,
  COMMANDS,
  EDITOR_MENU_GROUPS,
  editorMenu,
  editorMenuCommands,
  globalCommandFor,
  keysChanged,
  KEYS,
  matchesShortcut,
  menuCommands,
  paletteCommands,
  parseShortcut,
  pressedShortcut,
  setShortcutOverrides,
  shortcutClash,
  shortcutOverrides,
  SHORTCUT_PLACES,
  shortcutMatches,
  shortcutPlace,
  withKeys,
  shortcutLabel,
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
  result: { file: "components/ResultTabs.tsx", fn: "ResultTabMenu", pattern: /item\("([^"]+)"\)/g },
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
    "editor:resultTabs.unpin",
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
    // The grid's own commands: pending table edits, or the result grid's ("Go to row").
    const grid = command.id.startsWith("edits.") ? "Table data edits" : "Result grid";
    const where = command.scope === "global" ? "Anywhere" : command.scope === "grid" ? grid : "SQL editor";
    assert.ok(row(where).includes(command.shortcut), `${command.shortcut} (${command.id}) in the "${where}" row of the shortcut map`);
  }
});

/** A row of the shortcut map itself (the table after "| Where | Keys |"), not of the places table. */
function shortcutMapRow(where) {
  const docs = readFileSync(resolve(import.meta.dirname, "../../../docs/ui-guidelines.md"), "utf8");
  const map = docs.slice(docs.indexOf("| Where | Keys |"));
  return map.split("\n").find((line) => line.startsWith(`| ${where} |`)) ?? "";
}

test("a region's own keys (KEYS) are written as the guidelines say and in the shortcut map", () => {
  for (const [id, { shortcut, where }] of Object.entries(KEYS)) {
    assert.ok(parseShortcut(shortcut), `${id}: ${shortcut}`);
    assert.ok(shortcutMapRow(where).includes(shortcut), `${shortcut} (KEYS.${id}) in the "${where}" row of the shortcut map`);
  }
});

test("a shortcut is put in words with the language's key names, and with ⌘ ⌥ ⇧ on macOS (SDB-86)", () => {
  const spanish = { Shift: "Mayús", Enter: "Intro", Delete: "Supr", Escape: "Esc", click: "clic" };
  const windows = { mac: false, keyName: (key) => key };
  const es = { mac: false, keyName: (key) => spanish[key] ?? key };
  const mac = { mac: true, keyName: (key) => (key === "Escape" ? "Esc" : key) };
  assert.equal(shortcutLabel("Ctrl+Shift+Enter", windows), "Ctrl+Shift+Enter");
  assert.equal(shortcutLabel("Ctrl+Shift+Enter", es), "Ctrl+Mayús+Intro");
  assert.equal(shortcutLabel("Ctrl+Shift+Enter", mac), "⇧⌘↩");
  assert.equal(shortcutLabel("Shift+Alt+F", mac), "⌥⇧F");
  assert.equal(shortcutLabel("Ctrl+Alt+D", mac), "⌥⌘D");
  assert.equal(shortcutLabel("Ctrl+-", windows), "Ctrl+-");
  assert.equal(shortcutLabel("Ctrl+-", mac), "⌘-");
  assert.equal(shortcutLabel("Ctrl+,", mac), "⌘,");
  assert.equal(shortcutLabel("Escape", mac), "Esc");
  assert.equal(shortcutLabel("Alt+Delete", es), "Alt+Supr");
  assert.equal(shortcutLabel("Ctrl", mac), "⌘");
  assert.equal(shortcutLabel("Ctrl+click", es), "Ctrl+clic");
  assert.equal(shortcutLabel("Ctrl+click", mac), "⌘-click");
});

test("no translated string writes a shortcut: it interpolates one (SDB-86)", () => {
  // Labels that name a key or a setting, not a shortcut to press.
  const names = new Set(["app.json commands.categories.tab", "editor.json results.copyDelimiters.tab", "editor.json export.delimiters.tab", "settings.json editor.tabSize"]);
  const written = /\b(Ctrl|Cmd|Alt|Shift|Mayús)\b|\bF\d{1,2}\b|\((Esc|Escape|Enter|Intro|Del|Supr)\b|\b(press|pulsa|pulsar|before|antes de)\s+(Enter|Intro|Tab|Esc|Escape)\b|^(Enter|Intro|Tab)\b|[;,.]\s+(Enter|Intro|Escape|Tab)\b/;
  for (const lang of ["en", "es"]) {
    const dir = resolve(src, "locales", lang);
    for (const file of ["app.json", "connections.json", "editor.json", "mcp.json", "settings.json", "shell.json"]) {
      const walk = (node, path) => {
        if (typeof node === "string") {
          const at = `${file} ${path}`;
          if (!names.has(at)) assert.ok(!written.test(node), `${lang}/${at} writes a shortcut: ${node}`);
        } else if (node && typeof node === "object") {
          // The key names themselves ("Mayús", "Intro"), which shortcuts.ts reads.
          for (const [key, value] of Object.entries(node)) if (!(file === "shell.json" && path === "" && key === "keys")) walk(value, path ? `${path}.${key}` : key);
        }
      };
      walk(JSON.parse(readFileSync(resolve(dir, file), "utf8")), "");
    }
  }
});

test("a table's or a view's explorer menu and its palette actions offer the same things (SDB-92)", () => {
  // The items the menu's shared components put in it, by their `data-action`.
  const parts = {
    CopyMenuItems: ["copyName", "copyQualifiedName"],
    BookmarkMenuItem: ["bookmark"],
    DropMenuItem: ["drop"],
    FilterMenuItems: ["hide", "showOnly"],
    MaintenanceMenu: ["maintenance"],
  };
  const sidebar = read("components/Sidebar.tsx");
  for (const [part, ids] of Object.entries(parts)) {
    const source = functionSource("components/Sidebar.tsx", part);
    for (const id of ids) assert.match(source, new RegExp(`action="${id}"`), `${part} marks its item ${id}`);
  }
  assert.match(sidebar, /function GenerateMenu\([^)]*action/, "GenerateMenu passes its trigger's action on");
  const ids = (source, pattern) => new Set([...source.matchAll(pattern)].map((m) => m[1]));
  const templates = "generate:*";

  const node = functionSource("components/Sidebar.tsx", "ObjectNode");
  const menu = ids(node, /\baction="([^"]+)"/g);
  for (const [part, partIds] of Object.entries(parts)) if (node.includes(`<${part}`)) for (const id of partIds) menu.add(id);
  if (node.includes("data-action={`generate:${template}`}")) menu.add(templates);

  const actions = functionSource("commands.ts", "relationActions");
  const palette = ids(actions, /\bid: "([^"]+)"/g);
  if (actions.includes("id: `generate:${template}`")) palette.add(templates);

  // Items that need the row they were opened on, or what was chosen in the tree.
  const menuOnly = new Set([
    "countRows", // its result is drawn in the row
    "refresh", // reads the row's details again
    "hide", // changes the connection's explorer filter through its node
    "showOnly",
    "join", // the tables chosen with Ctrl+click
    "innerJoin",
    "maintenance", // a submenu of the actions the core lists for that object
  ]);
  const paletteOnly = new Set(["openStructure", "newConsole", "showInExplorer"]);
  const without = (set, others) => [...set].filter((id) => !others.has(id)).sort();
  assert.deepEqual(without(menu, menuOnly), without(palette, paletteOnly));
  for (const id of menuOnly) assert.ok(menu.has(id), `${id} is still in the menu`);
  for (const id of paletteOnly) assert.ok(palette.has(id), `${id} is still in the palette`);
  // Both call the shared actions, not their own copies.
  for (const shared of ["openRelationSql", "exportRelations", "importIntoTable", "openRelationDiagram", "editViewSource", "duplicateTable", "renameRelation", "dropRelation", "openRelationDdl"]) {
    assert.match(node, new RegExp(`\\b${shared}\\(`), `the menu calls ${shared}`);
    assert.match(actions, new RegExp(`\\b${shared}\\(`), `the palette calls ${shared}`);
  }
});

/** The items of a row's explorer menu (`<ObjectMenu>` in `fn` of Sidebar.tsx), by their action:
 *  `data-action` / `action`, those of the shared parts it uses, and `prefix:*` for an item per
 *  kind. Every item written there carries its action, so none can be left out of the comparison. */
function explorerMenu(fn) {
  const parts = {
    CopyMenuItems: ["copyName", "copyQualifiedName"],
    BookmarkMenuItem: ["bookmark"],
    DropMenuItem: ["drop"],
    FilterMenuItems: ["hide", "showOnly"],
    MaintenanceMenu: ["maintenance"],
  };
  const node = functionSource("components/Sidebar.tsx", fn);
  const menu = node.slice(node.indexOf("<ObjectMenu"), node.indexOf("</ObjectMenu>"));
  assert.ok(menu.length > 0, `${fn} has a menu`);
  const ids = new Set();
  for (const m of menu.matchAll(/\baction=(?:"([^"]+)"|\{`([a-zA-Z]+):\$\{[^}]+\}`\})/g)) ids.add(m[1] ?? `${m[2]}:*`);
  for (const [part, partIds] of Object.entries(parts)) if (menu.includes(`<${part}`)) for (const id of partIds) ids.add(id);
  for (const tag of menu.split(/<(?=button\b|ShortcutMenuItem\b)/).slice(1)) {
    const attributes = tag.slice(0, tag.indexOf(">"));
    if (/^button\b/.test(tag) && !attributes.includes('role="menuitem"')) continue;
    assert.match(attributes, /\baction=/, `${fn}: a menu item without its action: <${attributes.slice(0, 80)}`);
  }
  return { node, ids };
}

/** The ids a palette function of commands.ts offers, with `prefix:*` for one per kind. */
function paletteActions(fn) {
  const source = functionSource("commands.ts", fn);
  const ids = new Set([...source.matchAll(/\bid: "([^"]+)"/g)].map((m) => m[1]));
  for (const m of source.matchAll(/\bid: `([a-zA-Z]+):\$\{[^}]+\}`/g)) ids.add(`${m[1]}:*`);
  return { source, ids };
}

/** The menu and the palette offer the same, but for the items listed with their reason. */
function sameActions(name, menu, palette, menuOnly, paletteOnly) {
  const without = (set, others) => [...set].filter((id) => !(id in others)).sort();
  assert.deepEqual(without(menu, menuOnly), without(palette, paletteOnly), `${name}: menu and palette`);
  for (const id of Object.keys(menuOnly)) assert.ok(menu.has(id), `${name}: ${id} is still in the menu`);
  for (const id of Object.keys(paletteOnly)) assert.ok(palette.has(id), `${name}: ${id} is still in the palette`);
}

const PALETTE_ONLY = {
  newConsole: "a new query on the object's schema, which the tree opens by double-clicking",
  showInExplorer: "the tree is where the menu already is",
};

test("a schema's and a schema object's explorer menus and their palette actions offer the same things (SDB-441)", () => {
  const schema = explorerMenu("SchemaNode");
  const schemaPalette = paletteActions("schemaActions");
  sameActions("schema", schema.ids, schemaPalette.ids, {
    refresh: "reads the row's objects again",
    refreshAll: "reads every schema row of the connection again",
    hide: "changes the connection's explorer filter through its node",
    showOnly: "changes the connection's explorer filter through its node",
    maintenance: "a submenu of the actions the core lists for that object",
  }, PALETTE_ONLY);
  for (const shared of ["setEditorSchema", "importIntoNewTable", "openSchemaDiagram", "compareSchemas", "createInSchema", "dropSchema", "schemaBookmark"]) {
    assert.match(schema.node, new RegExp(`\\b${shared}\\(`), `the schema menu calls ${shared}`);
    assert.match(schemaPalette.source, new RegExp(`\\b${shared}\\(`), `the schema palette calls ${shared}`);
  }

  const object = explorerMenu("SchemaObjectNode");
  const objectPalette = paletteActions("schemaObjectActions");
  sameActions("schema object", object.ids, objectPalette.ids, {
    hide: "changes the connection's explorer filter through its node",
    showOnly: "changes the connection's explorer filter through its node",
  }, PALETTE_ONLY);
  for (const shared of ["openObjectDefinition", "editRoutineSource", "openRoutineCall", "dropSchemaObject", "schemaObjectBookmark", "canEditRoutineSource", "canRunRoutine", "canGenerateCall"]) {
    assert.match(object.node, new RegExp(`\\b${shared}\\(`), `the schema object menu calls ${shared}`);
    assert.match(objectPalette.source, new RegExp(`\\b${shared}\\(`), `the schema object palette calls ${shared}`);
  }
});

test("a column's and an index's explorer menus stay menu-only, each item with its reason (SDB-441)", () => {
  // The palette lists what the explorer catalog holds (schemas, relations and other schema
  // objects, catalog.ts); a table's columns and indexes are read only when its row opens, so
  // they are not palette objects: every item of their menus needs that row.
  const notInPalette = "columns and indexes are not palette objects: they are read when their table's row opens";
  const column = explorerMenu("ColumnRow");
  sameActions("column", column.ids, new Set(), Object.fromEntries([
    "copyName", "copyQualifiedName", "rename", "editColumn", "drop", "hide", "showOnly", "newIndex:*",
  ].map((id) => [id, notInPalette])), {});
  const index = explorerMenu("IndexNode");
  sameActions("index", index.ids, new Set(), Object.fromEntries(["openDefinition", "copyName", "copyQualifiedName", "drop"].map((id) => [id, notInPalette])), {});
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

test("the editor menu is the registry's groups and submenus; the keymap binds only the editor's own keys (SDB-438)", () => {
  const groups = editorMenu();
  assert.deepEqual(groups.length, EDITOR_MENU_GROUPS.length);
  // 18 rows: each group's own commands first, then its submenus.
  assert.equal(groups.flat().length, 18);
  for (const group of groups) {
    const firstSubmenu = group.findIndex((entry) => entry.kind === "submenu");
    if (firstSubmenu >= 0) assert.ok(group.slice(firstSubmenu).every((entry) => entry.kind === "submenu"), "submenus come last in a group");
  }
  assert.deepEqual(groups.map((group) => group.map((entry) => entry.kind === "command" ? entry.id : `${entry.submenu} ▸`)), [
    ["editor.run", "editor.runNewTab", "editor.explain", "run ▸"],
    ["editor.cut", "editor.copy", "editor.paste", "editor.copyStatement", "editor.selectAll"],
    ["editor.format", "editor.lineComment", "selection ▸", "insert ▸"],
    ["editor.openObject", "editor.revealInExplorer", "goTo ▸"],
    ["view ▸", "session ▸"],
  ]);
  const reached = groups.flat().flatMap((entry) => entry.kind === "command" ? [entry.id] : entry.items.map((item) => item.id));
  assert.deepEqual([...reached].sort(), editorMenuCommands().map((command) => command.id).sort());
  // Every SQL editor command has a place in it; Cancel is Run's place while a query runs.
  for (const command of COMMANDS.filter((command) => command.category === "editor" && command.id !== "editor.cancel")) {
    assert.ok(command.editorMenu, `${command.id} has a group in the editor menu`);
  }
  // The editor's zoom (SDB-389): Ctrl+= / Ctrl+- / Ctrl+0, the editor's own keys.
  const editor = editorMenuCommands();
  assert.deepEqual(editor.filter((command) => command.id.startsWith("editor.zoom")).map((command) => command.shortcut), ["Ctrl+=", "Ctrl+-", "Ctrl+0"]);
  for (const key of ["=", "-", "0", "+"]) assert.equal(globalCommandFor(press(key, { ctrlKey: true })), null, `Ctrl+${key} is the editor's`);
  // Variables… opens the tab's variables (SDB-383): a menu item without a key, like Explain.
  assert.ok(editor.some(command => command.id === "editor.variables" && command.shortcut === undefined));
  // Run in a new result tab is the editor's own key, like its other actions (SDB-378).
  assert.ok(editor.some(command => command.id === "editor.runNewTab" && command.shortcut === "Alt+Enter" && command.editorAction));
  assert.ok(editor.some(command => command.id === "editor.explain" && command.shortcut === undefined));
  for (const command of editor) {
    assert.equal(command.scope, command.shortcut ? "editor" : undefined);
    assert.ok(paletteCommands().includes(command));
  }
  // The keymap's own chords have a key; Run, Format and the clipboard keep theirs where they are.
  for (const command of COMMANDS.filter((command) => command.editorAction)) assert.ok(command.shortcut, command.id);
  for (const id of ["editor.run", "editor.runScript", "editor.format", "editor.cut", "editor.copy", "editor.paste", "editor.selectAll"]) {
    assert.ok(!COMMANDS.find((command) => command.id === id).editorAction, `${id} is not bound by the editor's keymap`);
  }
  const menu = functionSource("components/EditorMenu.tsx", "EditorMenu");
  assert.match(menu, /editorMenu\(\)/);
  assert.match(menu, /runCommand\(/);
  assert.doesNotMatch(menu, /role="menuitem"[^>]*>\s*\{?t\("/, "no item written by hand");
  assert.match(read("editorActions.ts"), /command\.editorAction/);
});

test("Settings lists every command in one place and one of its categories (SDB-447)", () => {
  for (const command of COMMANDS) {
    assert.ok(COMMAND_CATEGORIES.includes(command.category), `${command.id}: category ${command.category}`);
    const place = shortcutPlace(command);
    assert.ok(SHORTCUT_PLACES.includes(place), `${command.id}: place ${place}`);
    assert.equal(place === "none", !command.shortcut, `${command.id}: listed without keys only when it has none`);
  }
  const page = read("components/ShortcutSettings.tsx");
  assert.match(page, /COMMANDS/, "the page lists the registry's commands");
  assert.match(page, /shortcutText\(/, "the page writes keys through shortcuts.ts");
});

test("the shortcut search finds a command by its name or by its keys (SDB-447)", () => {
  const texts = ["Run script", "Ctrl+Enter", "Ctrl+Enter"];
  assert.ok(shortcutMatches("", texts), "an empty search lists everything");
  assert.ok(shortcutMatches("  ", texts), "spaces alone too");
  assert.ok(shortcutMatches("run", texts), "by name, without case");
  assert.ok(shortcutMatches("ctrl+enter", texts), "by keys as typed");
  assert.ok(shortcutMatches("Ctrl + Enter", texts), "spaces around the + do not matter");
  assert.ok(!shortcutMatches("ctrl+shift+enter", texts), "other keys do not match");
  assert.ok(shortcutMatches("mayús", ["Format", "Shift+Alt+F", "Mayús+Alt+F"]), "by the keys as shown in the language");
});

const byId = (id) => COMMANDS.find((command) => command.id === id);
const keyPress = (key, { ctrl = false, shift = false, alt = false, meta = false } = {}) => ({ key, ctrlKey: ctrl, shiftKey: shift, altKey: alt, metaKey: meta });

test("the user's keys act in place of the defaults, and only the differences are kept (SDB-447)", () => {
  try {
    setShortcutOverrides([
      { command: "tab.new", keys: "Ctrl+Alt+F9" },
      { command: "editor.runScript", keys: null },
      { command: "tab.close", keys: "Ctrl+W" }, // its default: not a change
      { command: "no.such.command", keys: "Ctrl+Alt+F8" }, // an id that is no longer a command
      { command: "editor.copy", keys: "Ctrl+Alt+F7" }, // keys the editor keeps
    ]);
    assert.deepEqual(shortcutOverrides(), [{ command: "tab.new", keys: "Ctrl+Alt+F9" }, { command: "editor.runScript", keys: null }]);
    assert.equal(commandKeys(byId("tab.new")), "Ctrl+Alt+F9");
    assert.equal(globalCommandFor(keyPress("F9", { ctrl: true, alt: true }))?.id, "tab.new", "the new keys run it");
    assert.equal(globalCommandFor(keyPress("t", { ctrl: true })), null, "the old keys stop");
    assert.equal(commandKeys(byId("editor.runScript")), undefined, "removed");
    assert.equal(shortcutPlace(byId("editor.runScript")), "none");
    assert.equal(commandKeys(byId("editor.copy")), "Ctrl+C", "fixed keys keep their default");
    assert.ok(keysChanged(byId("tab.new")) && !keysChanged(byId("tab.close")));
    const changes = shortcutOverrides();
    assert.deepEqual(withKeys(changes, byId("tab.new"), "Ctrl+T"), [{ command: "editor.runScript", keys: null }], "its default again drops the change");
    assert.deepEqual(withKeys(changes, byId("tab.rename"), null).at(-1), { command: "tab.rename", keys: null });
  } finally {
    setShortcutOverrides([]);
  }
  assert.equal(commandKeys(byId("tab.new")), "Ctrl+T");
});

test("a clash is judged by where the keys act (SDB-447)", () => {
  for (const command of COMMANDS) {
    if (command.shortcut) assert.equal(shortcutClash(command, command.shortcut), null, `${command.id}: its own default`);
  }
  assert.equal(shortcutClash(byId("tab.close"), "Ctrl+T")?.command?.id, "tab.new", "two commands anywhere");
  assert.equal(shortcutClash(byId("editor.runScript"), "Ctrl+T")?.command?.id, "tab.new", "anywhere takes the editor's keys too");
  assert.equal(shortcutClash(byId("tab.new"), "Ctrl+Enter")?.command?.id, "editor.run", "and the editor's keys anywhere");
  assert.equal(shortcutClash(byId("edits.undo"), "Ctrl+Alt+Enter"), null, "the editor and the grid are focused apart");
  assert.deepEqual(shortcutClash(byId("editor.format"), "Ctrl+C"), { kind: "fixed", command: byId("editor.copy") });
  assert.deepEqual(shortcutClash(byId("editor.format"), "Ctrl+D"), { kind: "region", key: "selectNextOccurrence", place: "editor" });
  assert.deepEqual(shortcutClash(byId("edits.undo"), "Ctrl+D"), { kind: "region", key: "fillDown", place: "grid" });
  assert.equal(shortcutClash(byId("tab.new"), "Ctrl+D"), null, "a focused area's own keys do not stop a command anywhere");
  assert.deepEqual(shortcutClash(byId("tab.new"), "Ctrl+Tab"), { kind: "region", key: "switchTabs", place: "global" });
});

test("the dialog writes pressed keys as the registry does (SDB-447)", () => {
  assert.equal(pressedShortcut(keyPress("a", { ctrl: true })), "Ctrl+A");
  assert.equal(pressedShortcut(keyPress("a", { meta: true })), "Ctrl+A", "Cmd counts as Ctrl");
  assert.equal(pressedShortcut(keyPress("F", { shift: true, alt: true })), "Shift+Alt+F");
  assert.equal(pressedShortcut(keyPress("Enter", { ctrl: true, shift: true })), "Ctrl+Shift+Enter");
  assert.equal(pressedShortcut(keyPress(" ", { ctrl: true })), "Ctrl+Space");
  for (const modifier of ["Control", "Shift", "Alt", "Meta", "AltGraph"]) assert.equal(pressedShortcut(keyPress(modifier, { ctrl: true })), null, modifier);
  for (const keys of ["Ctrl+A", "Shift+Alt+F", "Ctrl+Space", "Ctrl+Shift+Enter"]) {
    assert.ok(parseShortcut(keys), keys);
  }
  assert.ok(matchesShortcut(keyPress(" ", { ctrl: true }), "Ctrl+Space"), "the space bar matches Space");
});

test("no handler or keymap writes a registry command's keys by hand (SDB-447)", () => {
  const defaults = new Set();
  for (const command of COMMANDS) {
    if (!command.shortcut || command.fixedKeys) continue;
    defaults.add(command.shortcut.toLowerCase());
    defaults.add(command.shortcut.split("+").map((part) => (part === "Ctrl" ? "Mod" : part)).join("-").toLowerCase());
  }
  for (const file of ["App.tsx", "components/SqlEditor.tsx", "components/ResultGrid.tsx", "components/Sidebar.tsx", "components/TabBar.tsx", "editorActions.ts"]) {
    const code = read(file);
    for (const [, literal] of code.matchAll(/["'`]((?:Ctrl|Mod|Shift|Alt)[+-][^"'`\s]+)["'`]/g)) {
      assert.ok(!defaults.has(literal.toLowerCase()), `${file} writes ${literal}, a command's keys: read them from the registry`);
    }
  }
  assert.match(read("components/SqlEditor.tsx"), /editorKeyBindings\(\)/, "the editor's keymap is the registry's");
  assert.match(read("components/ResultGrid.tsx"), /commandKeys\(commandSpec\("edits\.undo"\)\)/, "the grid reads undo's keys now");
  assert.doesNotMatch(read("components/ResultGrid.tsx"), /commandSpec\("[^"]+"\)\.shortcut \?\?/, "never a command's default alone");
});

test("no command's keys act while a dialog is open, whatever keys the user gave it (SDB-447)", () => {
  const app = read("App.tsx");
  const handler = app.slice(app.indexOf("const onKey = (e: KeyboardEvent) =>"));
  const modal = handler.indexOf("const modal = topDialog();");
  const run = handler.indexOf("runCommand(command.id");
  assert.ok(modal > 0 && run > modal, "the global handler looks for an open dialog before it runs a command");
  assert.match(handler.slice(modal, run), /if \(modal\) \{[\s\S]*return;\s*\}/, "and runs nothing while one is open");
  const dialog = read("components/ShortcutSettings.tsx");
  assert.match(dialog, /event\.stopPropagation\(\)/, "keys pressed in the shortcut dialog never reach the app behind it");
});
