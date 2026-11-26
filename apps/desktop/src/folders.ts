// The folders the window holds open (ADR-075, SDB-502): opening one from the folder menu, a
// recent folder, the command palette or a launch adds it beside the others and closes nothing,
// so it asks nothing; its kept tabs come back. Closing one asks first about what closing its
// sessions would lose (ADR-031), as changing folder used to, and closes only that folder's
// sessions and tabs (the core keeps the tabs for when it opens again). Nothing connects or runs,
// and each folder goes through its own repository trust (ADR-022). At most ten are open: the
// core refuses an eleventh with a message. Recent folders are the core's list.

import { call, commands } from "./api";
import { explorerSession, windowContext } from "./windowContext";
import { keyOf } from "./connectionKey";
import type { RecentFolderItem } from "./bindings";
import type { ConnectionItem } from "./api";
import { useMemo } from "react";
import { useTranslation } from "react-i18next";
import { connectionName, folderNames } from "./folderNames";
import { confirmLeaving } from "./connect";
import { useApp, type Tab } from "./store";
import { isConnecting, isRunning, savingEdits } from "./tabState";
import { restoreFolderTabs, whileFoldersChange } from "./tabPersistence";
import { refreshConnections } from "./components/Sidebar";

/** Fired on `window` when the recent folders change. */
export const RECENT_FOLDERS_EVENT = "spelaion-recent-folders";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Whether the open folders cannot change now: they are changing, or history is being saved.
 *  Only the main window changes them (SDB-30). */
export function folderSwitchBlocked(state = useApp.getState()): boolean {
  if (windowContext().kind !== "main") return true;
  return state.workspaceChanging || state.historySaving.length > 0;
}

const fold = (path: string) => path.replace(/[\\/]+$/, "").replaceAll("\\", "/").toLowerCase();

/** Whether `path` is one of the open folders, as it was chosen or as its root. */
export function isOpenFolder(path: string): boolean {
  return useApp.getState().openFolders.some((folder) => fold(folder.path) === fold(path) || fold(folder.root) === fold(path));
}

export { connectionName, folderNames } from "./folderNames";

/** A listed connection's name as pickers and dialogs show it (ADR-075 point 8): with its
 *  folder's when another listed connection has the same name. The same function until the
 *  connections or the open folders change, so it can be a dependency of a memo or an effect. */
export function useConnectionName(): (connection: ConnectionItem) => string {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  const openFolders = useApp((s) => s.openFolders);
  const personal = t("personal");
  return useMemo(() => {
    const names = folderNames(openFolders);
    return (connection: ConnectionItem) => connectionName(connection, connections, names, personal);
  }, [connections, openFolders, personal]);
}

/** Opens `path` beside the open folders; nothing closes and nothing is asked. Its root, as
 *  listed, or null when it could not open (the reason is shown above the explorer,
 *  `folderError`). A folder open already is shown as it is. */
export async function addFolder(path: string): Promise<string | null> {
  const app = useApp.getState();
  if (folderSwitchBlocked(app)) return null;
  app.setWorkspaceChanging(true);
  app.setFolderError(null);
  try {
    return await whileFoldersChange(async () => {
      const added = await call(commands.addFolder(path));
      // The open folders and their connections come together (`refreshConnections`).
      await refreshConnections();
      // Its kept tabs come back among the ones on screen (ADR-075 point 7).
      if (added.added) await restoreFolderTabs(added.root);
      return added.root;
    });
  } catch (e) {
    app.setFolderError(errorText(e));
    await refreshConnections().catch(() => undefined);
    return null;
  } finally {
    app.setWorkspaceChanging(false);
    window.dispatchEvent(new Event(RECENT_FOLDERS_EVENT));
  }
}

/** Closes the open folder `root` (as listed), after asking about what closing its sessions
 *  would lose. Its tabs are kept by the core and leave the tab bar; personal connections'
 *  sessions, tabs with none and other folders' stay. Whether it closed. */
export async function closeFolder(root: string): Promise<boolean> {
  const app = useApp.getState();
  if (folderSwitchBlocked(app)) return false;
  const inFolder = (tab: Tab) => tab.connectionId !== null && tab.connectionFolder === root;
  // A tab opening a session on it could come back connected after the folder closed.
  if (app.tabs.some((t) => inFolder(t) && isConnecting(t))) return false;
  const ids = app.tabs.filter(inFolder).map((t) => t.id);
  let open: string[] = [];
  try {
    open = await call(commands.sessionsToConfirm(ids));
  } catch {
    // The sessions are gone: nothing is left to roll back.
  }
  if (!(await confirmLeaving("folder", open, [], ids))) return false;
  // Saving started while the question was open, or a tab began connecting: nothing changes.
  const now = useApp.getState();
  if (now.tabs.some((t) => inFolder(t) && (savingEdits(t) || isConnecting(t))) || folderSwitchBlocked(now)) return false;
  app.setWorkspaceChanging(true);
  app.setFolderError(null);
  try {
    await whileFoldersChange(async () => {
      const closing = now.tabs.filter(inFolder);
      const connections = now.connections.filter((c) => c.folder === root).map(keyOf);
      // The core stops the runs the user agreed to cancel, then the sessions close.
      await Promise.all(closing.filter(isRunning).map((t) => call(commands.cancel(t.id)).catch(() => undefined)));
      await Promise.all(
        [...closing.map((t) => t.id), ...connections.map(explorerSession)].map((session) => call(commands.disconnect(session)).catch(() => undefined)),
      );
      await Promise.all(closing.map((t) => call(commands.releaseResults(t.id)).catch(() => undefined)));
      // Written under the folder already (`whileFoldersChange`): they come back with it.
      useApp.getState().closeTabs(closing.map((t) => t.id), false);
      for (const key of connections) {
        useApp.getState().setExplorerConnected(key, false);
        useApp.getState().setConnectionOpen(key, false);
      }
      await call(commands.closeFolder(root));
      // The open folders and their connections come together (`refreshConnections`).
      await refreshConnections();
    });
    return true;
  } catch (e) {
    app.setFolderError(errorText(e));
    await refreshConnections().catch(() => undefined);
    return false;
  } finally {
    app.setWorkspaceChanging(false);
  }
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
