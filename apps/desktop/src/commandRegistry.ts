// Every app command in one place: its name, its category, its shortcut and the menus that show
// it (SDB-18). The tab menu, the file menu, the activity bar, the welcome screen, the global
// shortcut handler (`App.tsx`) and the command palette all read this list, so a name or a
// shortcut cannot differ between them. What each command does is in `commands.ts`, keyed by
// the same ids (a `Record`, so the compiler refuses a command without an action).
//
// This module imports nothing: `scripts/commands.test.mjs` loads it with `node --test` and
// compares it with the menus' source, the locales and the shortcut map of
// `docs/ui-guidelines.md`.
//
// Menus that act on a selection made in their own region (an explorer row, the grid's cells)
// are not app commands: the palette reaches database objects with their own actions
// (`PaletteObjectAction` in `commands.ts`).

/** Where else a command appears, besides the palette. */
export type CommandMenu = "tab" | "file" | "activity" | "welcome" | "editor";

/**
 * Who handles the shortcut: `global` ones `App.tsx`'s handler (they do nothing while a modal is
 * open); `editor` ones the SQL editor, where the focus must be (the palette and the menus only
 * show them); `grid` ones the result grid of an editable result (pending table edits).
 */
export type ShortcutScope = "global" | "editor" | "grid";

export type CommandCategory = "general" | "tab" | "file" | "editor" | "view" | "data";

export interface CommandSpec {
  id: string;
  category: CommandCategory;
  /** A translation key with its namespace (`app:tabs.rename`). */
  label: string;
  shortcut?: string;
  scope?: ShortcutScope;
  menus?: readonly CommandMenu[];
  /** The palette's own entry: not listed in the palette. */
  hidden?: boolean;
}

export const COMMANDS = [
  { id: "palette.show", category: "general", label: "app:commands.palette", shortcut: "Ctrl+Shift+P", scope: "global", hidden: true },
  { id: "object.openByName", category: "general", label: "app:commands.openObject", shortcut: "Ctrl+Shift+D", scope: "global" },
  { id: "connection.add", category: "general", label: "connections:add", menus: ["welcome"] },
  { id: "data.importClipboard", category: "general", label: "app:commands.importClipboard", shortcut: "Ctrl+Alt+V", scope: "global" },
  { id: "tab.new", category: "tab", label: "app:newQuery", shortcut: "Ctrl+T", scope: "global", menus: ["welcome"] },
  { id: "tab.rename", category: "tab", label: "app:tabs.rename", shortcut: "Ctrl+F2", scope: "global", menus: ["tab"] },
  { id: "tab.duplicate", category: "tab", label: "app:tabs.duplicate", shortcut: "Ctrl+Alt+D", scope: "global", menus: ["tab"] },
  { id: "tab.pin", category: "tab", label: "app:tabs.pin", shortcut: "Ctrl+Alt+P", scope: "global", menus: ["tab"] },
  { id: "tab.close", category: "tab", label: "app:tabs.close", shortcut: "Ctrl+W", scope: "global", menus: ["tab"] },
  { id: "tab.closeOthers", category: "tab", label: "app:tabs.closeOthers", shortcut: "Ctrl+Alt+W", scope: "global", menus: ["tab"] },
  { id: "tab.closeRight", category: "tab", label: "app:tabs.closeRight", shortcut: "Ctrl+Alt+R", scope: "global", menus: ["tab"] },
  { id: "tab.closeAll", category: "tab", label: "app:tabs.closeAll", shortcut: "Ctrl+Shift+W", scope: "global", menus: ["tab"] },
  { id: "tab.reopen", category: "tab", label: "app:tabs.reopen", shortcut: "Ctrl+Shift+T", scope: "global", menus: ["tab"] },
  { id: "tab.next", category: "tab", label: "app:commands.nextTab", shortcut: "Ctrl+PageDown", scope: "global" },
  { id: "tab.previous", category: "tab", label: "app:commands.previousTab", shortcut: "Ctrl+PageUp", scope: "global" },
  { id: "tab.moveRight", category: "tab", label: "app:commands.moveTabRight", shortcut: "Ctrl+Shift+PageDown", scope: "global" },
  { id: "tab.moveLeft", category: "tab", label: "app:commands.moveTabLeft", shortcut: "Ctrl+Shift+PageUp", scope: "global" },
  { id: "edits.undo", category: "data", label: "app:commands.undoEdit", shortcut: "Ctrl+Z", scope: "grid" },
  { id: "edits.redo", category: "data", label: "app:commands.redoEdit", shortcut: "Ctrl+Y", scope: "grid" },
  { id: "results.maximize", category: "view", label: "app:commands.maximizeResults", shortcut: "Ctrl+Alt+M", scope: "global" },
  { id: "file.open", category: "file", label: "app:files.open", shortcut: "Ctrl+O", scope: "global", menus: ["file", "welcome"] },
  { id: "file.openPlan", category: "file", label: "editor:plan.open", menus: ["file"] },
  { id: "file.save", category: "file", label: "app:tabs.saveFile", shortcut: "Ctrl+S", scope: "global", menus: ["tab"] },
  { id: "file.saveAs", category: "file", label: "app:tabs.saveFileAs", shortcut: "Ctrl+Shift+S", scope: "global", menus: ["tab"] },
  { id: "file.reload", category: "file", label: "app:tabs.reloadFile", menus: ["tab"] },
  { id: "editor.run", category: "editor", label: "editor:run", shortcut: "Ctrl+Enter", scope: "editor" },
  { id: "editor.explain", category: "editor", label: "editor:plan.explain", menus: ["editor"] },
  { id: "editor.runScript", category: "editor", label: "editor:runScript", shortcut: "Ctrl+Shift+Enter", scope: "editor" },
  { id: "editor.cancel", category: "editor", label: "app:commands.cancelQuery" },
  { id: "editor.format", category: "editor", label: "editor:format", shortcut: "Shift+Alt+F", scope: "editor" },
  { id: "editor.lineComment", category: "editor", label: "editor:actions.lineComment", shortcut: "Ctrl+/", scope: "editor", menus: ["editor"] },
  { id: "editor.blockComment", category: "editor", label: "editor:actions.blockComment", shortcut: "Shift+Alt+A", scope: "editor", menus: ["editor"] },
  { id: "editor.wordWrap", category: "editor", label: "editor:actions.wordWrap", shortcut: "Alt+Z", scope: "editor", menus: ["editor"] },
  { id: "editor.fold", category: "editor", label: "editor:actions.fold", shortcut: "Ctrl+Shift+[", scope: "editor", menus: ["editor"] },
  { id: "editor.unfold", category: "editor", label: "editor:actions.unfold", shortcut: "Ctrl+Shift+]", scope: "editor", menus: ["editor"] },
  { id: "editor.matchingBracket", category: "editor", label: "editor:actions.matchingBracket", shortcut: "Ctrl+Shift+\\", scope: "editor", menus: ["editor"] },
  { id: "editor.upperCase", category: "editor", label: "editor:actions.upperCase", shortcut: "Ctrl+Shift+Alt+U", scope: "editor", menus: ["editor"] },
  { id: "editor.lowerCase", category: "editor", label: "editor:actions.lowerCase", shortcut: "Ctrl+Shift+Alt+L", scope: "editor", menus: ["editor"] },
  { id: "editor.quotedList", category: "editor", label: "editor:actions.quotedList", shortcut: "Ctrl+Alt+Q", scope: "editor", menus: ["editor"] },
  { id: "editor.selectStatement", category: "editor", label: "editor:actions.selectStatement", shortcut: "Ctrl+Alt+S", scope: "editor", menus: ["editor"] },
  { id: "editor.copyStatement", category: "editor", label: "editor:actions.copyStatement", shortcut: "Ctrl+Alt+C", scope: "editor", menus: ["editor"] },
  { id: "editor.selectOccurrences", category: "editor", label: "editor:actions.selectOccurrences", shortcut: "Ctrl+Shift+L", scope: "editor", menus: ["editor"] },
  { id: "editor.insertSelect", category: "editor", label: "editor:actions.insertSelect", shortcut: "Ctrl+Alt+1", scope: "editor", menus: ["editor"] },
  { id: "editor.insertInsert", category: "editor", label: "editor:actions.insertInsert", shortcut: "Ctrl+Alt+2", scope: "editor", menus: ["editor"] },
  { id: "editor.insertUpdate", category: "editor", label: "editor:actions.insertUpdate", shortcut: "Ctrl+Alt+3", scope: "editor", menus: ["editor"] },
  { id: "view.explorer", category: "view", label: "shell:connections", menus: ["activity"] },
  { id: "view.explorerFilter", category: "view", label: "app:commands.explorerFilter", shortcut: "Ctrl+Shift+F", scope: "global" },
  { id: "view.erDiagram", category: "view", label: "app:commands.erDiagram" },
  { id: "view.processes", category: "view", label: "shell:processMonitor.title", menus: ["activity"] },
  { id: "view.search", category: "view", label: "shell:search.title", shortcut: "Ctrl+H", scope: "global", menus: ["activity"] },
  { id: "view.history", category: "view", label: "shell:queryHistory", menus: ["activity"] },
  { id: "view.savedQueries", category: "view", label: "shell:savedQueries", menus: ["activity"] },
  { id: "view.mcp", category: "view", label: "shell:mcpServer", menus: ["activity"] },
  { id: "view.settings", category: "view", label: "settings:title", shortcut: "Ctrl+,", scope: "global", menus: ["activity"] },
] as const satisfies readonly CommandSpec[];

export type CommandId = (typeof COMMANDS)[number]["id"];

const BY_ID = new Map<string, CommandSpec>(COMMANDS.map((command) => [command.id, command]));

export function commandSpec(id: CommandId): CommandSpec {
  const spec = BY_ID.get(id);
  if (!spec) throw new Error(`Unknown command ${id}`);
  return spec;
}

/** The commands the palette lists, in this order when nothing is typed. */
export function paletteCommands(): CommandSpec[] {
  return COMMANDS.filter((command: CommandSpec) => !command.hidden);
}

/** The commands a menu shows. */
export function menuCommands(menu: CommandMenu): CommandSpec[] {
  return COMMANDS.filter((command: CommandSpec) => command.menus?.includes(menu) ?? false);
}

/** "Label (Shortcut)", for a tooltip. */
export function withShortcut(label: string, spec: CommandSpec): string {
  return spec.shortcut ? `${label} (${spec.shortcut})` : label;
}

const MODIFIERS = ["Ctrl", "Shift", "Alt"] as const;

export interface ParsedShortcut {
  ctrl: boolean;
  shift: boolean;
  alt: boolean;
  /** `KeyboardEvent.key`, lower case. */
  key: string;
}

/**
 * Reads "Ctrl+Shift+PageDown": modifiers in the order Ctrl, Shift, Alt (docs/ui-guidelines.md)
 * and then one key. Null for anything written otherwise, so a typo never becomes a shortcut.
 */
export function parseShortcut(shortcut: string): ParsedShortcut | null {
  const parts = shortcut.split("+");
  // "Ctrl++" would be the plus key: no command uses it.
  if (parts.some((part) => part === "")) return null;
  const key = parts.pop();
  if (!key || (MODIFIERS as readonly string[]).includes(key)) return null;
  let at = 0;
  const parsed: ParsedShortcut = { ctrl: false, shift: false, alt: false, key: key.toLowerCase() };
  for (const part of parts) {
    const index = (MODIFIERS as readonly string[]).indexOf(part);
    if (index < at) return null;
    at = index + 1;
    if (part === "Ctrl") parsed.ctrl = true;
    else if (part === "Shift") parsed.shift = true;
    else parsed.alt = true;
  }
  return parsed;
}

/** The parts of a `KeyboardEvent` a shortcut is matched on. */
export interface KeyPress {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  shiftKey: boolean;
  altKey: boolean;
}

/**
 * Whether the key press is the shortcut: Ctrl or Cmd, and exactly the other modifiers. The key
 * is matched on `key`, so a key that AltGr (Ctrl+Alt on Windows) turns into a character never
 * fires a Ctrl+Alt shortcut.
 */
export function matchesShortcut(press: KeyPress, shortcut: string): boolean {
  const parsed = parseShortcut(shortcut);
  // Some synthetic key events (autofill) have no key.
  if (!parsed || typeof press.key !== "string") return false;
  return (
    parsed.ctrl === (press.ctrlKey || press.metaKey) &&
    parsed.shift === press.shiftKey &&
    parsed.alt === press.altKey &&
    parsed.key === press.key.toLowerCase()
  );
}

/** The global command whose shortcut this key press is, if any. */
export function globalCommandFor(press: KeyPress): CommandSpec | null {
  for (const command of COMMANDS as readonly CommandSpec[]) {
    if (command.scope === "global" && command.shortcut && matchesShortcut(press, command.shortcut)) return command;
  }
  return null;
}
