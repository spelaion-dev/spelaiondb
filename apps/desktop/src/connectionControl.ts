// "Disconnect all" and "Disconnect all others" (SDB-395): every editor tab's session and every
// connection the explorer opened, but those of one connection when one is kept. What would be
// lost is asked first, as closing tabs asks (ADR-031: an open transaction rolls back; a query
// running is cancelled; row edits not applied are dropped). Nothing reconnects or runs again
// afterwards (ADR-028): each tab connects when the user runs in it, the explorer when a
// connection is expanded.

import { call, commands } from "./api";
import { runningOutsideTransactions } from "./closeQuestion";
import { EXPLORER_SESSION_CLOSED_EVENT, tabName } from "./connect";
import { useApp, useDialogs, type Tab } from "./store";
import { DISCONNECTED, hasSession, hasUnsavedEdits, isRunning } from "./tabState";

/** The editor tabs with a session, and the connections open in the explorer, but `keep`'s. */
function targets(keep: string | null): { tabs: Tab[]; explorer: string[] } {
  const app = useApp.getState();
  return {
    tabs: app.tabs.filter((tab) => tab.connectionId !== null && tab.connectionId !== keep && hasSession(tab)),
    explorer: app.explorerConnected.filter((id) => id !== keep),
  };
}

/** Whether there is anything to disconnect but `keep`'s sessions. */
export function hasSessions(keep: string | null): boolean {
  const { tabs, explorer } = targets(keep);
  return tabs.length > 0 || explorer.length > 0;
}

/** Disconnects every session but those of the connection `keep` (none: all). `false` when the
 *  user chose to keep them after being asked. */
export async function disconnectAll(keep: string | null): Promise<boolean> {
  const { tabs, explorer } = targets(keep);
  if (tabs.length === 0 && explorer.length === 0) return true;
  const ids = tabs.map((tab) => tab.id);
  let open: string[] = [];
  try {
    open = await call(commands.sessionsToConfirm(ids));
  } catch {
    open = [];
  }
  // A tab with an open transaction is asked about once, under it (SDB-253).
  const running = runningOutsideTransactions(tabs.filter(isRunning).map((tab) => tab.id), open);
  const unsaved = tabs.filter(hasUnsavedEdits).map((tab) => tab.id);
  if (open.length > 0 || running.length > 0 || unsaved.length > 0) {
    const accepted = await useDialogs.getState().askOpenTransactions(open.map(tabName), "disconnect", running.map(tabName), unsaved.map(tabName));
    if (!accepted) return false;
  }
  const app = useApp.getState();
  // The runs the user agreed to cancel stop first, then the sessions close.
  await Promise.all(tabs.filter(isRunning).map((tab) => call(commands.cancel(tab.id)).catch(() => undefined)));
  await Promise.all(ids.map((id) => call(commands.disconnect(id)).catch(() => undefined)));
  for (const id of ids) app.setConnection(id, DISCONNECTED);
  await Promise.all(explorer.map((id) => call(commands.disconnect(`meta:${id}`)).catch(() => undefined)));
  for (const id of explorer) {
    app.setExplorerConnected(id, false);
    // The tree folds, as when its session is lost; expanding it connects again.
    window.dispatchEvent(new CustomEvent(EXPLORER_SESSION_CLOSED_EVENT, { detail: { connectionId: id, idleSeconds: null } }));
  }
  return true;
}
