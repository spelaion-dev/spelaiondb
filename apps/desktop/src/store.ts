import type { EditorActionId } from "./editorActions";
// UI state. Result data does NOT live here (AGENTS.md): only its metadata; rows are
// requested from the core in windows.

import { create } from "zustand";
import { sameTag } from "./connectionInfo";
import type { BookmarkItem, ColumnFilterItem, ColumnItem, ConnectionItem, ErrorAnswerItem, FieldChangeItem, FileFormatItem, InteractionKind, OnErrorItem, RestoreNoteItem, RiskKind, ScriptSummaryItem, SkippedSetupItem, TransactionStatusItem, WorkspaceInfo } from "./api";
import type { DiagramScopeItem_Deserialize, RestoredTabItem } from "./bindings";

export interface ResultMeta {
  id: number;
  statement: number;
  columns: ColumnItem[];
  rows: number;
  complete: boolean;
  /** Column the core sorted the rows by (`undefined`: the server's order). */
  sort?: SortState;
  /** Text the core filtered the rows by; `rows` then counts what is left of `total`. */
  filter?: string;
  /** Column conditions the core filtered the rows by (all must hold). */
  columnFilters?: ColumnFilterItem[];
  total?: number;
  /** The statement that produced it, as it ran: what an export runs again (ADR-034). */
  sql?: string;
  /** Pages fetched from one safe SELECT, appended to this result. */
  paging?: { sql: string; pageSize: number; hasMore: boolean; loading: boolean; total?: number };
}

export interface SortState {
  column: number;
  descending: boolean;
}

export interface Translation {
  key: string;
  values?: Record<string, string | number>;
}

export interface Message {
  kind: "info" | "notice" | "error";
  text: string;
  translation?: Translation;
}

export interface RunState {
  running: boolean;
  results: ResultMeta[];
  activeResult: number | null;
  messages: Message[];
  /** Error with its position in the editor text (not in the script that was sent). */
  error: {
    message: string;
    offset: number | null;
    translation?: Translation;
    diagnostic?: {
      message: string;
      detail?: string | null;
      hint?: string | null;
      code?: string | null;
      cancelled: boolean;
    };
  } | null;
  startedAt: number;
  elapsedMs: number | null;
  /** What the core counted when the run ended (ADR-035). */
  summary?: ScriptSummaryItem;
}

export type TabStatus = "disconnected" | "connecting" | "connected";
export type TableDetailKind = "columns" | "constraints" | "foreignKeys" | "references" | "indexes" | "triggers";

/**
 * A tab, in the order the tab bar shows it (pinned tabs first). The fields kept when a tab is
 * closed and reopened are those of {@link TabSnapshot}; everything else (session, results,
 * transaction) belongs to this run of the app only.
 */
export interface Tab {
  id: string;
  number: number;
  /** Name given by the user (rename); without it the tab shows its default name. */
  title?: string;
  /** Pinned tabs sort first and are left open by "close others / to the right / all". */
  pinned?: boolean;
  /** The results panel fills the tab, hiding the SQL editor (UI only, never kept). */
  resultsMaximized?: boolean;
  connectionId: string | null;
  status: TabStatus;
  /** Why its session was lost (ADR-028): nothing runs until the user reconnects. */
  lost?: string;
  /** Its session was closed by its connection type after this many seconds idle (ADR-029). */
  idleClosed?: number;
  /** Its session was lost with a transaction open: the server rolled it back (ADR-031). */
  lostTransaction?: boolean;
  /** Its transaction as the core last told it (ADR-031); the core decides, this only shows. */
  transaction?: TransactionStatusItem;
  serverVersion: string | null;
  sql: string;
  run: RunState | null;
  /** Name it was saved under (or opened from), offered again by Ctrl+S. */
  snippetName?: string;
  /** A table data tab (pages of one table) instead of an SQL editor. */
  table?: TableView;
  tableDetails?: { schema: string; name: string; kind: TableDetailKind };
  /** An ER diagram (ADR-046) of a schema, chosen tables or one table and its neighbours, read
   *  from the catalog on this tab's own session. */
  diagram?: DiagramScopeItem_Deserialize;
  /** A local saved plan, opened without executing SQL. */
  plan?: { path: string };
  /** Pending row changes are kept in the mounted editor until saved or discarded. */
  pendingEdits?: boolean;
  savingEdits?: boolean;
  /** This tab's own run options (ADR-035); `null` fields take the defaults. */
  runOptions?: { onError: OnErrorItem | null; timeoutSeconds: number | null };
  /** Bumped when `sql` is replaced from outside the editor (a file reloaded from disk). */
  textRevision?: number;
  /** The `.sql` file this tab shows (ADR-036); a console has none. */
  file?: TabFile;
  /** What changed while the app was closed (a connection or file gone), shown until dismissed. */
  restoreNotes?: RestoreNoteItem[];
  /** Session setup statements that failed and were skipped when the session opened (ADR-037),
   *  shown until dismissed or the next connection. */
  setupSkipped?: SkippedSetupItem[];
}

/** The `.sql` file behind a file tab. The core reads and writes it (ADR-036). */
export interface TabFile {
  path: string;
  /** Its name, the tab's default title. */
  name: string;
  /** Encoding and line ending, kept when it is saved. */
  format: FileFormatItem;
  /** What the core last read or saved, sent back with the next save to notice a change on disk. */
  hash: string | null;
  /** The text as last read or saved; the tab is unsaved while its text differs. `null`: not
   *  known (restored or reopened with unsaved changes). */
  savedText: string | null;
}

/** What a table data tab shows: the core writes and checks the page's SELECT. */
export interface TableView {
  schema: string;
  name: string;
  /** Typed after WHERE / ORDER BY. */
  filter: string;
  orderBy: string;
  /** Set by a header click (shown in the grid); typing an ORDER BY clears it. */
  sort?: SortState;
  pageSize: number;
  /** From 0. */
  page: number;
  /** Rows the filter keeps, once counted ("Count"). */
  total?: number;
}

/** Rows per page a table data tab starts with. */
export const DEFAULT_PAGE_SIZE = 500;

/**
 * What a closed SQL editor tab keeps for Ctrl+Shift+T: plain data, ready to be serialised.
 * Never its session, results or transaction: a reopened tab starts disconnected and runs
 * nothing on its own (AGENTS.md).
 */
export interface TabSnapshot {
  connectionId: string | null;
  sql: string;
  title?: string;
  pinned?: boolean;
  snippetName?: string;
  runOptions?: Tab["runOptions"];
  /** A file tab comes back as it was closed, unsaved changes included (ADR-036). */
  file?: TabFile;
}

/** A closed tab and where it was, to reopen it in the same place. */
export interface ClosedTab {
  snapshot: TabSnapshot;
  index: number;
}

/** The durable part of a tab. Only SQL editor tabs have one: table data and metadata tabs
 *  are reopened from the explorer. */
export function snapshotOf(tab: Tab): TabSnapshot | null {
  if (tab.table || tab.tableDetails || tab.plan || tab.diagram) return null;
  return {
    connectionId: tab.connectionId,
    sql: tab.sql,
    ...(tab.title ? { title: tab.title } : {}),
    ...(tab.pinned ? { pinned: true } : {}),
    ...(tab.snippetName ? { snippetName: tab.snippetName } : {}),
    ...(tab.runOptions ? { runOptions: tab.runOptions } : {}),
    ...(tab.file ? { file: tab.file } : {}),
  };
}

/** Pinned tabs first, each group keeping its order. */
function pinnedFirst(tabs: Tab[]): Tab[] {
  return [...tabs.filter((t) => t.pinned), ...tabs.filter((t) => !t.pinned)];
}

/** After closing, the tab used most recently among those left (or the last one). */
function nextActive(tabs: Tab[], recent: string[], current: string | null): string | null {
  if (current && tabs.some((t) => t.id === current)) return current;
  return recent.find((id) => tabs.some((t) => t.id === id)) ?? tabs[tabs.length - 1]?.id ?? null;
}

let tabCounter = 0;

export type ConnectionDialog =
  | { kind: "add" }
  /** `notice`: shown above the form, such as "this is the copy just made". */
  | { kind: "edit"; id: string; notice?: string };

/** An object to show in the explorer: its connection expands (if it is connected there), then
 *  its schema and the folder holding the object, which is scrolled into view. */
export interface ExplorerReveal {
  connectionId: string;
  schema?: string;
  name?: string;
  /** Asked by the user (a bookmark): the connection connects if it is not yet. */
  connect?: boolean;
  /** Tells one request from the next for the same object. */
  nonce: number;
}

/** Window event asking the editor tab whose id is in `detail` to save its query (Ctrl+S). */
export const SAVE_QUERY_EVENT = "spelaion:save-query";

/** What the command palette asks of an editor tab: the same as its action column's buttons. */
export type EditorCommand = "runCurrent" | "runScript" | "explain" | "format" | "cancel" | EditorActionId;

/** Window event asking the editor tab `detail.tab` to do `detail.command` (SDB-18). */
export const EDITOR_COMMAND_EVENT = "spelaion:editor-command";
/** Undo or redo of a tab's pending table edits, from the command palette (`{ tab, command }`). */
export const EDITS_COMMAND_EVENT = "spelaion:edits-command";

/** The panel next to the activity bar: the explorer, the process monitor, the MCP page or none. */
export type SidebarSection = "connections" | "processes" | "search" | "mcp" | null;

/** How many closed tabs Ctrl+Shift+T can bring back. */
const MAX_CLOSED_TABS = 20;

/** `activeTab` value of the settings tab, which lives beside the query tabs. */

interface AppStore {
  workspace: WorkspaceInfo | null;
  connections: ConnectionItem[];
  tabs: Tab[];
  activeTab: string | null;
  historyOpen: boolean;
  snippetsOpen: boolean;
  snippetsRevision: number;
  settingsOpen: boolean;
  /** The add/edit connection wizard, if open. */
  connectionDialog: ConnectionDialog | null;
  /** Connection waiting for the user to confirm its deletion. */
  deletingConnection: ConnectionItem | null;
  /** The connection whose "Move to folder…" dialog is open. */
  movingConnection: ConnectionItem | null;
  historyRevision: number;
  historySaving: string[];
  workspaceChanging: boolean;
  /** Text of the explorer's quick filter; null while its box is closed. */
  explorerFilter: string | null;
  /** Tags the quick filter asks of connections (all of them); empty while its box is closed. */
  explorerTags: string[];
  /** Connections expanded in the explorer: the quick filter keeps them shown. */
  openConnections: string[];
  /** Connections the explorer has a session on (they were opened at least once). */
  explorerConnected: string[];
  /** Tables and views chosen with Ctrl+click, in the order they were chosen (one session). */
  explorerSelection: { session: string; schema: string; name: string }[];
  /** What the explorer is asked to show; the node that shows it clears it. */
  explorerReveal: ExplorerReveal | null;
  /** The explorer's bookmarks (ADR-026), as the core lists them. */
  bookmarks: BookmarkItem[];
  /** "Open object by name" is open, on this connection first (or the active tab's). */
  openObject: { connectionId: string | null } | null;
  /** The command palette (Ctrl+Shift+P) is open. */
  paletteOpen: boolean;
  sidebarSection: SidebarSection;
  setWorkspace(info: WorkspaceInfo): void;
  setConnections(list: ConnectionItem[]): void;
  addTab(connectionId: string | null, sql?: string): string;
  /** Opens a table data tab on a table (its first page loads when it appears), filtered by
   *  `filter` when given (a followed foreign key's, written by the core). */
  addTableTab(connectionId: string, schema: string, name: string, filter?: string): string;
  addTableDetailsTab(connectionId: string, schema: string, name: string, kind: TableDetailKind): string;
  /** Opens an ER diagram tab, or shows the one already open on the same scope. The second value
   *  says whether the tab is new (and needs connecting). */
  addDiagramTab(connectionId: string, scope: DiagramScopeItem_Deserialize): [string, boolean];
  /** Takes the tabs out of the bar (their sessions are closed by `connect.ts`). */
  closeTabs(ids: string[]): void;
  /** Opens again the last closed query tab (its connection, SQL and name, not its results). */
  reopenTab(): void;
  /** Closed query tabs, the last one at the end. */
  closedTabs: ClosedTab[];
  /** Tab ids, the one used most recently first (Ctrl+Tab). */
  recentTabs: string[];
  /** The tab whose name is being edited in the tab bar. */
  renamingTab: string | null;
  setRenamingTab(id: string | null): void;
  /** An empty or blank title gives the tab back its default name. */
  renameTab(id: string, title: string): void;
  /** A copy next to it: same connection, SQL and options, without session or results. */
  duplicateTab(id: string): string | null;
  setPinned(id: string, pinned: boolean): void;
  /** Moves a tab to `index`, kept inside its group (pinned or not). */
  moveTab(id: string, index: number): void;
  setActiveTab(id: string): void;
  /** Replaces every tab with the ones the core restored for the open folder (ADR-036): all
   *  disconnected, with their text and nothing to run. Closed tabs are forgotten. */
  restoreTabs(restored: RestoredTabItem[], active: number | null): void;
  updateTab(id: string, patch: Partial<Tab>): void;
  updateRun(id: string, update: (run: RunState) => RunState): void;
  setHistoryOpen(open: boolean): void;
  /** The saved queries panel takes the place of the history panel, and vice versa. */
  setSnippetsOpen(open: boolean): void;
  refreshSnippets(): void;
  openSettings(): void;
  closeSettings(): void;
  setConnectionDialog(dialog: ConnectionDialog | null): void;
  setDeletingConnection(connection: ConnectionItem | null): void;
  setMovingConnection(connection: ConnectionItem | null): void;
  refreshHistory(): void;
  setHistorySaving(connection: string, saving: boolean): void;
  setWorkspaceChanging(changing: boolean): void;
  clearTabConnections(): void;
  setResultsMaximized(id: string, maximized: boolean): void;
  setExplorerFilter(filter: string | null): void;
  /** Adds the tag to the quick filter (opening it) or takes it out. */
  toggleExplorerTag(tag: string): void;
  setConnectionOpen(connection: string, open: boolean): void;
  setExplorerConnected(connection: string, connected: boolean): void;
  /** Ctrl+click: adds or removes one; choosing in another session starts again. */
  toggleExplorerSelection(item: { session: string; schema: string; name: string }): void;
  clearExplorerSelection(): void;
  revealInExplorer(target: Omit<ExplorerReveal, "nonce">): void;
  /** The explorer showed it (or cannot): later renders do not scroll to it again. */
  revealDone(nonce: number): void;
  setOpenObject(open: { connectionId: string | null } | null): void;
  setPaletteOpen(open: boolean): void;
  setSidebarSection(section: SidebarSection | ((current: SidebarSection) => SidebarSection)): void;
  setBookmarks(bookmarks: BookmarkItem[]): void;
}

export const useApp = create<AppStore>((set) => ({
  workspace: null,
  connections: [],
  tabs: [],
  activeTab: null,
  historyOpen: false,
  snippetsOpen: false,
  snippetsRevision: 0,
  settingsOpen: false,
  connectionDialog: null,
  deletingConnection: null,
  movingConnection: null,
  historyRevision: 0,
  historySaving: [],
  workspaceChanging: false,
  explorerFilter: null,
  explorerTags: [],
  openConnections: [],
  explorerConnected: [],
  explorerSelection: [],
  explorerReveal: null,
  openObject: null,
  paletteOpen: false,
  sidebarSection: "connections",
  bookmarks: [],
  setWorkspace: (workspace) => set({ workspace }),
  setConnections: (connections) => set({ connections }),
  addTab: (connectionId, sql = "") => {
    tabCounter += 1;
    const id = `tab-${tabCounter}`;
    const tab: Tab = {
      id,
      number: tabCounter,
      connectionId,
      status: "disconnected",
      serverVersion: null,
      sql,
      run: null,
    };
    set((s) => ({ tabs: [...s.tabs, tab], activeTab: id, recentTabs: [id, ...s.recentTabs] }));
    return id;
  },
  addTableTab: (connectionId, schema, name, filter = ""): string => {
    const app: AppStore = useApp.getState();
    const id: string = app.addTab(connectionId);
    app.updateTab(id, { table: { schema, name, filter, orderBy: "", pageSize: DEFAULT_PAGE_SIZE, page: 0 } });
    return id;
  },
  addTableDetailsTab: (connectionId, schema, name, kind): string => {
    const app = useApp.getState();
    const existing = app.tabs.find((tab) => tab.connectionId === connectionId && tab.tableDetails?.schema === schema && tab.tableDetails.name === name && tab.tableDetails.kind === kind);
    if (existing) { app.setActiveTab(existing.id); return existing.id; }
    const id = app.addTab(connectionId);
    app.updateTab(id, { tableDetails: { schema, name, kind } });
    return id;
  },
  addDiagramTab: (connectionId, scope): [string, boolean] => {
    const app = useApp.getState();
    const key = JSON.stringify(scope);
    const existing = app.tabs.find((tab) => tab.connectionId === connectionId && tab.diagram && JSON.stringify(tab.diagram) === key);
    if (existing) { app.setActiveTab(existing.id); return [existing.id, false]; }
    const id = app.addTab(connectionId);
    app.updateTab(id, { diagram: scope });
    return [id, true];
  },
  closedTabs: [],
  recentTabs: [],
  renamingTab: null,
  closeTabs: (ids) =>
    set((s) => {
      const closing = new Set(ids);
      const closed: ClosedTab[] = [];
      s.tabs.forEach((tab, index) => {
        const snapshot = closing.has(tab.id) ? snapshotOf(tab) : null;
        if (snapshot) closed.push({ snapshot, index });
      });
      const tabs = s.tabs.filter((t) => !closing.has(t.id));
      const recentTabs = s.recentTabs.filter((id) => !closing.has(id));
      return {
        tabs,
        recentTabs,
        // The leftmost of a batch comes back first, so each one lands at its old index.
        closedTabs: [...s.closedTabs, ...closed.reverse()].slice(-MAX_CLOSED_TABS),
        activeTab: nextActive(tabs, recentTabs, s.activeTab && closing.has(s.activeTab) ? null : s.activeTab),
        renamingTab: s.renamingTab && closing.has(s.renamingTab) ? null : s.renamingTab,
      };
    }),
  reopenTab: () => {
    const s: AppStore = useApp.getState();
    const last = s.closedTabs[s.closedTabs.length - 1];
    if (!last) return;
    set({ closedTabs: s.closedTabs.slice(0, -1) });
    const { snapshot } = last;
    // The connection may have been deleted since.
    const connectionId = snapshot.connectionId && s.connections.some((c) => c.id === snapshot.connectionId) ? snapshot.connectionId : null;
    const id: string = s.addTab(connectionId, snapshot.sql);
    set((now) => {
      const tab = now.tabs.find((t) => t.id === id);
      if (!tab) return now;
      const restored: Tab = { ...tab, title: snapshot.title, pinned: snapshot.pinned, snippetName: snapshot.snippetName, runOptions: snapshot.runOptions, file: snapshot.file };
      const others = now.tabs.filter((t) => t.id !== id);
      others.splice(Math.min(last.index, others.length), 0, restored);
      return { tabs: pinnedFirst(others) };
    });
  },
  setRenamingTab: (renamingTab) => set({ renamingTab }),
  renameTab: (id, title) => {
    const trimmed = title.trim();
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, title: trimmed === "" ? undefined : trimmed } : t)) }));
  },
  duplicateTab: (id): string | null => {
    const s: AppStore = useApp.getState();
    const source = s.tabs.find((t) => t.id === id);
    // A metadata tab is opened once per object (addTableDetailsTab finds the open one).
    if (!source || source.tableDetails || source.diagram) return null;
    const copy: string = s.addTab(source.connectionId, source.sql);
    set((now) => {
      const tab = now.tabs.find((t) => t.id === copy);
      if (!tab) return now;
      const duplicate: Tab = {
        ...tab,
        title: source.title,
        runOptions: source.runOptions,
        table: source.table ? { ...source.table, page: 0, total: undefined } : undefined,
      };
      const others = now.tabs.filter((t) => t.id !== copy);
      others.splice(others.findIndex((t) => t.id === id) + 1, 0, duplicate);
      return { tabs: pinnedFirst(others) };
    });
    return copy;
  },
  setPinned: (id, pinned) =>
    set((s) => ({ tabs: pinnedFirst(s.tabs.map((t) => (t.id === id ? { ...t, pinned: pinned || undefined } : t))) })),
  moveTab: (id, index) =>
    set((s) => {
      const tab = s.tabs.find((t) => t.id === id);
      if (!tab) return s;
      const pinnedCount = s.tabs.filter((t) => t.pinned).length;
      // Pinned tabs move among the pinned ones, the others after them.
      const [min, max] = tab.pinned ? [0, pinnedCount - 1] : [pinnedCount, s.tabs.length - 1];
      const target = Math.max(min, Math.min(max, index));
      const others = s.tabs.filter((t) => t.id !== id);
      others.splice(target, 0, tab);
      return { tabs: others };
    }),
  setActiveTab: (activeTab) => set((s) => ({ activeTab, recentTabs: [activeTab, ...s.recentTabs.filter((id) => id !== activeTab)] })),
  restoreTabs: (restored, active) => {
    const tabs: Tab[] = restored.map(({ tab: kept, notes }) => {
      tabCounter += 1;
      const file = kept.file ?? null;
      return {
        id: `tab-${tabCounter}`,
        number: tabCounter,
        title: kept.title ?? undefined,
        pinned: kept.pinned || undefined,
        connectionId: kept.connection ?? null,
        status: "disconnected",
        serverVersion: null,
        sql: kept.text,
        run: null,
        snippetName: kept.snippetName ?? undefined,
        runOptions: kept.onError || kept.timeoutSeconds ? { onError: kept.onError ?? null, timeoutSeconds: kept.timeoutSeconds ?? null } : undefined,
        file: file
          ? {
              path: file.path,
              name: file.path.split(/[\\/]/).pop() ?? file.path,
              format: file.format,
              hash: file.hash ?? null,
              savedText: file.modified ? null : kept.text,
            }
          : undefined,
        restoreNotes: notes.length > 0 ? notes : undefined,
      };
    });
    const ordered = pinnedFirst(tabs);
    const activeTab = (active !== null ? tabs[active]?.id : undefined) ?? ordered[ordered.length - 1]?.id ?? null;
    set({ tabs: ordered, activeTab, recentTabs: activeTab ? [activeTab] : [], closedTabs: [], renamingTab: null });
  },
  updateTab: (id, patch) => set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, ...patch } : t)) })),
  updateRun: (id, update) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.run ? { ...t, run: update(t.run) } : t)) })),
  setHistoryOpen: (historyOpen) => set(historyOpen ? { historyOpen, snippetsOpen: false } : { historyOpen }),
  setSnippetsOpen: (snippetsOpen) => set(snippetsOpen ? { snippetsOpen, historyOpen: false } : { snippetsOpen }),
  refreshSnippets: () => set((s) => ({ snippetsRevision: s.snippetsRevision + 1 })),
  setConnectionDialog: (connectionDialog) => set({ connectionDialog }),
  setDeletingConnection: (deletingConnection) => set({ deletingConnection }),
  setMovingConnection: (movingConnection) => set({ movingConnection }),
  openSettings: () => set({ settingsOpen: true }),
  closeSettings: () => set({ settingsOpen: false }),
  refreshHistory: () => set((s) => ({ historyRevision: s.historyRevision + 1 })),
  setHistorySaving: (connection, saving) => set((s) => ({
    historySaving: saving ? [...new Set([...s.historySaving, connection])] : s.historySaving.filter((id) => id !== connection),
  })),
  setWorkspaceChanging: (workspaceChanging) => set({ workspaceChanging }),
  setExplorerFilter: (explorerFilter) =>
    set((s) => ({ explorerFilter, explorerTags: explorerFilter === null ? [] : s.explorerTags })),
  toggleExplorerTag: (tag) =>
    set((s) => ({
      explorerFilter: s.explorerFilter ?? "",
      explorerTags: s.explorerTags.some((other) => sameTag(other, tag))
        ? s.explorerTags.filter((other) => !sameTag(other, tag))
        : [...s.explorerTags, tag],
    })),
  toggleExplorerSelection: (item) => set((s) => {
    const same = (o: typeof item) => o.session === item.session && o.schema === item.schema && o.name === item.name;
    const mine = s.explorerSelection.filter((o) => o.session === item.session);
    return { explorerSelection: mine.some(same) ? mine.filter((o) => !same(o)) : [...mine, item] };
  }),
  clearExplorerSelection: () => set((s) => (s.explorerSelection.length === 0 ? s : { explorerSelection: [] })),
  revealInExplorer: (target) => set((s) => ({ explorerReveal: { ...target, nonce: (s.explorerReveal?.nonce ?? 0) + 1 } })),
  revealDone: (nonce) => set((s) => (s.explorerReveal?.nonce === nonce ? { explorerReveal: null } : s)),
  setOpenObject: (openObject) => set({ openObject }),
  setPaletteOpen: (paletteOpen) => set({ paletteOpen }),
  setSidebarSection: (section) =>
    set((s) => ({ sidebarSection: typeof section === "function" ? section(s.sidebarSection) : section })),
  setBookmarks: (bookmarks) => set({ bookmarks }),
  setExplorerConnected: (connection, connected) => set((s) => ({
    explorerConnected: connected ? [...new Set([...s.explorerConnected, connection])] : s.explorerConnected.filter((id) => id !== connection),
  })),
  setConnectionOpen: (connection, open) => set((s) => ({
    openConnections: open ? [...new Set([...s.openConnections, connection])] : s.openConnections.filter((id) => id !== connection),
  })),
  clearTabConnections: () => set((s) => ({ tabs: s.tabs.map((tab) => ({ ...tab, connectionId: null, status: "disconnected", serverVersion: null, run: null })) })),
  setResultsMaximized: (id, resultsMaximized) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, resultsMaximized: resultsMaximized || undefined } : t)) })),
}));

// ------------------------------------------------------------------- dialogs

export interface PasswordRequest {
  connectionName: string;
  canRemember: boolean;
  error: string | null;
  resolve(answer: { password: string; remember: boolean } | null): void;
}

export interface TrustRequest {
  workspace: string | null;
  resolve(accept: boolean): void;
}

/** Values for the user's personal `${user.…}` variables (ADR-022). */
export interface VariablesRequest {
  names: string[];
  resolve(values: Record<string, string> | null): void;
}

/** A repository connection or tunnel changed since the repository was trusted (ADR-022). */
export interface ChangeRequest {
  item: string;
  isNew: boolean;
  fields: FieldChangeItem[];
  resolve(accept: boolean): void;
}

/** A line of text, such as the name to save a query under. */
export interface TextRequest {
  title: string;
  label: string;
  initial: string;
  confirm: string;
  resolve(text: string | null): void;
}

/** A question with a few answers (save, don't save, cancel…); `null` when dismissed. */
export interface ChoiceRequest {
  title: string;
  message: string;
  choices: { id: string; label: string; primary?: boolean; danger?: boolean }[];
  resolve(choice: string | null): void;
}

/** Values for the `:name` placeholders of a script. */
export interface ParamsRequest {
  names: string[];
  /** Values used last time in this tab, to start from. */
  previous: Record<string, string>;
  resolve(values: Record<string, string> | null): void;
}

/** Tabs with an open transaction about to be closed, disconnected or quit (ADR-031), and tabs
 *  about to be closed while their query runs: asked once for all of them. */
export interface OpenTransactionsRequest {
  /** Tabs whose transaction the server rolls back. */
  tabs: string[];
  /** Tabs whose running query is cancelled. */
  running: string[];
  action: "close" | "disconnect" | "quit";
  resolve(proceed: boolean): void;
}

/** A run paused after an error (ADR-035): the core waits for the answer to its ticket. */
export interface ScriptErrorRequest {
  tab: string;
  /** 1-based. */
  statement: number;
  message: string;
  remaining: number;
  resolve(answer: ErrorAnswerItem): void;
}

/** Statements the core wants confirmed before they run. */
export interface RiskRequest {
  connection: string;
  statements: { sql: string; risk: RiskKind }[];
  resolve(run: boolean): void;
}

export interface InteractionItem {
  id: number;
  request: InteractionKind;
}

interface DialogStore {
  password: PasswordRequest | null;
  trust: TrustRequest | null;
  change: ChangeRequest | null;
  variables: VariablesRequest | null;
  risk: RiskRequest | null;
  scriptError: ScriptErrorRequest | null;
  openTransactions: OpenTransactionsRequest | null;
  params: ParamsRequest | null;
  text: TextRequest | null;
  choice: ChoiceRequest | null;
  interactions: InteractionItem[];
  askOpenTransactions(tabs: string[], action: OpenTransactionsRequest["action"], running?: string[]): Promise<boolean>;
  askText(request: Omit<TextRequest, "resolve">): Promise<string | null>;
  askChoice(request: Omit<ChoiceRequest, "resolve">): Promise<string | null>;
  askParams(names: string[], previous: Record<string, string>): Promise<Record<string, string> | null>;
  askRisks(connection: string, statements: RiskRequest["statements"]): Promise<boolean>;
  askScriptError(request: Omit<ScriptErrorRequest, "resolve">): Promise<ErrorAnswerItem>;
  askPassword(connectionName: string, canRemember: boolean, error: string | null): Promise<{ password: string; remember: boolean } | null>;
  askTrust(workspace: string | null): Promise<boolean>;
  askChange(change: Omit<ChangeRequest, "resolve">): Promise<boolean>;
  askVariables(names: string[]): Promise<Record<string, string> | null>;
  pushInteraction(item: InteractionItem): void;
  popInteraction(id: number): void;
}

export const useDialogs = create<DialogStore>((set) => ({
  password: null,
  trust: null,
  change: null,
  variables: null,
  risk: null,
  scriptError: null,
  openTransactions: null,
  params: null,
  text: null,
  choice: null,
  interactions: [],
  askText: (request) =>
    new Promise((resolve) =>
      set({
        text: {
          ...request,
          resolve: (text) => {
            set({ text: null });
            resolve(text);
          },
        },
      }),
    ),
  askChoice: (request) =>
    new Promise((resolve) =>
      set({
        choice: {
          ...request,
          resolve: (choice) => {
            set({ choice: null });
            resolve(choice);
          },
        },
      }),
    ),
  askParams: (names, previous) =>
    new Promise((resolve) =>
      set({
        params: {
          names,
          previous,
          resolve: (values) => {
            set({ params: null });
            resolve(values);
          },
        },
      }),
    ),
  askScriptError: (request) =>
    new Promise((resolve) =>
      set({
        scriptError: {
          ...request,
          resolve: (answer) => {
            set({ scriptError: null });
            resolve(answer);
          },
        },
      }),
    ),
  askRisks: (connection, statements) =>
    new Promise((resolve) =>
      set({
        risk: {
          connection,
          statements,
          resolve: (run) => {
            set({ risk: null });
            resolve(run);
          },
        },
      }),
    ),
  askPassword: (connectionName, canRemember, error) =>
    new Promise((resolve) =>
      set({
        password: {
          connectionName,
          canRemember,
          error,
          resolve: (answer) => {
            set({ password: null });
            resolve(answer);
          },
        },
      }),
    ),
  askTrust: (workspace) =>
    new Promise((resolve) =>
      set({
        trust: {
          workspace,
          resolve: (accept) => {
            set({ trust: null });
            resolve(accept);
          },
        },
      }),
    ),
  askChange: (change) =>
    new Promise((resolve) =>
      set({
        change: {
          ...change,
          resolve: (accept) => {
            set({ change: null });
            resolve(accept);
          },
        },
      }),
    ),
  askVariables: (names) =>
    new Promise((resolve) =>
      set({
        variables: {
          names,
          resolve: (values) => {
            set({ variables: null });
            resolve(values);
          },
        },
      }),
    ),
  askOpenTransactions: (tabs, action, running = []) =>
    new Promise((resolve) =>
      set({
        openTransactions: {
          tabs,
          running,
          action,
          resolve: (proceed) => {
            set({ openTransactions: null });
            resolve(proceed);
          },
        },
      }),
    ),
  pushInteraction: (item) => set((s) => ({ interactions: [...s.interactions, item] })),
  popInteraction: (id) => set((s) => ({ interactions: s.interactions.filter((i) => i.id !== id) })),
}));
