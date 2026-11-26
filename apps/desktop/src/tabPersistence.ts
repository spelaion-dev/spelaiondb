// Open tabs that survive a restart (ADR-036). The core keeps each tab under its connection's
// folder, and one on a personal connection or none under the window's key (ADR-075), and
// decides how often they are written; this module sends the window's tabs as they are now (at
// most every 250 ms while they change), each with its connection's folder, and puts back what
// the core restores. Restored tabs are disconnected: the optional "reconnect at startup" only
// connects them, it never runs anything.

import { call, commands, type OpenTabItem, type RestoredTabsItem } from "./api";
import { connectionFailedRun, openTabSession } from "./connect";
import { isModified } from "./sqlFiles";
import { useApp, type Tab } from "./store";
import { stillOnConnection, tabConnectionOf } from "./connectionKey";
import { CONNECTING, DISCONNECTED, connectFailed, connectedWith, hasSession } from "./tabState";

/** Longest wait before the tabs as they are now go to the core. */
const SEND_DELAY_MS = 250;

/** The tabs on screen were restored: nothing is sent before, so an empty start (or a folder
 *  switch in progress) can never overwrite the kept tabs. */
let restored = false;
let lastSent = "";
let timer: ReturnType<typeof setTimeout> | null = null;

/** Only editor tabs are kept: table data and metadata tabs are views opened again from the
 *  explorer. Never results, transaction or session state. */
function kept(tab: Tab): boolean {
  return !tab.table && !tab.tableDetails && !tab.plan && !tab.diagram;
}

function snapshot(tabs: Tab[], activeTab: string | null): { tabs: OpenTabItem[]; active: number | null } {
  const items = tabs.filter(kept);
  const active = items.findIndex((t) => t.id === activeTab);
  return {
    tabs: items.map((tab) => ({
      title: tab.title ?? null,
      text: tab.sql,
      connection: tab.connectionId,
      database: tab.connectionId ? (tab.database ?? null) : null,
      // The core keeps the tab under this folder, or under the window's key (ADR-075).
      folder: tab.connectionId ? tab.connectionFolder : null,
      pinned: tab.pinned ?? false,
      onError: tab.runOptions?.onError ?? null,
      timeoutSeconds: tab.runOptions?.timeoutSeconds ?? null,
      snippetName: tab.snippetName ?? null,
      file: tab.file
        ? { path: tab.file.path, hash: tab.file.hash, format: tab.file.format, modified: isModified(tab) }
        : null,
    })),
    active: active < 0 ? null : active,
  };
}

/** Sends the tabs now if they changed since the last time. */
function sendNow() {
  if (timer) {
    clearTimeout(timer);
    timer = null;
  }
  if (!restored) return;
  const { tabs, activeTab } = useApp.getState();
  const current = snapshot(tabs, activeTab);
  const text = JSON.stringify(current);
  if (text === lastSent) return;
  lastSent = text;
  void call(commands.saveTabs(current.tabs, current.active)).catch(() => {
    // Sent again with the next change.
    lastSent = "";
  });
}

function schedule() {
  if (!restored || timer) return;
  timer = setTimeout(sendNow, SEND_DELAY_MS);
}

/** Sends what is waiting and has the core write it (before folders change). */
export async function flushTabs(): Promise<void> {
  sendNow();
  await call(commands.flushTabs()).catch(() => undefined);
}

/** Stops sending: the folders are about to change, and the tabs on screen with them. */
export function detachTabs() {
  if (timer) clearTimeout(timer);
  timer = null;
  restored = false;
  lastSent = "";
}

/** Puts back the window's kept tabs and every open folder's, replacing the ones on screen, and
 *  starts keeping them. A tab on a personal connection, or none, comes back whatever folder is
 *  open (ADR-075). With "reconnect at startup", connects them one at a time afterwards (never
 *  runs). */
export async function restoreTabs(): Promise<void> {
  let kept: RestoredTabsItem;
  try {
    kept = await call(commands.restoreTabsForWorkspace(null));
  } catch {
    return;
  }
  useApp.getState().restoreTabs(kept.tabs, kept.active);
  restored = true;
  lastSent = JSON.stringify(snapshot(useApp.getState().tabs, useApp.getState().activeTab));
  if (kept.reconnect) void reconnectRestored();
}

/** Puts back the kept tabs of the folder `root` (as listed), just opened beside the others,
 *  among the tabs on screen (ADR-075). They come back disconnected; nothing connects. */
export async function restoreFolderTabs(root: string): Promise<void> {
  const kept = await call(commands.restoreTabsForWorkspace(root));
  useApp.getState().addRestoredTabs(kept.tabs);
}

/** Runs `change` (a folder opened or closed beside the others) with the tabs written first and
 *  none sent meanwhile: the core keeps a set per open folder, so the tabs on screen must match
 *  the open folders again (a closed folder's taken out, an opened one's put back) before they
 *  are sent, or a folder's kept tabs would be written as none. */
export async function whileFoldersChange<T>(change: () => Promise<T>): Promise<T> {
  const keeping = restored;
  await flushTabs();
  detachTabs();
  try {
    return await change();
  } finally {
    if (keeping) {
      restored = true;
      schedule();
    }
  }
}

/** Connects the restored tabs that have a connection, one at a time, as a click would: a
 *  password or trust question may come up. It runs nothing (ADR-036, ADR-028). */
async function reconnectRestored() {
  const ids = useApp.getState().tabs.filter((t) => kept(t) && t.connectionId).map((t) => t.id);
  for (const id of ids) {
    const tab = useApp.getState().tabs.find((t) => t.id === id);
    if (!tab?.connectionId || hasSession(tab)) continue;
    const connection = tabConnectionOf(tab.connectionId, tab.connectionFolder);
    useApp.getState().setConnection(id, CONNECTING);
    try {
      const session = await openTabSession(id, connection);
      const now = useApp.getState().tabs.find((t) => t.id === id);
      if (!now || !stillOnConnection(now, connection)) {
        await call(commands.disconnect(id)).catch(() => undefined);
        continue;
      }
      useApp.getState().setConnection(id, session === null ? DISCONNECTED : connectedWith(session));
      // Cancelled (a password not given): the others are not asked either.
      if (session === null) break;
    } catch (e) {
      const detail = e instanceof Error ? e.message : String(e);
      useApp.getState().updateTab(id, { connectionState: connectFailed(detail), run: connectionFailedRun(detail, []) });
    }
  }
}

// Every change to the tabs schedules a send; the snapshot decides whether anything changed.
useApp.subscribe((state, previous) => {
  if (state.tabs !== previous.tabs || state.activeTab !== previous.activeTab) schedule();
});
