// Opens a session with whatever dialogs are needed: repository trust (ADR-009), changes to
// trusted repository items (ADR-022) and the password. Passwords go straight to the
// backend; they are never kept in the state.

import { call, commands, type ConnectionTestResult, type ConnectResult } from "./api";
import { runningOutsideTransactions } from "./closeQuestion";
import { confirm } from "@tauri-apps/plugin-dialog";
import i18n from "./i18n";
import { confirmUnsavedFiles } from "./sqlFiles";
import { type RunState, type Tab, useApp, useDialogs } from "./store";
import { CONNECTING, DISCONNECTED, connectFailed, connectedWith, hasPendingEdits, hasUnsavedEdits, isConnecting, isRunning, savingEdits, type OpenedSession } from "./tabState";

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
  return Boolean(tab && !tab.table && !tab.tableDetails && !tab.plan && !tab.diagram);
}

/** What a freshly opened session has: its version and, for an editor tab, the transaction the
 *  core keeps for it (ADR-031). */
async function opened(session: string, version: string): Promise<OpenedSession> {
  if (!isEditorSession(session)) return { version };
  try {
    return { version, transaction: await call(commands.transactionStatus(session)) };
  } catch {
    // The tab was closed meanwhile.
    return { version };
  }
}

/** What a tab shows when its session could not open and no run was asked for: the error, with
 *  the results it had (a run that failed before it started replaces nothing). */
export function connectionFailedRun(detail: string, results: RunState["results"]): RunState {
  return { running: false, results, activeResult: null, messages: [], error: { message: detail, offset: null, translation: { key: "messages.connectionFailed", values: { detail } } }, startedAt: 0, elapsedMs: null };
}

/** A tab's name as its tab shows it: the user's name for it, or its default one. */
export function tabLabel(tab: Tab): string {
  if (tab.title) return tab.title;
  return defaultTabLabel(tab);
}

/** What a tab is called before the user renames it. */
export function defaultTabLabel(tab: Tab): string {
  if (tab.file) return tab.file.name;
  if (tab.tableDetails) return `${tab.tableDetails.name} · ${i18n.t(`shell:folders.${tab.tableDetails.kind}`)}`;
  if (tab.table) return tab.table.name;
  if (tab.diagram) {
    const scope = tab.diagram;
    if (scope.kind === "schema") return i18n.t("shell:diagram.titleSchema", { schema: scope.schema });
    if (scope.kind === "focus") return i18n.t("shell:diagram.titleFocus", { table: scope.table });
    return i18n.t("shell:diagram.titleTables", { count: scope.tables.length });
  }
  return i18n.t("shell:queryTab", { number: tab.number });
}

/** A tab's name as its tab shows it, by id. */
export function tabName(id: string): string {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  return tab ? tabLabel(tab) : id;
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

/**
 * Asks before quitting or opening another folder (SDB-140), over every tab, as `closeTabs`
 * asks before closing tabs: row edits not applied (or being applied), queries running and
 * open transactions (`open`, tab ids) are all lost. True when there is nothing to ask or the
 * user accepted; nothing is applied, committed or saved either way. Unsaved `.sql` text is not
 * asked about: tabs come back with it (ADR-036).
 */
export async function confirmLeaving(action: "quit" | "folder", open: string[]): Promise<boolean> {
  const tabs = useApp.getState().tabs;
  const unsaved = tabs.filter(hasUnsavedEdits).map((t) => t.id);
  // A tab with an open transaction is asked about once, under it (SDB-253).
  const running = runningOutsideTransactions(tabs.filter(isRunning).map((t) => t.id), open);
  if (open.length === 0 && running.length === 0 && unsaved.length === 0) return true;
  return useDialogs.getState().askOpenTransactions(open.map(tabName), action, running.map(tabName), unsaved.map(tabName));
}

let answeringQuit = false;

/**
 * The window was asked to close while sessions are open (`QuitRequested`): the core waits for
 * the UI. It quits at once when nothing would be lost, else asks. The open transactions come
 * from what each session last read, so a server that does not answer cannot hold the question.
 * A second request while the question is open only tells the core the UI is alive.
 */
export async function answerQuitRequest() {
  await call(commands.quitAsking()).catch(() => undefined);
  if (answeringQuit) return;
  answeringQuit = true;
  try {
    const open = await call(commands.quitOpenTransactions()).catch(() => [] as string[]);
    if (await confirmLeaving("quit", open)) await call(commands.quitApp());
    else await call(commands.quitCancelled());
  } catch {
    await call(commands.quitCancelled()).catch(() => undefined);
  } finally {
    answeringQuit = false;
  }
}

/** Returns the server version (and an editor tab's transaction), or null if the user
 *  cancelled. Throws if it fails. */
export async function openSession(session: string, connectionId: string): Promise<OpenedSession | null> {
  const version = await openSessionInner(session, connectionId);
  return version === null ? null : opened(session, version);
}

async function openSessionInner(session: string, connectionId: string): Promise<string | null> {
  const app = useApp.getState();
  const dialogs = useDialogs.getState();
  const connection = app.connections.find((c) => c.id === connectionId);
  const name = connection?.name ?? connectionId;
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
  if (outcome.kind === "untrusted" || outcome.kind === "changed" || outcome.kind === "missingVariables" || outcome.kind === "cancelled") return null;
  // Setup statements the connection skipped (ADR-037) are said in the tab until dismissed.
  useApp.getState().updateTab(session, { setupSkipped: outcome.setupSkipped.length > 0 ? outcome.setupSkipped : undefined });
  const mark = outcome.engineMark;
  if (connection && connection.engineMark === connection.engine && mark) {
    const current = useApp.getState();
    if (current.connections.includes(connection)) {
      current.setConnections(current.connections.map((c) => c === connection ? { ...c, engineMark: mark } : c));
    }
  }
  return outcome.serverVersion;
}

/** Open an independent editor session from a connected explorer without exposing its password. */
export function connectTabFromExplorer(tabId: string, source: string, database?: string) {
  const initial = useApp.getState().tabs.find((tab) => tab.id === tabId);
  if (!initial?.connectionId) return;
  const connectionId = initial.connectionId;
  useApp.getState().setConnection(tabId, CONNECTING);
  void (async () => {
    try {
      const outcome = await call(commands.connectFromSession(source, tabId, database ?? null, isEditorSession(tabId)));
      // The connection was edited since the source opened and needs the user (SDB-426): the
      // tab connects as a new one does, from the saved file, then to the database it wanted.
      const session = outcome.kind === "connected"
        ? await opened(tabId, outcome.serverVersion)
        : await openSession(tabId, connectionId);
      const tab = useApp.getState().tabs.find((item) => item.id === tabId);
      if (!tab || tab.connectionId !== connectionId || !isConnecting(tab)) {
        if (session !== null) await call(commands.disconnect(tabId)).catch(() => undefined);
        return;
      }
      if (session === null) {
        useApp.getState().setConnection(tabId, DISCONNECTED);
        return;
      }
      if (outcome.kind === "connectAgain" && database) {
        await call(commands.selectDatabase(tabId, database)).catch(() => undefined);
      }
      useApp.getState().setConnection(tabId, connectedWith(session));
    } catch (e) {
      failConnecting(tabId, connectionId, e);
    }
  })();
}

/** Open a query on a connection, independently of the explorer's expanded state. */
export function newQueryOnConnection(connectionId: string): string {
  const app = useApp.getState();
  const tabId = app.addTab(connectionId);
  if (app.explorerConnected.includes(connectionId)) {
    connectTabFromExplorer(tabId, `meta:${connectionId}`);
    return tabId;
  }
  useApp.getState().setConnection(tabId, CONNECTING);
  void (async () => {
    try {
      const session = await openSession(tabId, connectionId);
      const tab = useApp.getState().tabs.find((item) => item.id === tabId);
      if (!tab || tab.connectionId !== connectionId || !isConnecting(tab)) {
        if (session !== null) await call(commands.disconnect(tabId)).catch(() => undefined);
        return;
      }
      useApp.getState().setConnection(tabId, session === null ? DISCONNECTED : connectedWith(session));
    } catch (e) {
      failConnecting(tabId, connectionId, e);
    }
  })();
  return tabId;
}

/** The session a tab was opening on `connectionId` failed with `error`: the tab says why, if
 *  it is still waiting for that session. */
function failConnecting(tabId: string, connectionId: string, error: unknown) {
  const tab = useApp.getState().tabs.find((item) => item.id === tabId);
  if (!tab || tab.connectionId !== connectionId || !isConnecting(tab)) return;
  const detail = error instanceof Error ? error.message : String(error);
  useApp.getState().updateTab(tabId, { connectionState: connectFailed(detail), run: connectionFailedRun(detail, tab.run?.results ?? []) });
}

/** Opens a table data tab on `schema.table` filtered by `filter` (a followed foreign key's,
 *  written by the core), on a session of its own opened like `source`'s. */
export function openTableData(connectionId: string, source: string, schema: string, table: string, filter = ""): string {
  const id = useApp.getState().addTableTab(connectionId, schema, table, filter);
  connectTabFromExplorer(id, source, explorerDatabase(connectionId, schema));
  return id;
}

/** MySQL and MariaDB call databases schemas in their explorer. */
export function explorerDatabase(connectionId: string, schema: string): string | undefined {
  const engine = useApp.getState().connections.find((item) => item.id === connectionId)?.engine;
  return engine === "mysql" || engine === "mariadb" ? schema : undefined;
}

/**
 * Closes tabs, asking once for all of them first: unsaved row edits, transactions the server
 * would roll back (the core says which, ADR-031) and queries still running. A running query is
 * cancelled (a run paused after an error stops, ADR-035), its session disconnected and its
 * results freed in the core. Ctrl+Shift+T reopens an SQL tab, without results or session.
 */
export async function closeTabs(ids: string[]) {
  const find = (id: string) => useApp.getState().tabs.find((t) => t.id === id);
  const targets = ids.map(find).filter((t): t is Tab => t !== undefined && !savingEdits(t));
  if (targets.length === 0) return;
  const unsaved = targets.filter(hasPendingEdits);
  if (unsaved.length > 0) {
    const text = unsaved.length === 1 && targets.length === 1
      ? i18n.t("editor:edit.closeUnsaved")
      : i18n.t("editor:edit.closeUnsavedMany", { count: unsaved.length, tabs: unsaved.map(tabLabel).join(", ") });
    if (!(await confirm(text, { kind: "warning" }))) return;
  }
  // File tabs with unsaved changes (ADR-036): save, don't save or cancel.
  if (!(await confirmUnsavedFiles(targets.map((t) => t.id)))) return;
  const sessions = targets.map((t) => t.id);
  if (!(await confirmClose(sessions))) return;
  // Saving started while the question was open: that tab stays.
  const closing = sessions.filter((id) => {
    const tab = find(id);
    return tab !== undefined && !savingEdits(tab);
  });
  if (closing.length === 0) return;
  const running = new Map(closing.map((id) => [id, isRunning(find(id))]));
  stopScriptQuestion(closing);
  useApp.getState().closeTabs(closing);
  for (const id of closing) {
    const wasRunning = running.get(id);
    void (async () => {
      // The core stops the run that sent the statement, then the session closes.
      if (wasRunning) await call(commands.cancel(id)).catch(() => undefined);
      await call(commands.disconnect(id)).catch(() => undefined);
    })();
    // The core frees every result of the tab, and any its run still starts (SDB-378).
    void call(commands.releaseResults(id)).catch(() => undefined);
  }
}

/** Closes one tab (the tab's ×, Ctrl+W). */
export function closeQueryTab(id: string) {
  return closeTabs([id]);
}

/**
 * Running queries and open transactions among the tabs to close: one question for all. A tab
 * with an open transaction is listed once, under it: its running statement is inside that
 * transaction, rolled back with it (SDB-253).
 */
async function confirmClose(sessions: string[]): Promise<boolean> {
  let open: string[] = [];
  try {
    open = await call(commands.sessionsToConfirm(sessions));
  } catch {
    // The sessions are gone: nothing is left to roll back.
  }
  const running = runningOutsideTransactions(
    sessions.filter((id) => isRunning(useApp.getState().tabs.find((t) => t.id === id))),
    open,
  );
  if (open.length === 0 && running.length === 0) return true;
  return useDialogs.getState().askOpenTransactions(open.map(tabName), "close", running.map(tabName));
}

/** A run of a closing tab that waits for an answer after an error stops: its dialog goes. */
function stopScriptQuestion(closing: string[]) {
  // Wherever it waits: a question behind another one's dialog too (SDB-139).
  useDialogs.getState().stopScriptQuestions(closing);
}
