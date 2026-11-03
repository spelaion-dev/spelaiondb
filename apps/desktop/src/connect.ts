// Opens a session with whatever dialogs are needed: repository trust (ADR-009) and the
// password. Passwords go straight to the backend; they are never kept in the state.

import { call, commands } from "./api";
import { useApp, useDialogs } from "./store";

/** Returns the server version, or null if the user cancelled. Throws if it fails. */
export async function openSession(session: string, connectionId: string): Promise<string | null> {
  const app = useApp.getState();
  const dialogs = useDialogs.getState();
  const name = app.connections.find((c) => c.id === connectionId)?.name ?? connectionId;

  let outcome = await call(commands.connect(session, connectionId, null, false));
  if (outcome.kind === "untrusted") {
    const workspace = app.workspace?.workspace ?? null;
    if (!(await dialogs.askTrust(workspace))) return null;
    useApp.getState().setWorkspace(await call(commands.trustWorkspace()));
    outcome = await call(commands.connect(session, connectionId, null, false));
  }
  // Only ask again if the server rejected the password; any other failure (server down,
  // tunnel, TLS…) arrives as an exception and is shown as is.
  let error: string | null = null;
  while (outcome.kind === "passwordRequired" || outcome.kind === "authFailed") {
    if (outcome.kind === "authFailed") error = outcome.message;
    const answer = await dialogs.askPassword(name, outcome.canRemember, error);
    if (!answer) return null;
    outcome = await call(commands.connect(session, connectionId, answer.password, answer.remember));
  }
  if (outcome.kind === "untrusted") return null;
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
      const serverVersion = await call(commands.connectFromSession(source, tabId, database ?? null));
      const tab = useApp.getState().tabs.find((item) => item.id === tabId);
      if (!tab || tab.connectionId !== connectionId || tab.status !== "connecting") {
        await call(commands.disconnect(tabId)).catch(() => undefined);
        return;
      }
      useApp.getState().updateTab(tabId, { status: "connected", serverVersion });
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
export function closeQueryTab(id: string) {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab) return;
  useApp.getState().closeTab(id);
  void call(commands.disconnect(id)).catch(() => undefined);
  for (const r of tab.run?.results ?? []) void call(commands.releaseResult(r.id)).catch(() => undefined);
}
