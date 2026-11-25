// Changing the open folder (SDB-397): from the folder menu, a recent folder, the command
// palette or a second launch, always the same way. It asks first about what closing the
// sessions would lose (ADR-031), never connects or runs anything, and the folder then goes
// through repository trust like any other (ADR-022). Recent folders are the core's list.

import { call, commands } from "./api";
import { explorerSession, windowContext } from "./windowContext";
import type { RecentFolderItem } from "./bindings";
import { confirmLeaving } from "./connect";
import { useApp } from "./store";
import { isBusy, isConnecting, isRunning, savingEdits } from "./tabState";
import { detachTabs, flushTabs, restoreTabs } from "./tabPersistence";
import { refreshConnections } from "./components/Sidebar";

/** Fired on `window` when the recent folders change. */
export const RECENT_FOLDERS_EVENT = "spelaion-recent-folders";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Whether the folder cannot change now: it is changing, history is being saved, or a tab is
 *  connecting or running. */
export function folderSwitchBlocked(state = useApp.getState()): boolean {
  // The open folder is the whole app's: only the main window changes it (SDB-30).
  if (windowContext().kind !== "main") return true;
  return state.workspaceChanging || state.historySaving.length > 0 || state.tabs.some(isBusy);
}

/** Whether `path` is the folder open now. */
export function isOpenFolder(path: string): boolean {
  const open = useApp.getState().workspace?.workspace;
  if (!open) return false;
  const fold = (p: string) => p.replace(/[\\/]+$/, "").replaceAll("\\", "/").toLowerCase();
  return fold(open) === fold(path);
}

/** Opens `path` in place of the open folder. Whether it did; a failure is shown above the
 *  explorer (`folderError`). */
export async function switchFolder(path: string): Promise<boolean> {
  const app = useApp.getState();
  if (windowContext().kind !== "main") return false;
  // A connection ID can refer to a different server in the next folder: the tabs on screen
  // are kept under this folder and the next folder's own tabs come back (ADR-036).
  if (app.tabs.some(isConnecting) || app.historySaving.length > 0) return false;
  // Changing folder closes every tab's session (ADR-031): ask first, as before closing tabs,
  // about open transactions, running queries and row edits not applied (SDB-140).
  let open: string[] = [];
  try {
    open = await call(commands.sessionsToConfirm(app.tabs.map((t) => t.id)));
  } catch {
    // The sessions are gone: nothing is left to roll back.
  }
  if (!(await confirmLeaving("folder", open))) return false;
  // Saving started while the question was open, or a tab began connecting: nothing changes.
  const now = useApp.getState();
  if (now.tabs.some((t) => savingEdits(t) || isConnecting(t)) || now.historySaving.length > 0) return false;
  app.setWorkspaceChanging(true);
  app.setFolderError(null);
  // Tabs belong to the folder they were opened in (ADR-036): the old folder's are written
  // now, and the new folder's come back once it is open.
  await flushTabs();
  detachTabs();
  let switched = false;
  try {
    const sessions = [
      ...now.tabs.map((t) => t.id),
      ...now.connections.map((c) => explorerSession(c.id)),
    ];
    // The core stops the runs the user agreed to cancel, then the sessions close.
    const running = now.tabs.filter(isRunning).map((t) => t.id);
    app.clearTabConnections();
    app.setConnections([]);
    await Promise.all(running.map((id) => call(commands.cancel(id)).catch(() => undefined)));
    await Promise.all(sessions.map((session) => call(commands.disconnect(session))));
    await Promise.all(now.tabs.map((tab) => call(commands.releaseResults(tab.id))));
    app.setWorkspace(await call(commands.openWorkspace(path)));
    await refreshConnections();
    switched = true;
  } catch (e) {
    app.setFolderError(errorText(e));
    await refreshConnections().catch(() => undefined);
  } finally {
    await restoreTabs();
    app.setWorkspaceChanging(false);
    window.dispatchEvent(new Event(RECENT_FOLDERS_EVENT));
  }
  return switched;
}

/** The recent folders, newest first; none when they cannot be read. */
export async function recentFolders(): Promise<RecentFolderItem[]> {
  try {
    return await call(commands.recentFolders());
  } catch {
    return [];
  }
}

/** Takes a folder out of the list (the folder itself is not touched). */
export async function forgetRecentFolder(path: string): Promise<void> {
  try {
    await call(commands.forgetRecentFolder(path));
  } finally {
    window.dispatchEvent(new Event(RECENT_FOLDERS_EVENT));
  }
}
