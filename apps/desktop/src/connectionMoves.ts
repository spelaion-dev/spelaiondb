// Moving connections between folders: drag and drop in the explorer and "Move to folder…" in a
// connection's menu. The core moves the file, its stored password and what repository trust
// pinned for it (a move never makes a changed connection trusted, ADR-022), and its explorer
// settings follow; this only checks it is not in use and points the tabs at its new id.

import { call, commands, type ConnectionItem } from "./api";
import { explorerSession } from "./windowContext";
import { keyOf, tabOnConnection } from "./connectionKey";
import i18n from "./i18n";
import { refreshConnections } from "./components/Sidebar";
import { useApp } from "./store";
import { connectionInUse, explorerSessionOpen } from "./tabState";

/** A connection's folder path as the core takes it: `prod/eu`, or `""` for none. */
export function folderOf(connection: ConnectionItem): string {
  return connection.folders.join("/");
}

/** Every folder of a layer, nested ones with their parents (`prod`, `prod/eu`), sorted. */
export function foldersOf(connections: ConnectionItem[], personal: boolean): string[] {
  const folders = new Set<string>();
  for (const c of connections) {
    if (c.personal !== personal) continue;
    for (let i = 1; i <= c.folders.length; i++) folders.add(c.folders.slice(0, i).join("/"));
  }
  return [...folders].sort((a, b) => a.localeCompare(b));
}

/** Why the connection cannot move now, or null: a tab connected to it must disconnect first. */
export function moveBlocked(connection: ConnectionItem): string | null {
  return connectionInUse(useApp.getState().tabs, keyOf(connection)) ? i18n.t("connections:move.inUse", { name: connection.name }) : null;
}

/** Moves a connection to `folder` and shows it there. Returns its new id. */
export async function moveConnection(connection: ConnectionItem, folder: string): Promise<string> {
  const blocked = moveBlocked(connection);
  if (blocked) throw new Error(blocked);
  const app = useApp.getState();
  // The explorer's own session reads only the catalog: it closes, and expanding reconnects.
  const key = keyOf(connection);
  if (explorerSessionOpen(app.explorerConnected, key)) {
    await call(commands.disconnect(explorerSession(key))).catch(() => undefined);
    app.setExplorerConnected(key, false);
  }
  const moved = await call(commands.moveConnection(key, folder));
  if (moved.id !== connection.id) {
    for (const tab of useApp.getState().tabs) {
      if (tabOnConnection(tab, key)) app.updateTab(tab.id, { connectionId: moved.id });
    }
    app.setConnectionOpen(key, false);
  }
  await refreshConnections();
  // A connection moves within its layer: its folder stays, only its id changes.
  app.revealInExplorer({ connection: { folder: key.folder, id: moved.id } });
  return moved.id;
}
