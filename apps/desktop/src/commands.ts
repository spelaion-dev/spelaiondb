// What each command of `commandRegistry.ts` does, when it can run and, for the few whose name
// follows the state (Pin / Unpin), its name now. Menus, the global shortcut handler and the
// command palette all run commands through `runCommand`, so they cannot do different things.
// Everything here is UI: anything that reaches a database goes through the same core commands
// as the buttons and menus it stands for.

import { call, commands as core, type ConnectionItem, type NameFilter, type ObjectItem, type SchemaObjectItem } from "./api";
import { windowContext } from "./windowContext";
import { detachResults, openConnectionWindow, reattachResults } from "./resultsWindow";
import { COMMANDS, commandSpec, type CommandId, type CommandSpec } from "./commandRegistry";
import { answerWindowCloseRequest, closeQueryTab, connectTabFromExplorer, explorerDatabase, newQueryOnConnection, tabLabel } from "./connect";
import { readClipboardText } from "./clipboardText";
import { toggleSidePanel } from "./sidePanel";
import { duplicateConnection } from "./connectionCopies";
import { disconnectAll, hasSessions } from "./connectionControl";
import { openFileDialog, reloadTab, saveTab, saveTabAs } from "./sqlFiles";
import { openSavedPlan } from "./planFiles";
import { openDiagram } from "./diagramTabs";
import { usePreferences } from "./preferences";
import { copyDiagnostics, openIssuePage } from "./diagnostics";
import { EDITOR_COMMAND_EVENT, EDITS_COMMAND_EVENT, GRID_COMMAND_EVENT, SAVE_QUERY_EVENT, useApp, type EditorCommand, type GridCommand, type ResultMeta, type Tab } from "./store";
import {
  closable,
  closeAllTabs,
  closeOtherTabs,
  closeTabsToTheRight,
  closeSplit,
  duplicateTab,
  focusOtherPane,
  moveTabBy,
  splitTab,
  startRename,
  toggleResultsMaximized,
  togglePinned,
} from "./tabActions";
import { FOCUS_EXPLORER_FILTER_EVENT } from "./components/Sidebar";
import { FOCUS_SEARCH_EVENT } from "./components/SearchPanel";
import { openFound } from "./components/OpenObjectDialog";
import {
  canEditRoutineSource,
  canGenerateCall,
  canRenameRelation,
  canRunRoutine,
  createInSchema,
  dropRelation,
  dropSchema,
  dropSchemaObject,
  duplicateTable,
  editRoutineSource,
  editViewSource,
  explorerSession,
  exportRelations,
  importIntoNewTable,
  copyToConnection,
  importIntoTable,
  isBookmarked,
  isPlainTable,
  knownObjectActions,
  openObjectDefinition,
  openRelationDdl,
  openRelationDiagram,
  openRelationSql,
  openRoutineCall,
  openSchemaDiagram,
  relationBookmark,
  relationTemplates,
  renameRelation,
  schemaBookmark,
  schemaCreateKinds,
  schemaEditorTab,
  schemaObjectBookmark,
  setEditorSchema,
  toggleBookmark,
} from "./explorerActions";
import { compareSchemas } from "./schemaCompare";
import { useImport } from "./components/ImportDialog";
import { ended } from "./resultEnd";
import { canStartRun, explorerSessionOpen, isConnected, isRunning, sessionReadOnly } from "./tabState";
import { connectionKey, keyOf, listedConnection, tabKey, tabOnConnection, type ConnectionKey } from "./connectionKey";

export interface CommandContext {
  /** The tab the command acts on: the menu's tab, or the active one (none behind Settings). */
  tab: string | null;
  source: "key" | "menu" | "palette";
  /** For a shortcut, where the key was pressed. */
  target?: EventTarget | null;
}

interface CommandBinding {
  run(context: CommandContext): unknown;
  /** Whether it can run now: a menu shows it disabled, the palette leaves it out. */
  enabled?(context: CommandContext): boolean;
  /** Its name now, when that follows the state (a translation key, like the registry's). */
  label?(context: CommandContext): string;
}

function tabOf(context: CommandContext): Tab | undefined {
  return context.tab ? useApp.getState().tabs.find((tab) => tab.id === context.tab) : undefined;
}

/** A SQL editor tab (not a table's data or details). */
function editorTab(context: CommandContext): Tab | undefined {
  const tab = tabOf(context);
  return tab && !tab.table && !tab.tableDetails && !tab.plan && !tab.diagram ? tab : undefined;
}

/** The result a SQL editor tab shows now, whose tab the result commands act on. */
function activeResult(context: CommandContext): ResultMeta | undefined {
  const run = editorTab(context)?.run;
  return run?.results.find((result) => result.id === run.activeResult);
}

/** A SQL editor tab where a run can start now, as its Run button says: a disconnected tab can
 *  (Run connects it first), one running or connecting cannot (SDB-371). */
function runnableTab(context: CommandContext): boolean {
  const tab = editorTab(context);
  return tab !== undefined && canStartRun(tab);
}

/** A SQL editor tab whose run is not going. */
function idleEditorTab(context: CommandContext): boolean {
  const tab = editorTab(context);
  return tab !== undefined && !isRunning(tab);
}

function editorCommand(command: EditorCommand): CommandBinding["run"] {
  return (context) => {
    const tab = editorTab(context);
    if (tab) window.dispatchEvent(new CustomEvent(EDITOR_COMMAND_EVENT, { detail: { tab: tab.id, command } }));
  };
}

/** The result whose grid a tab shows now: a SQL editor's or a table data tab's. */
function gridResult(context: CommandContext): ResultMeta | undefined {
  const tab = tabOf(context);
  if (!tab || tab.tableDetails || tab.plan || tab.diagram) return undefined;
  return tab.run?.results.find((result) => result.id === tab.run?.activeResult);
}

/** A grid with rows, whose selection a grid command acts on. */
function hasRows(context: CommandContext): boolean {
  return (gridResult(context)?.rows ?? 0) > 0;
}

/** Sends a command to the grid of the tab's result, which acts on it as its menu does. */
function gridCommand(command: GridCommand): CommandBinding["run"] {
  return (context) => {
    const result = gridResult(context);
    if (result) window.dispatchEvent(new CustomEvent(GRID_COMMAND_EVENT, { detail: { result: result.id, command } }));
  };
}

/** A SQL editor tab with its session open. */
function connectedEditorTab(context: CommandContext): boolean {
  const tab = editorTab(context);
  return tab !== undefined && isConnected(tab);
}

/** A tab whose result may hold pending table edits (a table's data, or a query's). */
function editableTab(context: CommandContext): Tab | undefined {
  const tab = tabOf(context);
  return tab && !tab.tableDetails && !tab.plan && !tab.diagram && isConnected(tab) ? tab : undefined;
}

function editsCommand(command: "undo" | "redo"): CommandBinding["run"] {
  return (context) => {
    const tab = editableTab(context);
    if (tab) window.dispatchEvent(new CustomEvent(EDITS_COMMAND_EVENT, { detail: { tab: tab.id, command } }));
  };
}

function toggleSection(section: "connections" | "processes" | "search") {
  useApp.getState().setSidebarSection((current) => (current === section ? null : section));
}

/** The open session an import from the active tab copies: the tab's own, or the explorer's for
 *  its connection. */
function importSession(context: CommandContext): { session: string; connection: ConnectionKey } | null {
  const tab = tabOf(context);
  if (!tab?.connectionId) return null;
  const connection = connectionKey(tab.connectionId);
  if (isConnected(tab)) return { session: tab.id, connection };
  if (explorerSessionOpen(useApp.getState().explorerConnected, connection)) {
    return { session: explorerSession(connection), connection };
  }
  return null;
}

/** Opens the import with the clipboard's text, into the active tab's connection (ADR-038). */
async function importFromClipboard(context: CommandContext) {
  const target = importSession(context);
  if (!target) return;
  let text: string | null = null;
  try {
    text = (await readClipboardText()) || null;
  } catch {
    // The dialog's own Paste button says why the clipboard could not be read.
  }
  const schema = tabOf(context)?.table?.schema ?? null;
  useImport.getState().open({ ...target, schema, table: null, text });
}

/** The connected tab an "ER diagram" command starts from: any but a diagram or a plan. */
function diagramSource(context: CommandContext): Tab | undefined {
  const tab = tabOf(context);
  return tab && tab.connectionId && !tab.diagram && !tab.plan && isConnected(tab) ? tab : undefined;
}

/** A diagram around the active tab's table, or of its editor's current schema. */
async function openActiveDiagram(context: CommandContext) {
  const tab = diagramSource(context);
  if (!tab?.connectionId) return;
  const table = tab.table ?? tab.tableDetails;
  if (table) {
    openDiagram(tab.connectionId, { kind: "focus", schema: table.schema, table: table.name }, tab.id);
    return;
  }
  const schema = await call(core.activeSchema(tab.id));
  if (schema !== null) openDiagram(tab.connectionId, { kind: "schema", schema, filter: null }, tab.id);
}

/** The identity of the active tab's connection, if it has one. */
function tabConnection(context: CommandContext): ConnectionKey | null {
  const id = tabOf(context)?.connectionId;
  return id ? connectionKey(id) : null;
}

function stepTab(back: boolean) {
  const app = useApp.getState();
  const order = app.tabs.map((tab) => tab.id);
  if (order.length === 0) return;
  const at = Math.max(0, order.indexOf(app.activeTab ?? ""));
  const next = order[(at + (back ? order.length - 1 : 1)) % order.length];
  if (next) app.setActiveTab(next);
}

const BINDINGS: Record<CommandId, CommandBinding> = {
  "palette.show": { run: () => useApp.getState().setPaletteOpen(true) },
  "object.openByName": { run: () => useApp.getState().setOpenObject({ connection: null }) },
  "connection.import": { run: () => useApp.getState().setConnectionDialog({ kind: "add", imports: true }) },
  "connection.add": { run: () => useApp.getState().setConnectionDialog({ kind: "add" }) },
  "data.importClipboard": {
    enabled: (c) => importSession(c) !== null,
    run: (c) => importFromClipboard(c),
  },
  "tab.new": { run: () => useApp.getState().addTab(null) },
  "tab.rename": {
    enabled: (c) => tabOf(c) !== undefined,
    run: (c) => c.tab && startRename(c.tab),
  },
  "tab.duplicate": {
    enabled: (c) => {
      const tab = tabOf(c);
      return tab !== undefined && !tab.tableDetails && !tab.plan && !tab.diagram;
    },
    run: (c) => c.tab && duplicateTab(c.tab),
  },
  "tab.pin": {
    enabled: (c) => tabOf(c) !== undefined,
    label: (c) => (tabOf(c)?.pinned ? "app:tabs.unpin" : "app:tabs.pin"),
    run: (c) => c.tab && togglePinned(c.tab),
  },
  "tab.close": {
    enabled: (c) => tabOf(c) !== undefined || useApp.getState().settingsOpen,
    run: (c) => {
      // Ctrl+W closes Settings when it is open (it takes the tabs' place).
      if (useApp.getState().settingsOpen) useApp.getState().closeSettings();
      else if (c.tab) void closeQueryTab(c.tab);
    },
  },
  "tab.closeOthers": {
    enabled: (c) => c.tab !== null && closable(c.tab).others,
    run: (c) => c.tab && closeOtherTabs(c.tab),
  },
  "tab.closeRight": {
    enabled: (c) => c.tab !== null && closable(c.tab).right,
    run: (c) => c.tab && closeTabsToTheRight(c.tab),
  },
  "tab.closeAll": {
    enabled: () => useApp.getState().tabs.some((tab) => !tab.pinned),
    run: () => closeAllTabs(),
  },
  "tab.reopen": {
    enabled: () => useApp.getState().closedTabs.length > 0,
    run: () => useApp.getState().reopenTab(),
  },
  "tab.next": { enabled: () => useApp.getState().tabs.length > 1, run: () => stepTab(false) },
  "tab.previous": { enabled: () => useApp.getState().tabs.length > 1, run: () => stepTab(true) },
  "tab.moveRight": { enabled: (c) => tabOf(c) !== undefined, run: (c) => c.tab && moveTabBy(c.tab, 1) },
  "tab.moveLeft": { enabled: (c) => tabOf(c) !== undefined, run: (c) => c.tab && moveTabBy(c.tab, -1) },
  "results.maximize": {
    enabled: (c) => editorTab(c) !== undefined,
    label: (c) => (tabOf(c)?.resultsMaximized ? "app:commands.restoreEditor" : "app:commands.maximizeResults"),
    run: (c) => c.tab && toggleResultsMaximized(c.tab),
  },
  "view.splitRight": {
    enabled: (c) => tabOf(c) !== undefined,
    run: (c) => c.tab && splitTab(c.tab, "right"),
  },
  "view.splitDown": {
    enabled: (c) => tabOf(c) !== undefined,
    run: (c) => c.tab && splitTab(c.tab, "down"),
  },
  "view.closeSplit": {
    enabled: () => useApp.getState().layout.kind === "split",
    run: () => closeSplit(),
  },
  "view.focusOtherPane": {
    enabled: () => useApp.getState().layout.kind === "split",
    run: () => focusOtherPane(),
  },
  "results.detach": {
    enabled: (c) => editorTab(c) !== undefined,
    label: (c) => (tabOf(c)?.resultsWindow ? "app:detach.reattach" : "app:detach.results"),
    run: async (c) => {
      const tab = editorTab(c);
      if (!tab) return;
      if (tab.resultsWindow) {
        await reattachResults(tab.id);
        return;
      }
      const label = await detachResults(tab.id, tabLabel(tab));
      useApp.getState().updateTab(tab.id, { resultsWindow: label });
    },
  },
  "connection.openWindow": {
    // From the main window only: a connection's window shows that connection already.
    enabled: (c) => windowContext().kind === "main" && Boolean(tabOf(c)?.connectionId),
    run: (c) => {
      const id = tabOf(c)?.connectionId;
      const connection = useApp.getState().connections.find((item) => item.id === id);
      if (connection) void openConnectionWindow(connection);
    },
  },
  "window.close": {
    // A connection's window, as its close button: it asks about its own tabs first.
    enabled: () => windowContext().kind === "connection",
    run: () => answerWindowCloseRequest(),
  },
  "file.open": { run: () => openFileDialog() },
  "file.openPlan": { run: () => openSavedPlan() },
  "editor.explain": { enabled: runnableTab, run: editorCommand("explain") },
  "file.save": {
    enabled: (c) => editorTab(c) !== undefined,
    // A file tab saves its file; a console saves its query by name (ADR-036).
    label: (c) => (tabOf(c)?.file ? "app:tabs.saveFile" : "app:commands.saveQuery"),
    run: (c) => {
      const tab = editorTab(c);
      if (!tab) return;
      if (tab.file) {
        void saveTab(tab.id);
        return;
      }
      // Ctrl+S saves the query only from the editor: the grid's Ctrl+S saves its edits.
      const target = c.target instanceof Element ? c.target : null;
      if (c.source !== "key" || target?.closest(".editor-pane")) {
        window.dispatchEvent(new CustomEvent(SAVE_QUERY_EVENT, { detail: tab.id }));
      }
    },
  },
  "file.saveAs": {
    enabled: (c) => editorTab(c) !== undefined,
    run: (c) => c.tab && saveTabAs(c.tab),
  },
  "file.reload": {
    enabled: (c) => Boolean(editorTab(c)?.file),
    run: (c) => c.tab && reloadTab(c.tab),
  },
  "editor.run": { enabled: runnableTab, run: editorCommand("runCurrent") },
  "editor.runScript": { enabled: runnableTab, run: editorCommand("runScript") },
  "editor.runFromCursor": { enabled: runnableTab, run: editorCommand("runFromCursor") },
  "editor.runNewTab": { enabled: runnableTab, run: editorCommand("runNewTab") },
  // The active result's tab (SDB-378). What a run replaces, the pin and the name are the
  // core's; a re-run is a new run, through the guard like any other.
  "results.rerun": {
    enabled: (c) => Boolean(ended(activeResult(c)) && (activeResult(c)?.paging?.sql ?? activeResult(c)?.sql)) && idleEditorTab(c),
    run: editorCommand("resultRerun"),
  },
  "results.pin": {
    enabled: (c) => activeResult(c) !== undefined,
    label: (c) => (activeResult(c)?.pinned ? "editor:resultTabs.unpin" : "editor:resultTabs.pin"),
    run: editorCommand("resultPin"),
  },
  "results.rename": { enabled: (c) => activeResult(c) !== undefined, run: editorCommand("resultRename") },
  // Only a name the user gave can be taken off: the tab shows its comment's name or "Result N"
  // again, as the core answers (SDB-407).
  "results.resetName": { enabled: (c) => Boolean(activeResult(c)?.renamed), run: editorCommand("resultResetName") },
  "results.close": { enabled: (c) => Boolean(ended(activeResult(c))), run: editorCommand("resultClose") },
  "edits.undo": { enabled: (c) => editableTab(c) !== undefined, run: editsCommand("undo") },
  "edits.redo": { enabled: (c) => editableTab(c) !== undefined, run: editsCommand("redo") },
  // The active result's grid opens its "Go to row" (SDB-386).
  "results.goToRow": { enabled: hasRows, run: gridCommand({ kind: "goToRow" }) },
  // What the grid's menu does to its selection (SDB-441): the grid does it, with its selection;
  // without one it says to select cells first. The core writes every copy, as from the menu.
  "results.copySelection": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "tsv", header: false }) },
  "results.copyWithHeaders": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "tsv", header: true }) },
  "results.copyCsv": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "csv", header: true }) },
  "results.copyJson": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "json", header: false }) },
  "results.copyMarkdown": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "markdown", header: true }) },
  "results.copyInList": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "inList", header: false }) },
  "results.copyHtml": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "html", header: true }) },
  "results.copyInsert": { enabled: hasRows, run: gridCommand({ kind: "copy", format: "sqlInsert", header: false }) },
  "results.copySpecial": { enabled: hasRows, run: gridCommand({ kind: "copySpecial" }) },
  "results.copyUpdates": { enabled: hasRows, run: gridCommand({ kind: "copyUpdates" }) },
  "results.exportSelection": { enabled: (c) => hasRows(c) && Boolean(ended(gridResult(c))), run: gridCommand({ kind: "exportSelection" }) },
  "editor.variables": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("variables") },
  "editor.reconnect": {
    enabled: (c) => Boolean(editorTab(c)?.connectionId) && idleEditorTab(c),
    run: editorCommand("reconnect"),
  },
  "editor.sessionReadOnly": {
    enabled: (c) => {
      const tab = editorTab(c);
      return tab !== undefined && isConnected(tab) && !isRunning(tab);
    },
    label: (c) => {
      const tab = editorTab(c);
      return tab && sessionReadOnly(tab) ? "editor:session.readWrite" : "editor:session.readOnly";
    },
    run: editorCommand("sessionReadOnly"),
  },
  "connections.disconnectAll": {
    enabled: () => hasSessions(null),
    run: () => void disconnectAll(null),
  },
  // All but the active tab's connection.
  "connections.disconnectOthers": {
    enabled: (c) => Boolean(tabOf(c)?.connectionId) && hasSessions(tabConnection(c)),
    run: (c) => void disconnectAll(tabConnection(c)),
  },
  "editor.cancel": { enabled: (c) => isRunning(editorTab(c)), run: editorCommand("cancel") },
  "editor.format": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("format") },
  "editor.lineComment": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("lineComment") },
  "editor.blockComment": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("blockComment") },
  "editor.wordWrap": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("wordWrap") },
  "editor.fold": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("fold") },
  "editor.unfold": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("unfold") },
  "editor.matchingBracket": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("matchingBracket") },
  "editor.upperCase": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("upperCase") },
  "editor.lowerCase": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("lowerCase") },
  "editor.quotedList": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("quotedList") },
  "editor.selectStatement": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("selectStatement") },
  "editor.copyStatement": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("copyStatement") },
  // The clipboard, from the editor's menu (SDB-438): its keys stay the editor's own.
  "editor.cut": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("cut") },
  "editor.copy": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("copy") },
  "editor.paste": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("paste") },
  "editor.selectAll": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("selectAll") },
  "editor.selectOccurrences": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("selectOccurrences") },
  // The table under the cursor is looked up in the tab's connection: without one there is none.
  "editor.openObject": { enabled: (c) => connectedEditorTab(c), run: editorCommand("openObject") },
  "editor.revealInExplorer": { enabled: (c) => connectedEditorTab(c), run: editorCommand("revealInExplorer") },
  "editor.nextStatement": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("nextStatement") },
  "editor.previousStatement": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("previousStatement") },
  "editor.outline": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("outline") },
  "editor.insertSelect": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("insertSelect") },
  "editor.insertInsert": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("insertInsert") },
  "editor.insertUpdate": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("insertUpdate") },
  // A personal preference of every editor (SDB-389): it needs no editor to change.
  "editor.zoomIn": { run: () => void usePreferences.getState().zoomEditor(1) },
  "editor.zoomOut": { run: () => void usePreferences.getState().zoomEditor(-1) },
  "editor.zoomReset": { run: () => void usePreferences.getState().zoomEditor(0) },
  "view.explorer": { run: () => toggleSection("connections") },
  // Hides the side column, or brings back the panel it showed (SDB-451); the focus leaves a
  // hidden panel for the button that shows it again.
  "view.sidePanel": { run: () => toggleSidePanel() },
  "view.explorerFilter": {
    run: () => {
      // Focus the explorer's quick filter, showing the explorer if it is hidden.
      const app = useApp.getState();
      app.setSidebarSection("connections");
      if (app.explorerFilter === null) app.setExplorerFilter("");
      window.dispatchEvent(new Event(FOCUS_EXPLORER_FILTER_EVENT));
    },
  },
  "view.processes": { run: () => toggleSection("processes") },
  // The shortcut always shows the search and focuses its box; the activity button toggles it.
  "view.erDiagram": {
    enabled: (c) => diagramSource(c) !== undefined,
    run: (c) => openActiveDiagram(c),
  },
  "view.search": {
    run: (c) => {
      const app = useApp.getState();
      if (c.source !== "menu" || app.sidebarSection !== "search") {
        app.setSidebarSection("search");
        window.dispatchEvent(new Event(FOCUS_SEARCH_EVENT));
      } else app.setSidebarSection(null);
    },
  },
  "view.history": { run: () => useApp.getState().setHistoryOpen(!useApp.getState().historyOpen) },
  "view.savedQueries": { run: () => useApp.getState().setSnippetsOpen(!useApp.getState().snippetsOpen) },
  // The MCP page fills the window: leaving it shows the explorer again.
  "view.mcp": { run: () => useApp.getState().setSidebarSection((current) => (current === "mcp" ? "connections" : "mcp")) },
  "view.settings": { run: () => useApp.getState().openSettings() },
  "view.plugins": { run: () => useApp.getState().openSettings("plugins") },
  // The palette closes as it runs: a failure is shown where Settings › Updates offers the same.
  "help.copyDiagnostics": { run: () => copyDiagnostics().catch(() => useApp.getState().openSettings("updates")) },
  "help.openIssue": { run: () => openIssuePage().catch(() => useApp.getState().openSettings("updates")) },
};

export function commandEnabled(id: CommandId, context: CommandContext): boolean {
  return BINDINGS[id].enabled?.(context) ?? true;
}

/** The command's name now, as a translation key with its namespace. */
export function commandLabel(id: CommandId, context: CommandContext): string {
  return BINDINGS[id].label?.(context) ?? commandSpec(id).label;
}

/** Runs the command if it can run now; true if it ran. */
export function runCommand(id: CommandId, context: CommandContext): boolean {
  if (!commandEnabled(id, context)) return false;
  void Promise.resolve(BINDINGS[id].run(context)).catch((e: unknown) => console.error(`command ${id}`, e));
  return true;
}

/** The context of the palette and of shortcuts: the active tab, none behind Settings. */
export function activeContext(source: CommandContext["source"], target?: EventTarget | null): CommandContext {
  const app = useApp.getState();
  return { tab: app.settingsOpen ? null : app.activeTab, source, target };
}

export function isCommandId(id: string): id is CommandId {
  return (COMMANDS as readonly CommandSpec[]).some((command) => command.id === id);
}

// ------------------------------------------------------------ database objects in the palette

/** A connection or an object the explorer has read, as the palette lists it. */
export type PaletteObject =
  | { kind: "connection"; connection: ConnectionItem }
  | { kind: "relation"; connection: ConnectionKey; object: ObjectItem }
  | { kind: "schemaObject"; connection: ConnectionKey; object: SchemaObjectItem }
  /** A schema the explorer has read, with the object filter it was read with (ADR-025). */
  | { kind: "schema"; connection: ConnectionKey; schema: string; filter: NameFilter | null };

export interface PaletteObjectAction {
  id: string;
  /** A translation key with its namespace. */
  label: string;
  /** Rejects with what went wrong, shown in the palette, which stays open. */
  run(): Promise<void> | void;
  danger?: boolean;
}

function showInExplorer(target: { connection: ConnectionKey; schema?: string; name?: string }) {
  const app = useApp.getState();
  app.setSidebarSection("connections");
  app.revealInExplorer(target);
}

function newConsole(connection: ConnectionKey, schema: string) {
  const tab = useApp.getState().addTab(tabKey(connection));
  connectTabFromExplorer(tab, explorerSession(connection), explorerDatabase(connection, schema));
}

async function copyName(name: string) {
  await navigator.clipboard.writeText(name);
}

async function copyQualifiedName(connection: ConnectionKey, schema: string, name: string) {
  const names = await call(core.qualifiedNames(explorerSession(connection), [{ schema, names: [name] }]));
  await navigator.clipboard.writeText(names.join("\n"));
}

/** The structure of a table or view: its columns' details tab, as the explorer opens it. */
function openStructure(connection: ConnectionKey, object: ObjectItem) {
  const app = useApp.getState();
  const existing = app.tabs.some(
    (tab) =>
      tabOnConnection(tab.connectionId, connection) &&
      tab.tableDetails?.schema === object.schema &&
      tab.tableDetails.name === object.name &&
      tab.tableDetails.kind === "columns",
  );
  const tab = app.addTableDetailsTab(tabKey(connection), object.schema, object.name, "columns");
  if (!existing) connectTabFromExplorer(tab, explorerSession(connection), explorerDatabase(connection, object.schema));
}

/**
 * What the palette offers for a table or a view: the actions of its menu in the explorer, run by
 * the same functions (`explorerActions.ts`, SDB-92), and a few of its own (open structure, new
 * console, show in explorer). `scripts/commands.test.mjs` compares the two lists; the menu's
 * items that need its row (count, refresh, the filter, joins of the tables chosen with
 * Ctrl+click, the maintenance submenu) stay there. A collection, or a table of an engine whose
 * language is not SQL, only opens, copies its name and is shown in the explorer.
 */
function relationActions(connection: ConnectionKey, object: ObjectItem): PaletteObjectAction[] {
  const session = explorerSession(connection);
  const opening: PaletteObjectAction[] = [
    { id: "openData", label: "app:palette.actions.openData", run: () => openFound(connection, { kind: "relation", object }) },
  ];
  const own = (structure: boolean): PaletteObjectAction[] => [
    ...(structure ? [{ id: "openStructure", label: "app:palette.actions.openStructure", run: () => openStructure(connection, object) }] : []),
    { id: "newConsole", label: "app:palette.actions.newConsole", run: () => newConsole(connection, object.schema) },
  ];
  const names: PaletteObjectAction[] = [
    { id: "copyName", label: "shell:copyName", run: () => copyName(object.name) },
    { id: "copyQualifiedName", label: "shell:copyQualifiedName", run: () => copyQualifiedName(connection, object.schema, object.name) },
  ];
  const show: PaletteObjectAction = {
    id: "showInExplorer",
    label: "app:palette.actions.showInExplorer",
    run: () => showInExplorer({ connection, schema: object.schema, name: object.name }),
  };
  const sql = listedConnection(useApp.getState().connections, connection)?.sql ?? true;
  if (object.kind === "collection" || !sql) return [...opening, ...own(false), ...names, show];
  const table = isPlainTable(object);
  const bookmark = relationBookmark(connection, object);
  return [
    ...opening,
    ...own(true),
    { id: "openAsSql", label: "shell:sql.openAsSql", run: () => openRelationSql(connection, session, object, "preview", true) },
    { id: "export", label: "shell:exportTable", run: () => exportRelations(connection, session, [{ schema: object.schema, name: object.name }]) },
    ...(table ? [{ id: "import", label: "shell:importData", run: () => importIntoTable(connection, session, object) }] : []),
    { id: "copyToConnection", label: "shell:copyToConnection", run: () => copyToConnection(connection, session, object) },
    ...(table ? [{ id: "erDiagram", label: "app:palette.actions.erDiagram", run: () => openRelationDiagram(connection, session, object) }] : []),
    ...(object.kind === "view" ? [{ id: "editSource", label: "shell:editSource", run: () => editViewSource(connection, session, object) }] : []),
    ...names,
    { id: "bookmark", label: isBookmarked(bookmark) ? "shell:removeBookmark" : "shell:addBookmark", run: () => toggleBookmark(bookmark) },
    ...(table ? [{ id: "duplicate", label: "shell:duplicate.menu", run: () => duplicateTable(connection, session, object) }] : []),
    ...(canRenameRelation(object, knownObjectActions(session))
      ? [{ id: "rename", label: "shell:objectChange.renameMenu", run: () => renameRelation(connection, session, object) }]
      : []),
    { id: "drop", label: "shell:objectChange.dropMenu", danger: true, run: () => dropRelation(connection, session, object) },
    { id: "generateDdl", label: "shell:generateDdl", run: () => openRelationDdl(session, object) },
    ...relationTemplates(session, object).map((template) => ({
      id: `generate:${template}`,
      label: `shell:sql.${template}`,
      run: () => openRelationSql(connection, session, object, template),
    })),
    show,
  ];
}

/**
 * What the palette offers for a routine, sequence, trigger or other schema object: its menu's
 * actions in the explorer, run by the same functions (`explorerActions.ts`, SDB-441), and its own
 * (new console, show in explorer). `scripts/commands.test.mjs` compares the two lists; "Hide" and
 * "Show only" stay in the menu, since they change the connection's filter through its node.
 */
function schemaObjectActions(connection: ConnectionKey, object: SchemaObjectItem): PaletteObjectAction[] {
  const session = explorerSession(connection);
  const actions = knownObjectActions(session);
  const bookmark = schemaObjectBookmark(connection, object);
  return [
    { id: "openDefinition", label: "app:palette.actions.openDefinition", run: () => openObjectDefinition(connection, session, object) },
    ...(canEditRoutineSource(object, actions) ? [{ id: "editSource", label: "shell:editSource", run: () => editRoutineSource(connection, session, object) }] : []),
    ...(canRunRoutine(object, actions) ? [{ id: "runRoutine", label: "shell:runRoutine", run: () => openRoutineCall(connection, session, object, true) }] : []),
    ...(canGenerateCall(object, actions) ? [{ id: "generateCall", label: "shell:generateCall", run: () => openRoutineCall(connection, session, object) }] : []),
    { id: "newConsole", label: "app:palette.actions.newConsole", run: () => newConsole(connection, object.schema) },
    { id: "copyName", label: "shell:copyName", run: () => copyName(object.name) },
    { id: "copyQualifiedName", label: "shell:copyQualifiedName", run: () => copyQualifiedName(connection, object.schema, object.name) },
    { id: "bookmark", label: isBookmarked(bookmark) ? "shell:removeBookmark" : "shell:addBookmark", run: () => toggleBookmark(bookmark) },
    { id: "drop", label: "shell:objectChange.dropMenu", danger: true, run: () => dropSchemaObject(connection, session, object) },
    {
      id: "showInExplorer",
      label: "app:palette.actions.showInExplorer",
      run: () => showInExplorer({ connection, schema: object.schema, name: object.name }),
    },
  ];
}

/**
 * What the palette offers for a schema the explorer has read: its menu's actions (SDB-441), by
 * the same functions, and its own (new console, show in explorer). Refreshing, the filter and
 * the maintenance submenu stay in the menu: they act on the schema's row in the tree.
 */
function schemaActions(connection: ConnectionKey, schema: string, filter: NameFilter | null): PaletteObjectAction[] {
  const session = explorerSession(connection);
  const actions = knownObjectActions(session);
  const sql = listedConnection(useApp.getState().connections, connection)?.sql ?? true;
  const bookmark = schemaBookmark(connection, schema);
  const editor = actions?.activeSchema ? schemaEditorTab(connection) : null;
  return [
    { id: "newConsole", label: "app:palette.actions.newConsole", run: () => newConsole(connection, schema) },
    ...(editor ? [{ id: "setActiveSchema", label: "shell:setActiveSchema", run: () => setEditorSchema(editor, schema) }] : []),
    ...(sql
      ? [
          { id: "importNewTable", label: "shell:importNewTable", run: () => importIntoNewTable(connection, session, schema) },
          { id: "erDiagram", label: "shell:diagram.menuSchema", run: () => openSchemaDiagram(connection, session, schema, filter) },
          // The dialog says why when the engine cannot be compared (ADR-070).
          { id: "compareSchemas", label: "shell:compare.menu", run: () => compareSchemas(connection, schema) },
        ]
      : []),
    ...schemaCreateKinds(actions).map((kind) => ({
      id: `create:${kind}`,
      label: `shell:create.${kind}`,
      run: () => createInSchema(connection, session, schema, kind),
    })),
    { id: "copyName", label: "shell:copyName", run: () => copyName(schema) },
    { id: "copyQualifiedName", label: "shell:copyQualifiedName", run: () => copyQualifiedName(connection, "", schema) },
    { id: "bookmark", label: isBookmarked(bookmark) ? "shell:removeBookmark" : "shell:addBookmark", run: () => toggleBookmark(bookmark) },
    ...(actions?.dropSchemas ? [{ id: "drop", label: "shell:objectChange.dropMenu", danger: true, run: () => dropSchema(connection, session, schema) }] : []),
    { id: "showInExplorer", label: "app:palette.actions.showInExplorer", run: () => showInExplorer({ connection, schema }) },
  ];
}

/** What the palette offers for an object; the first one is what Enter does. */
export function objectActions(item: PaletteObject): PaletteObjectAction[] {
  switch (item.kind) {
    case "connection": {
      const { connection } = item;
      return [
        {
          id: "newQuery",
          label: "shell:newQuery",
          run: () => {
            newQueryOnConnection(keyOf(connection));
          },
        },
        { id: "showInExplorer", label: "app:palette.actions.showInExplorer", run: () => showInExplorer({ connection: keyOf(connection) }) },
        { id: "edit", label: "connections:edit", run: () => useApp.getState().setConnectionDialog({ kind: "edit", id: connection.id }) },
        { id: "duplicate", label: "connections:copies.duplicate", run: () => duplicateConnection(connection) },
        { id: "delete", label: "connections:delete", danger: true, run: () => useApp.getState().setDeletingConnection(connection) },
      ];
    }
    case "relation":
      return relationActions(item.connection, item.object);
    case "schemaObject":
      return schemaObjectActions(item.connection, item.object);
    case "schema":
      return schemaActions(item.connection, item.schema, item.filter);
  }
}
