// Opens a session with whatever dialogs are needed: repository trust (ADR-009), changes to
// trusted repository items (ADR-022) and the password. Passwords go straight to the
// backend; they are never kept in the state.

import { call, commands, type ConnectionTestResult, type ConnectResult } from "./api";
import { confirm } from "@tauri-apps/plugin-dialog";
import i18n from "./i18n";
import { useApp, useDialogs } from "./store";

/**
 * Fired on `window` with a connection id when the explorer refreshes all its schemas: editor
 * tabs on that connection reload the names their autocompletion offers.
 */
export const SCHEMAS_REFRESHED_EVENT = "spelaion-schemas-refreshed";
/**
 * Fired on `window` with `{ connectionId, idleSeconds }` when the explorer's own session of a
 * connection is lost or closed for being idle (ADR-029): its tree collapses, and expanding it
 * again reconnects.
 */
export const EXPLORER_SESSION_CLOSED_EVENT = "spelaion-explorer-session-closed";
/** A catalog change in a metadata tab asks the explorer to reread its objects. */
export const EXPLORER_REFRESH_EVENT = "spelaion-explorer-refresh";

/**
 * Fired on `window` with `{ tabId, schema }` by "Set as the editor's schema" in the explorer:
 * that editor tab switches its own session's schema (its database on MySQL and MariaDB).
 */
export const SET_ACTIVE_SCHEMA_EVENT = "spelaion-set-active-schema";

/** What must be answered before resolving (ADR-022): a repository connection or tunnel that
 *  is new or changed (shown, and recorded if accepted) and personal `${user.…}` variables
 *  that are not defined yet (asked for and saved). Tries again after each answer; returns
 *  null if the user refused. */
export async function reviewChanges<T extends ConnectResult | ConnectionTestResult>(
  outcome: T,
  retry: () => Promise<T>,
): Promise<T | null> {
  // A connection and its tunnel may both have changed: one dialog each.
  for (let i = 0; i < 10; i++) {
    const current: ConnectResult | ConnectionTestResult = outcome;
    if (current.kind === "changed") {
      const accepted = await useDialogs.getState().askChange({ item: current.item, isNew: current.new, fields: current.fields });
      if (!accepted) return null;
      await call(commands.acceptChange(current.root, current.item, current.new, current.fields));
    } else if (current.kind === "missingVariables") {
      const values = await useDialogs.getState().askVariables(current.names);
      if (!values) return null;
      await call(commands.setUserVars(values));
    } else {
      break;
    }
    outcome = await retry();
  }
  return outcome;
}

/** An SQL editor tab: its session follows its transaction (ADR-031). The explorer's, the table
 *  data tabs' and the process monitor's sessions stay in auto-commit. */
function isEditorSession(session: string): boolean {
  const tab = useApp.getState().tabs.find((t) => t.id === session);
  return Boolean(tab && !tab.table && !tab.tableDetails);
}

/** Shows the transaction the core keeps for a freshly connected editor tab. */
async function showTransaction(session: string) {
  if (!isEditorSession(session)) return;
  try {
    const transaction = await call(commands.transactionStatus(session));
    useApp.getState().updateTab(session, { transaction, lostTransaction: undefined });
  } catch {
    // The tab was closed meanwhile.
  }
}

/** A tab's name as its tab shows it. */
export function tabName(id: string): string {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab) return id;
  if (tab.tableDetails) return `${tab.tableDetails.name} · ${i18n.t(`shell:folders.${tab.tableDetails.kind}`)}`;
  if (tab.table) return tab.table.name;
  return i18n.t("shell:queryTab", { number: tab.number });
}

/**
 * Asks before closing, disconnecting or quitting sessions with an open transaction (ADR-031):
 * the core says which ones need the question. True when there is nothing to ask or the user
 * accepted; also when the core cannot be asked (the sessions are gone).
 */
export async function confirmOpenTransactions(sessions: string[], action: "close" | "disconnect" | "quit"): Promise<boolean> {
  if (sessions.length === 0) return true;
  let open: string[];
  try {
    open = await call(commands.sessionsToConfirm(sessions));
  } catch {
    return true;
  }
  if (open.length === 0) return true;
  return useDialogs.getState().askOpenTransactions(open.map(tabName), action);
}

/** Returns the server version, or null if the user cancelled. Throws if it fails. */
export async function openSession(session: string, connectionId: string): Promise<string | null> {
  const version = await openSessionInner(session, connectionId);
  if (version !== null) await showTransaction(session);
  return version;
}

async function openSessionInner(session: string, connectionId: string): Promise<string | null> {
  const app = useApp.getState();
  const dialogs = useDialogs.getState();
  const name = app.connections.find((c) => c.id === connectionId)?.name ?? connectionId;
  const editor = isEditorSession(session);

  const connect = () => call(commands.connect(session, connectionId, null, false, editor));
  let outcome = await connect();
  if (outcome.kind === "untrusted") {
    const workspace = app.workspace?.workspace ?? null;
    if (!(await dialogs.askTrust(workspace))) return null;
    useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace ?? "")));
    outcome = await connect();
  }
  const reviewed = await reviewChanges(outcome, connect);
  if (!reviewed) return null;
  outcome = reviewed;
  // Only ask again if the server rejected the password; any other failure (server down,
  // tunnel, TLS…) arrives as an exception and is shown as is.
  let error: string | null = null;
  while (outcome.kind === "passwordRequired" || outcome.kind === "authFailed") {
    if (outcome.kind === "authFailed") error = outcome.message;
    const answer = await dialogs.askPassword(name, outcome.canRemember, error);
    if (!answer) return null;
    outcome = await call(commands.connect(session, connectionId, answer.password, answer.remember, editor));
  }
  if (outcome.kind === "untrusted" || outcome.kind === "changed" || outcome.kind === "missingVariables") return null;
  return outcome.serverVersion;
}

/** Open an independent editor session from a connected explorer without exposing its password. */
export function connectTabFromExplorer(tabId: string, source: string, database?: string) {
  const initial = useApp.getState().tabs.find((tab) => tab.id === tabId);
  if (!initial?.connectionId) return;
  const connectionId = initial.connectionId;
  useApp.getState().updateTab(tabId, { status: "connecting", serverVersion: null });
  void (async () => {
    try {
      const serverVersion = await call(commands.connectFromSession(source, tabId, database ?? null, isEditorSession(tabId)));
      const tab = useApp.getState().tabs.find((item) => item.id === tabId);
      if (!tab || tab.connectionId !== connectionId || tab.status !== "connecting") {
        await call(commands.disconnect(tabId)).catch(() => undefined);
        return;
      }
      useApp.getState().updateTab(tabId, { status: "connected", serverVersion });
      await showTransaction(tabId);
    } catch (e) {
      const tab = useApp.getState().tabs.find((item) => item.id === tabId);
      if (!tab || tab.connectionId !== connectionId || tab.status !== "connecting") return;
      const detail = e instanceof Error ? e.message : String(e);
      useApp.getState().updateTab(tabId, {
        status: "disconnected",
        run: { running: false, results: [], activeResult: null, messages: [], error: { message: detail, offset: null, translation: { key: "messages.connectionFailed", values: { detail } } }, startedAt: 0, elapsedMs: null },
      });
    }
  })();
}

/** MySQL and MariaDB call databases schemas in their explorer. */
export function explorerDatabase(connectionId: string, schema: string): string | undefined {
  const engine = useApp.getState().connections.find((item) => item.id === connectionId)?.engine;
  return engine === "mysql" || engine === "mariadb" ? schema : undefined;
}

/** Closes a query tab: its session is disconnected and its results are freed in the core (a
 *  script still running finishes and is recorded). Ctrl+Shift+T reopens it, without results. */
export async function closeQueryTab(id: string) {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab) return;
  if (tab.savingEdits) return;
  if (tab.pendingEdits && !(await confirm(i18n.t("editor:edit.closeUnsaved"), { kind: "warning" }))) return;
  if (!(await confirmOpenTransactions([id], "close"))) return;
  if (useApp.getState().tabs.find((t) => t.id === id)?.savingEdits) return;
  useApp.getState().closeTab(id);
  void call(commands.disconnect(id)).catch(() => undefined);
  for (const r of tab.run?.results ?? []) void call(commands.releaseResult(r.id)).catch(() => undefined);
}
