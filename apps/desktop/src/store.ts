// UI state. Result data does NOT live here (AGENTS.md): only its metadata; rows are
// requested from the core in windows.

import { create } from "zustand";
import type { BookmarkItem, ColumnFilterItem, ColumnItem, ConnectionItem, FieldChangeItem, InteractionKind, RiskKind, WorkspaceInfo } from "./api";

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
}

export type TabStatus = "disconnected" | "connecting" | "connected";
export type TableDetailKind = "columns" | "constraints" | "foreignKeys" | "indexes" | "triggers";

export interface Tab {
  id: string;
  number: number;
  connectionId: string | null;
  status: TabStatus;
  /** Why its session was lost (ADR-028): nothing runs until the user reconnects. */
  lost?: string;
  /** Its session was closed by its connection type after this many seconds idle (ADR-029). */
  idleClosed?: number;
  serverVersion: string | null;
  sql: string;
  run: RunState | null;
  /** Name it was saved under (or opened from), offered again by Ctrl+S. */
  snippetName?: string;
  /** A table data tab (pages of one table) instead of an SQL editor. */
  table?: TableView;
  tableDetails?: { schema: string; name: string; kind: TableDetailKind };
  /** Pending row changes are kept in the mounted editor until saved or discarded. */
  pendingEdits?: boolean;
  savingEdits?: boolean;
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

let tabCounter = 0;

export type ConnectionDialog = { kind: "add" } | { kind: "edit"; id: string };

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
  historyRevision: number;
  historySaving: string[];
  workspaceChanging: boolean;
  /** Text of the explorer's quick filter; null while its box is closed. */
  explorerFilter: string | null;
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
  setWorkspace(info: WorkspaceInfo): void;
  setConnections(list: ConnectionItem[]): void;
  addTab(connectionId: string | null, sql?: string): string;
  /** Opens a table data tab on a table (its first page loads when it appears). */
  addTableTab(connectionId: string, schema: string, name: string): string;
  addTableDetailsTab(connectionId: string, schema: string, name: string, kind: TableDetailKind): string;
  closeTab(id: string): void;
  /** Opens again the last closed query tab (its connection and SQL, not its results). */
  reopenTab(): void;
  /** Closed query tabs, the last one at the end. */
  closedTabs: { connectionId: string | null; sql: string }[];
  setActiveTab(id: string): void;
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
  refreshHistory(): void;
  setHistorySaving(connection: string, saving: boolean): void;
  setWorkspaceChanging(changing: boolean): void;
  clearTabConnections(): void;
  setExplorerFilter(filter: string | null): void;
  setConnectionOpen(connection: string, open: boolean): void;
  setExplorerConnected(connection: string, connected: boolean): void;
  /** Ctrl+click: adds or removes one; choosing in another session starts again. */
  toggleExplorerSelection(item: { session: string; schema: string; name: string }): void;
  clearExplorerSelection(): void;
  revealInExplorer(target: Omit<ExplorerReveal, "nonce">): void;
  /** The explorer showed it (or cannot): later renders do not scroll to it again. */
  revealDone(nonce: number): void;
  setOpenObject(open: { connectionId: string | null } | null): void;
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
  historyRevision: 0,
  historySaving: [],
  workspaceChanging: false,
  explorerFilter: null,
  openConnections: [],
  explorerConnected: [],
  explorerSelection: [],
  explorerReveal: null,
  openObject: null,
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
    set((s) => ({ tabs: [...s.tabs, tab], activeTab: id }));
    return id;
  },
  addTableTab: (connectionId, schema, name): string => {
    const app: AppStore = useApp.getState();
    const id: string = app.addTab(connectionId);
    app.updateTab(id, { table: { schema, name, filter: "", orderBy: "", pageSize: DEFAULT_PAGE_SIZE, page: 0 } });
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
  closedTabs: [],
  closeTab: (id) =>
    set((s) => {
      const closed = s.tabs.find((t) => t.id === id);
      const tabs = s.tabs.filter((t) => t.id !== id);
      const fallback = tabs[tabs.length - 1]?.id ?? null;
      const closedTabs = closed && !closed.tableDetails
        ? [...s.closedTabs, { connectionId: closed.connectionId, sql: closed.sql }].slice(-MAX_CLOSED_TABS)
        : s.closedTabs;
      return { tabs, closedTabs, activeTab: s.activeTab === id ? fallback : s.activeTab };
    }),
  reopenTab: () => {
    const s = useApp.getState();
    const last = s.closedTabs[s.closedTabs.length - 1];
    if (!last) return;
    set({ closedTabs: s.closedTabs.slice(0, -1) });
    // The connection may have been deleted since.
    const connectionId = last.connectionId && s.connections.some((c) => c.id === last.connectionId) ? last.connectionId : null;
    s.addTab(connectionId, last.sql);
  },
  setActiveTab: (activeTab) => set({ activeTab }),
  updateTab: (id, patch) => set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, ...patch } : t)) })),
  updateRun: (id, update) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.run ? { ...t, run: update(t.run) } : t)) })),
  setHistoryOpen: (historyOpen) => set(historyOpen ? { historyOpen, snippetsOpen: false } : { historyOpen }),
  setSnippetsOpen: (snippetsOpen) => set(snippetsOpen ? { snippetsOpen, historyOpen: false } : { snippetsOpen }),
  refreshSnippets: () => set((s) => ({ snippetsRevision: s.snippetsRevision + 1 })),
  setConnectionDialog: (connectionDialog) => set({ connectionDialog }),
  setDeletingConnection: (deletingConnection) => set({ deletingConnection }),
  openSettings: () => set({ settingsOpen: true }),
  closeSettings: () => set({ settingsOpen: false }),
  refreshHistory: () => set((s) => ({ historyRevision: s.historyRevision + 1 })),
  setHistorySaving: (connection, saving) => set((s) => ({
    historySaving: saving ? [...new Set([...s.historySaving, connection])] : s.historySaving.filter((id) => id !== connection),
  })),
  setWorkspaceChanging: (workspaceChanging) => set({ workspaceChanging }),
  setExplorerFilter: (explorerFilter) => set({ explorerFilter }),
  toggleExplorerSelection: (item) => set((s) => {
    const same = (o: typeof item) => o.session === item.session && o.schema === item.schema && o.name === item.name;
    const mine = s.explorerSelection.filter((o) => o.session === item.session);
    return { explorerSelection: mine.some(same) ? mine.filter((o) => !same(o)) : [...mine, item] };
  }),
  clearExplorerSelection: () => set((s) => (s.explorerSelection.length === 0 ? s : { explorerSelection: [] })),
  revealInExplorer: (target) => set((s) => ({ explorerReveal: { ...target, nonce: (s.explorerReveal?.nonce ?? 0) + 1 } })),
  revealDone: (nonce) => set((s) => (s.explorerReveal?.nonce === nonce ? { explorerReveal: null } : s)),
  setOpenObject: (openObject) => set({ openObject }),
  setBookmarks: (bookmarks) => set({ bookmarks }),
  setExplorerConnected: (connection, connected) => set((s) => ({
    explorerConnected: connected ? [...new Set([...s.explorerConnected, connection])] : s.explorerConnected.filter((id) => id !== connection),
  })),
  setConnectionOpen: (connection, open) => set((s) => ({
    openConnections: open ? [...new Set([...s.openConnections, connection])] : s.openConnections.filter((id) => id !== connection),
  })),
  clearTabConnections: () => set((s) => ({ tabs: s.tabs.map((tab) => ({ ...tab, connectionId: null, status: "disconnected", serverVersion: null, run: null })) })),
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

/** Values for the `:name` placeholders of a script. */
export interface ParamsRequest {
  names: string[];
  /** Values used last time in this tab, to start from. */
  previous: Record<string, string>;
  resolve(values: Record<string, string> | null): void;
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
  params: ParamsRequest | null;
  text: TextRequest | null;
  interactions: InteractionItem[];
  askText(request: Omit<TextRequest, "resolve">): Promise<string | null>;
  askParams(names: string[], previous: Record<string, string>): Promise<Record<string, string> | null>;
  askRisks(connection: string, statements: RiskRequest["statements"]): Promise<boolean>;
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
  params: null,
  text: null,
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
  pushInteraction: (item) => set((s) => ({ interactions: [...s.interactions, item] })),
  popInteraction: (id) => set((s) => ({ interactions: s.interactions.filter((i) => i.id !== id) })),
}));
