// Open tabs that survive a restart (ADR-036). The core keeps them per folder and decides how
// often they are written; this module sends what the tabs look like now (at most every
// 250 ms while they change) and puts back what the core restores. Restored tabs are
// disconnected: the optional "reconnect at startup" only connects them, it never runs anything.

import { call, commands, type OpenTabItem, type RestoredTabsItem } from "./api";
import { openSession } from "./connect";
import { isModified } from "./sqlFiles";
import { useApp, type Tab } from "./store";

/** Longest wait before the tabs as they are now go to the core. */
const SEND_DELAY_MS = 250;

/** The folder the tabs on screen belong to; null until they were restored (nothing is sent
 *  before, so an empty start can never overwrite the kept tabs). */
let scope: string | null = null;
let lastSent = "";
let timer: ReturnType<typeof setTimeout> | null = null;

/** Only editor tabs are kept: table data and metadata tabs are views opened again from the
 *  explorer. Never results, transaction or session state. */
function kept(tab: Tab): boolean {
  return !tab.table && !tab.tableDetails && !tab.plan;
}

function snapshot(tabs: Tab[], activeTab: string | null): { tabs: OpenTabItem[]; active: number | null } {
  const items = tabs.filter(kept);
  const active = items.findIndex((t) => t.id === activeTab);
  return {
    tabs: items.map((tab) => ({
      title: tab.title ?? null,
      text: tab.sql,
      connection: tab.connectionId,
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
  if (scope === null) return;
  const { tabs, activeTab } = useApp.getState();
  const current = snapshot(tabs, activeTab);
  const text = JSON.stringify(current);
  if (text === lastSent) return;
  lastSent = text;
  void call(commands.saveTabs(scope, current.tabs, current.active)).catch(() => {
    // Sent again with the next change.
    lastSent = "";
  });
}

function schedule() {
  if (scope === null || timer) return;
  timer = setTimeout(sendNow, SEND_DELAY_MS);
}

/** Sends what is waiting and has the core write it (before switching folders). */
export async function flushTabs(): Promise<void> {
  sendNow();
  await call(commands.flushTabs()).catch(() => undefined);
}

/** Stops sending: the tabs on screen are about to belong to another folder. */
export function detachTabs() {
  if (timer) clearTimeout(timer);
  timer = null;
  scope = null;
  lastSent = "";
}

/** Puts back the open folder's kept tabs, replacing the ones on screen, and starts keeping
 *  them. With "reconnect at startup", connects them one at a time afterwards (never runs). */
export async function restoreTabs(): Promise<void> {
  let restored: RestoredTabsItem;
  try {
    restored = await call(commands.restoreTabsForWorkspace());
  } catch {
    return;
  }
  useApp.getState().restoreTabs(restored.tabs, restored.active);
  scope = restored.scope;
  lastSent = JSON.stringify(snapshot(useApp.getState().tabs, useApp.getState().activeTab));
  if (restored.reconnect) void reconnectRestored();
}

/** Connects the restored tabs that have a connection, one at a time, as a click would: a
 *  password or trust question may come up. It runs nothing (ADR-036, ADR-028). */
async function reconnectRestored() {
  const ids = useApp.getState().tabs.filter((t) => kept(t) && t.connectionId).map((t) => t.id);
  for (const id of ids) {
    const tab = useApp.getState().tabs.find((t) => t.id === id);
    if (!tab?.connectionId || tab.status !== "disconnected") continue;
    const connectionId = tab.connectionId;
    useApp.getState().updateTab(id, { status: "connecting" });
    try {
      const version = await openSession(id, connectionId);
      const now = useApp.getState().tabs.find((t) => t.id === id);
      if (!now || now.connectionId !== connectionId) {
        await call(commands.disconnect(id)).catch(() => undefined);
        continue;
      }
      useApp.getState().updateTab(id, version === null ? { status: "disconnected" } : { status: "connected", serverVersion: version });
      // Cancelled (a password not given): the others are not asked either.
      if (version === null) break;
    } catch (e) {
      const detail = e instanceof Error ? e.message : String(e);
      useApp.getState().updateTab(id, {
        status: "disconnected",
        run: { running: false, results: [], activeResult: null, messages: [], error: { message: detail, offset: null, translation: { key: "messages.connectionFailed", values: { detail } } }, startedAt: 0, elapsedMs: null },
      });
    }
  }
}

// Every change to the tabs schedules a send; the snapshot decides whether anything changed.
useApp.subscribe((state, previous) => {
  if (state.tabs !== previous.tabs || state.activeTab !== previous.activeTab) schedule();
});
