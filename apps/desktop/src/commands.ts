// What each command of `commandRegistry.ts` does, when it can run and, for the few whose name
// follows the state (Pin / Unpin), its name now. Menus, the global shortcut handler and the
// command palette all run commands through `runCommand`, so they cannot do different things.
// Everything here is UI: anything that reaches a database goes through the same core commands
// as the buttons and menus it stands for.

import { call, commands as core, type ConnectionItem, type ObjectItem, type SchemaObjectItem } from "./api";
import { COMMANDS, commandSpec, type CommandId, type CommandSpec } from "./commandRegistry";
import { closeQueryTab, connectTabFromExplorer, explorerDatabase } from "./connect";
import { duplicateConnection } from "./connectionCopies";
import { openFileDialog, reloadTab, saveTab, saveTabAs } from "./sqlFiles";
import { EDITOR_COMMAND_EVENT, SAVE_QUERY_EVENT, useApp, type EditorCommand, type Tab } from "./store";
import {
  closable,
  closeAllTabs,
  closeOtherTabs,
  closeTabsToTheRight,
  duplicateTab,
  moveTabBy,
  startRename,
  toggleResultsMaximized,
  togglePinned,
} from "./tabActions";
import { FOCUS_EXPLORER_FILTER_EVENT } from "./components/Sidebar";
import { openFound } from "./components/OpenObjectDialog";

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
  return tab && !tab.table && !tab.tableDetails ? tab : undefined;
}

function editorCommand(command: EditorCommand): CommandBinding["run"] {
  return (context) => {
    const tab = editorTab(context);
    if (tab) window.dispatchEvent(new CustomEvent(EDITOR_COMMAND_EVENT, { detail: { tab: tab.id, command } }));
  };
}

function toggleSection(section: "connections" | "processes") {
  useApp.getState().setSidebarSection((current) => (current === section ? null : section));
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
  "object.openByName": { run: () => useApp.getState().setOpenObject({ connectionId: null }) },
  "connection.add": { run: () => useApp.getState().setConnectionDialog({ kind: "add" }) },
  "tab.new": { run: () => useApp.getState().addTab(null) },
  "tab.rename": {
    enabled: (c) => tabOf(c) !== undefined,
    run: (c) => c.tab && startRename(c.tab),
  },
  "tab.duplicate": {
    enabled: (c) => {
      const tab = tabOf(c);
      return tab !== undefined && !tab.tableDetails;
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
  "file.open": { run: () => openFileDialog() },
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
  "editor.run": { enabled: (c) => editorTab(c) !== undefined && !editorTab(c)?.run?.running, run: editorCommand("runCurrent") },
  "editor.runScript": { enabled: (c) => editorTab(c) !== undefined && !editorTab(c)?.run?.running, run: editorCommand("runScript") },
  "editor.cancel": { enabled: (c) => Boolean(editorTab(c)?.run?.running), run: editorCommand("cancel") },
  "editor.format": { enabled: (c) => editorTab(c) !== undefined, run: editorCommand("format") },
  "view.explorer": { run: () => toggleSection("connections") },
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
  "view.history": { run: () => useApp.getState().setHistoryOpen(!useApp.getState().historyOpen) },
  "view.savedQueries": { run: () => useApp.getState().setSnippetsOpen(!useApp.getState().snippetsOpen) },
  // The MCP page fills the window: leaving it shows the explorer again.
  "view.mcp": { run: () => useApp.getState().setSidebarSection((current) => (current === "mcp" ? "connections" : "mcp")) },
  "view.settings": { run: () => useApp.getState().openSettings() },
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
  | { kind: "relation"; connectionId: string; object: ObjectItem }
  | { kind: "schemaObject"; connectionId: string; object: SchemaObjectItem };

export interface PaletteObjectAction {
  id: string;
  /** A translation key with its namespace. */
  label: string;
  /** Rejects with what went wrong, shown in the palette, which stays open. */
  run(): Promise<void> | void;
  danger?: boolean;
}

function showInExplorer(target: { connectionId: string; schema?: string; name?: string }) {
  const app = useApp.getState();
  app.setSidebarSection("connections");
  app.revealInExplorer(target);
}

function newConsole(connectionId: string, schema: string) {
  const tab = useApp.getState().addTab(connectionId);
  connectTabFromExplorer(tab, `meta:${connectionId}`, explorerDatabase(connectionId, schema));
}

async function copyName(name: string) {
  await navigator.clipboard.writeText(name);
}

async function copyQualifiedName(connectionId: string, schema: string, name: string) {
  const names = await call(core.qualifiedNames(`meta:${connectionId}`, [{ schema, names: [name] }]));
  await navigator.clipboard.writeText(names.join("\n"));
}

/** The structure of a table or view: its columns' details tab, as the explorer opens it. */
function openStructure(connectionId: string, object: ObjectItem) {
  const app = useApp.getState();
  const existing = app.tabs.some(
    (tab) =>
      tab.connectionId === connectionId &&
      tab.tableDetails?.schema === object.schema &&
      tab.tableDetails.name === object.name &&
      tab.tableDetails.kind === "columns",
  );
  const tab = app.addTableDetailsTab(connectionId, object.schema, object.name, "columns");
  if (!existing) connectTabFromExplorer(tab, `meta:${connectionId}`, explorerDatabase(connectionId, object.schema));
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
            const app = useApp.getState();
            const tab = app.addTab(connection.id);
            if (app.explorerConnected.includes(connection.id)) connectTabFromExplorer(tab, `meta:${connection.id}`);
          },
        },
        { id: "showInExplorer", label: "app:palette.actions.showInExplorer", run: () => showInExplorer({ connectionId: connection.id }) },
        { id: "edit", label: "connections:edit", run: () => useApp.getState().setConnectionDialog({ kind: "edit", id: connection.id }) },
        { id: "duplicate", label: "connections:copies.duplicate", run: () => duplicateConnection(connection) },
        { id: "delete", label: "connections:delete", danger: true, run: () => useApp.getState().setDeletingConnection(connection) },
      ];
    }
    case "relation": {
      const { connectionId, object } = item;
      return [
        { id: "openData", label: "app:palette.actions.openData", run: () => openFound(connectionId, { kind: "relation", object }) },
        { id: "openStructure", label: "app:palette.actions.openStructure", run: () => openStructure(connectionId, object) },
        { id: "newConsole", label: "app:palette.actions.newConsole", run: () => newConsole(connectionId, object.schema) },
        { id: "copyName", label: "shell:copyName", run: () => copyName(object.name) },
        { id: "copyQualifiedName", label: "shell:copyQualifiedName", run: () => copyQualifiedName(connectionId, object.schema, object.name) },
        {
          id: "showInExplorer",
          label: "app:palette.actions.showInExplorer",
          run: () => showInExplorer({ connectionId, schema: object.schema, name: object.name }),
        },
      ];
    }
    case "schemaObject": {
      const { connectionId, object } = item;
      return [
        { id: "openDefinition", label: "app:palette.actions.openDefinition", run: () => openFound(connectionId, { kind: "schemaObject", object }) },
        { id: "newConsole", label: "app:palette.actions.newConsole", run: () => newConsole(connectionId, object.schema) },
        { id: "copyName", label: "shell:copyName", run: () => copyName(object.name) },
        { id: "copyQualifiedName", label: "shell:copyQualifiedName", run: () => copyQualifiedName(connectionId, object.schema, object.name) },
        {
          id: "showInExplorer",
          label: "app:palette.actions.showInExplorer",
          run: () => showInExplorer({ connectionId, schema: object.schema, name: object.name }),
        },
      ];
    }
  }
}
