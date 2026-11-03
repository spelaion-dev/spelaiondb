// UI state. Result data does NOT live here (CLAUDE.md): only its metadata; rows are
// requested from the core in windows.

import { create } from "zustand";
import type { ColumnFilterItem, ColumnItem, ConnectionItem, InteractionKind, RiskKind, WorkspaceInfo } from "./api";

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

export interface Tab {
  id: string;
  number: number;
  connectionId: string | null;
  status: TabStatus;
  serverVersion: string | null;
  sql: string;
  run: RunState | null;
  /** Name it was saved under (or opened from), offered again by Ctrl+S. */
  snippetName?: string;
  /** A table data tab (pages of one table) instead of an SQL editor. */
  table?: TableView;
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

/** Window event asking the editor tab whose id is in `detail` to save its query (Ctrl+S). */
export const SAVE_QUERY_EVENT = "spelunk:save-query";

/** How many closed tabs Ctrl+Shift+T can bring back. */
const MAX_CLOSED_TABS = 20;

/** `activeTab` value of the settings tab, which lives beside the query tabs. */
export const SETTINGS_TAB = "settings";

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
  setWorkspace(info: WorkspaceInfo): void;
  setConnections(list: ConnectionItem[]): void;
  addTab(connectionId: string | null, sql?: string): string;
  /** Opens a table data tab on a table (its first page loads when it appears). */
  addTableTab(connectionId: string, schema: string, name: string): string;
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
  closedTabs: [],
  closeTab: (id) =>
    set((s) => {
      const closed = s.tabs.find((t) => t.id === id);
      const tabs = s.tabs.filter((t) => t.id !== id);
      const fallback = tabs[tabs.length - 1]?.id ?? (s.settingsOpen ? SETTINGS_TAB : null);
      const closedTabs = closed
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
  openSettings: () => set({ settingsOpen: true, activeTab: SETTINGS_TAB }),
  closeSettings: () =>
    set((s) => ({
      settingsOpen: false,
      activeTab: s.activeTab === SETTINGS_TAB ? (s.tabs[s.tabs.length - 1]?.id ?? null) : s.activeTab,
    })),
  refreshHistory: () => set((s) => ({ historyRevision: s.historyRevision + 1 })),
  setHistorySaving: (connection, saving) => set((s) => ({
    historySaving: saving ? [...new Set([...s.historySaving, connection])] : s.historySaving.filter((id) => id !== connection),
  })),
  setWorkspaceChanging: (workspaceChanging) => set({ workspaceChanging }),
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
  risk: RiskRequest | null;
  params: ParamsRequest | null;
  text: TextRequest | null;
  interactions: InteractionItem[];
  askText(request: Omit<TextRequest, "resolve">): Promise<string | null>;
  askParams(names: string[], previous: Record<string, string>): Promise<Record<string, string> | null>;
  askRisks(connection: string, statements: RiskRequest["statements"]): Promise<boolean>;
  askPassword(connectionName: string, canRemember: boolean, error: string | null): Promise<{ password: string; remember: boolean } | null>;
  askTrust(workspace: string | null): Promise<boolean>;
  pushInteraction(item: InteractionItem): void;
  popInteraction(id: number): void;
}

export const useDialogs = create<DialogStore>((set) => ({
  password: null,
  trust: null,
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
  pushInteraction: (item) => set((s) => ({ interactions: [...s.interactions, item] })),
  popInteraction: (id) => set((s) => ({ interactions: s.interactions.filter((i) => i.id !== id) })),
}));
