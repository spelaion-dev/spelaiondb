// UI state. Result data does NOT live here (CLAUDE.md): only its metadata; rows are
// requested from the core in windows.

import { create } from "zustand";
import type { ColumnItem, ConnectionItem, InteractionKind, WorkspaceInfo } from "./api";

export interface ResultMeta {
  id: number;
  statement: number;
  columns: ColumnItem[];
  rows: number;
  complete: boolean;
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
}

let tabCounter = 0;

interface AppStore {
  workspace: WorkspaceInfo | null;
  connections: ConnectionItem[];
  tabs: Tab[];
  activeTab: string | null;
  historyOpen: boolean;
  historyRevision: number;
  historySaving: string[];
  workspaceChanging: boolean;
  setWorkspace(info: WorkspaceInfo): void;
  setConnections(list: ConnectionItem[]): void;
  addTab(connectionId: string | null, sql?: string): string;
  closeTab(id: string): void;
  setActiveTab(id: string): void;
  updateTab(id: string, patch: Partial<Tab>): void;
  updateRun(id: string, update: (run: RunState) => RunState): void;
  setHistoryOpen(open: boolean): void;
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
  closeTab: (id) =>
    set((s) => {
      const tabs = s.tabs.filter((t) => t.id !== id);
      const activeTab = s.activeTab === id ? (tabs[tabs.length - 1]?.id ?? null) : s.activeTab;
      return { tabs, activeTab };
    }),
  setActiveTab: (activeTab) => set({ activeTab }),
  updateTab: (id, patch) => set((s) => ({ tabs: s.tabs.map((t) => (t.id === id ? { ...t, ...patch } : t)) })),
  updateRun: (id, update) =>
    set((s) => ({ tabs: s.tabs.map((t) => (t.id === id && t.run ? { ...t, run: update(t.run) } : t)) })),
  setHistoryOpen: (historyOpen) => set({ historyOpen }),
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

export interface InteractionItem {
  id: number;
  request: InteractionKind;
}

interface DialogStore {
  password: PasswordRequest | null;
  trust: TrustRequest | null;
  interactions: InteractionItem[];
  askPassword(connectionName: string, canRemember: boolean, error: string | null): Promise<{ password: string; remember: boolean } | null>;
  askTrust(workspace: string | null): Promise<boolean>;
  pushInteraction(item: InteractionItem): void;
  popInteraction(id: number): void;
}

export const useDialogs = create<DialogStore>((set) => ({
  password: null,
  trust: null,
  interactions: [],
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
