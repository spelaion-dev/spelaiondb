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

/** Where else a command appears, besides the palette. The SQL editor's menu has its own
 *  place, with groups and submenus (`editorMenu`). */
export type CommandMenu = "tab" | "file" | "activity" | "welcome" | "result";

/** The SQL editor menu's groups, in order, divided by separators (SDB-438). */
export const EDITOR_MENU_GROUPS = ["run", "clipboard", "edit", "navigate", "view"] as const;
export type EditorMenuGroup = (typeof EDITOR_MENU_GROUPS)[number];

/** The SQL editor menu's submenus; each one's name is `editor:editorMenu.<submenu>`. */
export type EditorSubmenu = "run" | "selection" | "insert" | "goTo" | "view" | "session";

/** Where a command sits in the SQL editor's menu. */
export interface EditorMenuPlace {
  group: EditorMenuGroup;
  submenu?: EditorSubmenu;
  /** It starts a new part of its submenu, after a separator. */
  section?: true;
}

/**
 * Who handles the shortcut: `global` ones `App.tsx`'s handler (they do nothing while a modal is
 * open); `editor` ones the SQL editor, where the focus must be (the palette and the menus only
 * show them); `grid` ones the result grid, where the focus must be (pending table edits, and
 * "Go to row").
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
  /** Its place in the SQL editor's menu (the right click, the Menu key and the ☰ button). */
  editorMenu?: EditorMenuPlace;
  /** One of the SQL editor's own key chords: its keymap binds the shortcut (`editorActions.ts`). */
  editorAction?: true;
  /** The palette's own entry: not listed in the palette. */
  hidden?: boolean;
}

export const COMMANDS = [
  { id: "palette.show", category: "general", label: "app:commands.palette", shortcut: "Ctrl+Shift+P", scope: "global", hidden: true },
  { id: "object.openByName", category: "general", label: "app:commands.openObject", shortcut: "Ctrl+Shift+D", scope: "global" },
  { id: "connection.import", category: "general", label: "connections:importer.open" },
  { id: "connection.add", category: "general", label: "connections:add", menus: ["welcome"] },
  { id: "data.importClipboard", category: "general", label: "app:commands.importClipboard", shortcut: "Ctrl+Alt+V", scope: "global" },
  // Every connection's sessions at once (SDB-395): editor tabs and the explorer's.
  { id: "connections.disconnectAll", category: "general", label: "connections:control.disconnectAll" },
  { id: "connections.disconnectOthers", category: "general", label: "connections:control.disconnectOthers" },
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
  { id: "results.goToRow", category: "data", label: "editor:goToRow.command", shortcut: "Ctrl+G", scope: "grid" },
  { id: "results.maximize", category: "view", label: "app:commands.maximizeResults", shortcut: "Ctrl+Alt+M", scope: "global" },
  { id: "file.open", category: "file", label: "app:files.open", shortcut: "Ctrl+O", scope: "global", menus: ["file", "welcome"] },
  { id: "file.openPlan", category: "file", label: "editor:plan.open", menus: ["file"] },
  { id: "file.save", category: "file", label: "app:tabs.saveFile", shortcut: "Ctrl+S", scope: "global", menus: ["tab"] },
  { id: "file.saveAs", category: "file", label: "app:tabs.saveFileAs", shortcut: "Ctrl+Shift+S", scope: "global", menus: ["tab"] },
  { id: "file.reload", category: "file", label: "app:tabs.reloadFile", menus: ["tab"] },
  // The SQL editor's commands. `editorMenu` places one in the editor's menu (SDB-438): its group,
  // the submenu it is in, and `section` where a submenu starts a new part. A group lists its own
  // commands first and then its submenus, each in this order. `editorAction` ones are the
  // editor's own key chords (its keymap binds them, `editorActions.ts`).
  { id: "editor.run", category: "editor", label: "editor:run", shortcut: "Ctrl+Enter", scope: "editor", editorMenu: { group: "run" } },
  { id: "editor.runScript", category: "editor", label: "editor:runScript", shortcut: "Ctrl+Shift+Enter", scope: "editor", editorMenu: { group: "run", submenu: "run" } },
  { id: "editor.runFromCursor", category: "editor", label: "editor:actions.runFromCursor", shortcut: "Ctrl+Alt+Enter", scope: "editor", editorAction: true, editorMenu: { group: "run", submenu: "run" } },
  { id: "editor.runNewTab", category: "editor", label: "editor:actions.runNewTab", shortcut: "Alt+Enter", scope: "editor", editorAction: true, editorMenu: { group: "run" } },
  { id: "editor.explain", category: "editor", label: "editor:plan.explain", editorMenu: { group: "run" } },
  // A result tab's menu (SDB-378): they act on the tab's active result.
  { id: "results.rerun", category: "data", label: "editor:resultTabs.rerun", menus: ["result"] },
  { id: "results.pin", category: "data", label: "editor:resultTabs.pin", menus: ["result"] },
  { id: "results.rename", category: "data", label: "editor:resultTabs.rename", menus: ["result"] },
  { id: "results.resetName", category: "data", label: "editor:resultTabs.resetName", menus: ["result"] },
  { id: "results.close", category: "data", label: "editor:resultTabs.close", menus: ["result"] },
  { id: "editor.cancel", category: "editor", label: "app:commands.cancelQuery" },
  { id: "editor.variables", category: "editor", label: "editor:variables.command", editorMenu: { group: "run", submenu: "run" } },
  // The clipboard: the editor's own keys, written here so the menu and the palette have them;
  // the keymap leaves them to the editor itself.
  { id: "editor.cut", category: "editor", label: "editor:actions.cut", shortcut: "Ctrl+X", scope: "editor", editorMenu: { group: "clipboard" } },
  { id: "editor.copy", category: "editor", label: "editor:actions.copy", shortcut: "Ctrl+C", scope: "editor", editorMenu: { group: "clipboard" } },
  { id: "editor.paste", category: "editor", label: "editor:actions.paste", shortcut: "Ctrl+V", scope: "editor", editorMenu: { group: "clipboard" } },
  { id: "editor.copyStatement", category: "editor", label: "editor:actions.copyStatement", shortcut: "Ctrl+Alt+C", scope: "editor", editorAction: true, editorMenu: { group: "clipboard" } },
  { id: "editor.selectAll", category: "editor", label: "editor:actions.selectAll", shortcut: "Ctrl+A", scope: "editor", editorMenu: { group: "clipboard" } },
  { id: "editor.format", category: "editor", label: "editor:format", shortcut: "Shift+Alt+F", scope: "editor", editorMenu: { group: "edit" } },
  { id: "editor.lineComment", category: "editor", label: "editor:actions.lineComment", shortcut: "Ctrl+/", scope: "editor", editorAction: true, editorMenu: { group: "edit" } },
  { id: "editor.blockComment", category: "editor", label: "editor:actions.blockComment", shortcut: "Shift+Alt+A", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.upperCase", category: "editor", label: "editor:actions.upperCase", shortcut: "Ctrl+Shift+Alt+U", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.lowerCase", category: "editor", label: "editor:actions.lowerCase", shortcut: "Ctrl+Shift+Alt+L", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.quotedList", category: "editor", label: "editor:actions.quotedList", shortcut: "Ctrl+Alt+Q", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.selectStatement", category: "editor", label: "editor:actions.selectStatement", shortcut: "Ctrl+Alt+S", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection", section: true } },
  { id: "editor.selectOccurrences", category: "editor", label: "editor:actions.selectOccurrences", shortcut: "Ctrl+Shift+L", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.fold", category: "editor", label: "editor:actions.fold", shortcut: "Ctrl+Shift+[", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection", section: true } },
  { id: "editor.unfold", category: "editor", label: "editor:actions.unfold", shortcut: "Ctrl+Shift+]", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "selection" } },
  { id: "editor.insertSelect", category: "editor", label: "editor:actions.insertSelect", shortcut: "Ctrl+Alt+1", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "insert" } },
  { id: "editor.insertInsert", category: "editor", label: "editor:actions.insertInsert", shortcut: "Ctrl+Alt+2", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "insert" } },
  { id: "editor.insertUpdate", category: "editor", label: "editor:actions.insertUpdate", shortcut: "Ctrl+Alt+3", scope: "editor", editorAction: true, editorMenu: { group: "edit", submenu: "insert" } },
  { id: "editor.openObject", category: "editor", label: "editor:actions.openObject", shortcut: "F12", scope: "editor", editorAction: true, editorMenu: { group: "navigate" } },
  { id: "editor.revealInExplorer", category: "editor", label: "editor:actions.revealInExplorer", shortcut: "Shift+Alt+E", scope: "editor", editorAction: true, editorMenu: { group: "navigate" } },
  { id: "editor.nextStatement", category: "editor", label: "editor:actions.nextStatement", shortcut: "Alt+PageDown", scope: "editor", editorAction: true, editorMenu: { group: "navigate", submenu: "goTo" } },
  { id: "editor.previousStatement", category: "editor", label: "editor:actions.previousStatement", shortcut: "Alt+PageUp", scope: "editor", editorAction: true, editorMenu: { group: "navigate", submenu: "goTo" } },
  { id: "editor.matchingBracket", category: "editor", label: "editor:actions.matchingBracket", shortcut: "Ctrl+Shift+\\", scope: "editor", editorAction: true, editorMenu: { group: "navigate", submenu: "goTo" } },
  { id: "editor.outline", category: "editor", label: "editor:actions.outline", shortcut: "Ctrl+Shift+O", scope: "editor", editorAction: true, editorMenu: { group: "navigate", submenu: "goTo" } },
  { id: "editor.wordWrap", category: "editor", label: "editor:actions.wordWrap", shortcut: "Alt+Z", scope: "editor", editorAction: true, editorMenu: { group: "view", submenu: "view" } },
  // The editor's zoom (SDB-389), a personal preference: the editor also takes Ctrl++ for zoom in
  // (the + key, the keypad's, or Shift with =), and the webview's own zoom keys are off.
  { id: "editor.zoomIn", category: "editor", label: "editor:actions.zoomIn", shortcut: "Ctrl+=", scope: "editor", editorAction: true, editorMenu: { group: "view", submenu: "view" } },
  { id: "editor.zoomOut", category: "editor", label: "editor:actions.zoomOut", shortcut: "Ctrl+-", scope: "editor", editorAction: true, editorMenu: { group: "view", submenu: "view" } },
  { id: "editor.zoomReset", category: "editor", label: "editor:actions.zoomReset", shortcut: "Ctrl+0", scope: "editor", editorAction: true, editorMenu: { group: "view", submenu: "view" } },
  // The tab's own session (SDB-395): a new one on request, nothing re-run (ADR-028); and the
  // barrier of a read-only connection for it alone (ADR-053).
  { id: "editor.reconnect", category: "editor", label: "editor:session.reconnect", editorMenu: { group: "view", submenu: "session" } },
  { id: "editor.sessionReadOnly", category: "editor", label: "editor:session.readOnly", editorMenu: { group: "view", submenu: "session" } },
  { id: "view.explorer", category: "view", label: "shell:connections", menus: ["activity"] },
  { id: "view.explorerFilter", category: "view", label: "app:commands.explorerFilter", shortcut: "Ctrl+Shift+F", scope: "global" },
  { id: "view.erDiagram", category: "view", label: "app:commands.erDiagram" },
  { id: "view.processes", category: "view", label: "shell:processMonitor.title", menus: ["activity"] },
  { id: "view.search", category: "view", label: "shell:search.title", shortcut: "Ctrl+H", scope: "global", menus: ["activity"] },
  { id: "view.history", category: "view", label: "shell:queryHistory", menus: ["activity"] },
  { id: "view.savedQueries", category: "view", label: "shell:savedQueries", menus: ["activity"] },
  { id: "view.mcp", category: "view", label: "shell:mcpServer", menus: ["activity"] },
  { id: "view.settings", category: "view", label: "settings:title", shortcut: "Ctrl+,", scope: "global", menus: ["activity"] },
  { id: "view.plugins", category: "view", label: "settings:plugins.title" },
  // Bug reports (SDB-398): the report is the core's, scrubbed; nothing is sent.
  { id: "help.copyDiagnostics", category: "general", label: "app:commands.copyDiagnostics" },
  { id: "help.openIssue", category: "general", label: "app:commands.openIssue" },
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

/** A row of the SQL editor's menu: a command, or a submenu with its commands. */
export type EditorMenuEntry =
  | { kind: "command"; id: CommandId }
  | { kind: "submenu"; submenu: EditorSubmenu; items: { id: CommandId; section: boolean }[] };

/**
 * The SQL editor's menu, group by group (separators go between them): each group's own
 * commands first, then its submenus, all in the registry's order (SDB-438).
 */
export function editorMenu(): EditorMenuEntry[][] {
  const placed = (COMMANDS as readonly CommandSpec[]).filter((command) => command.editorMenu);
  return EDITOR_MENU_GROUPS.map((group) => {
    const inGroup = placed.filter((command) => command.editorMenu!.group === group);
    const own: EditorMenuEntry[] = inGroup
      .filter((command) => !command.editorMenu!.submenu)
      .map((command) => ({ kind: "command", id: command.id as CommandId }));
    const submenus: EditorMenuEntry[] = [];
    for (const command of inGroup) {
      const submenu = command.editorMenu!.submenu;
      if (!submenu) continue;
      let entry = submenus.find((item) => item.kind === "submenu" && item.submenu === submenu);
      if (!entry) submenus.push((entry = { kind: "submenu", submenu, items: [] }));
      if (entry.kind === "submenu") entry.items.push({ id: command.id as CommandId, section: command.editorMenu!.section === true });
    }
    return [...own, ...submenus];
  });
}

/** Every command the SQL editor's menu reaches, submenus included. */
export function editorMenuCommands(): CommandSpec[] {
  return (COMMANDS as readonly CommandSpec[]).filter((command) => command.editorMenu);
}

/**
 * Keys that are not app commands: a region's own keys (the grid, the explorer, a dialog), shown
 * in its tooltips and hints (SDB-86). Each says the row of the shortcut map
 * (`docs/ui-guidelines.md`) that lists it, and `scripts/commands.test.mjs` checks it is there.
 * A gesture is written with `click` as its key ("Ctrl+click"). No translated string writes a
 * shortcut: it interpolates one of these, or a command's, through `shortcuts.ts`.
 */
export const KEYS = {
  submit: { shortcut: "Enter", where: "Dialogs, menus, separators" },
  close: { shortcut: "Escape", where: "Dialogs, menus, separators" },
  switchTabs: { shortcut: "Ctrl+Tab", where: "Anywhere" },
  selectAll: { shortcut: "Ctrl+A", where: "Result grid" },
  copy: { shortcut: "Ctrl+C", where: "Result grid" },
  copyWithHeaders: { shortcut: "Ctrl+Shift+C", where: "Result grid" },
  cellViewer: { shortcut: "Enter", where: "Result grid" },
  findNext: { shortcut: "F3", where: "Result grid" },
  findPrevious: { shortcut: "Shift+F3", where: "Result grid" },
  findNextInBox: { shortcut: "Enter", where: "Result grid" },
  findPreviousInBox: { shortcut: "Shift+Enter", where: "Result grid" },
  findMatchCase: { shortcut: "Alt+C", where: "Result grid" },
  sortThen: { shortcut: "Shift+click", where: "Result grid" },
  selectColumn: { shortcut: "Ctrl+click", where: "Result grid" },
  editCell: { shortcut: "F2", where: "Table data edits" },
  addRow: { shortcut: "Alt+Insert", where: "Table data edits" },
  duplicateRow: { shortcut: "Ctrl+Alt+Insert", where: "Table data edits" },
  deleteRows: { shortcut: "Delete", where: "Table data edits" },
  deleteSelectedRows: { shortcut: "Alt+Delete", where: "Table data edits" },
  setNull: { shortcut: "Ctrl+Alt+N", where: "Table data edits" },
  saveEdits: { shortcut: "Ctrl+S", where: "Table data edits" },
  discardEdits: { shortcut: "Ctrl+R", where: "Table data edits" },
  openOverview: { shortcut: "Ctrl+Enter", where: "Explorer row" },
  pasteConnection: { shortcut: "Ctrl+V", where: "Explorer row" },
  openObjectByClick: { shortcut: "Ctrl+click", where: "SQL editor" },
  snippetNext: { shortcut: "Tab", where: "SQL editor" },
  snippetPrevious: { shortcut: "Shift+Tab", where: "SQL editor" },
} as const satisfies Record<string, { shortcut: string; where: string }>;

export type KeyId = keyof typeof KEYS;

/** How a shortcut is written for the person reading it (SDB-86). */
export interface ShortcutStyle {
  /** macOS: ⌘ for Ctrl (the handlers take Cmd for Ctrl), ⌥ for Alt, ⇧ for Shift, no "+". */
  mac: boolean;
  /** A key's name in the interface's language ("Mayús", "Intro"), or the key as written. */
  keyName(key: string): string;
}

const MAC_MODIFIERS: Record<string, string> = { Ctrl: "⌘", Shift: "⇧", Alt: "⌥" };
const MAC_KEYS: Record<string, string> = { Enter: "↩", Delete: "⌦", Tab: "⇥" };

/**
 * A shortcut as written in this module ("Ctrl+Shift+Enter") put in words for the reader:
 * "Ctrl+Shift+Enter" in English, "Ctrl+Mayús+Intro" in Spanish, "⇧⌘↩" on macOS. A modifier
 * alone ("Ctrl", for "keep Ctrl down") is written the same way.
 */
export function shortcutLabel(shortcut: string, style: ShortcutStyle): string {
  // "Ctrl+-" ends with its key: split on the "+" before the last character.
  const at = shortcut.lastIndexOf("+", shortcut.length - 2);
  const key = at < 0 ? shortcut : shortcut.slice(at + 1);
  const modifiers = at < 0 ? [] : shortcut.slice(0, at).split("+");
  if (!style.mac) return [...modifiers, key].map((part) => style.keyName(part)).join("+");
  // Apple's order: ⌥ ⇧ ⌘, then the key; a click reads "⌘-click".
  const marks = ["Alt", "Shift", "Ctrl"].filter((m) => modifiers.includes(m)).map((m) => MAC_MODIFIERS[m]).join("");
  const keyText = MAC_MODIFIERS[key] ?? MAC_KEYS[key] ?? style.keyName(key);
  return key === "click" && marks ? `${marks}-${keyText}` : marks + keyText;
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
