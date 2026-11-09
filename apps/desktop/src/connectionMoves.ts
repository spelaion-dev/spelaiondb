// Moving connections between folders: drag and drop in the explorer and "Move to folder…" in a
// connection's menu. The core moves the file, its stored password and what repository trust
// pinned for it (a move never makes a changed connection trusted, ADR-022), and its explorer
// settings follow; this only checks it is not in use and points the tabs at its new id.

import { call, commands, type ConnectionItem } from "./api";
import i18n from "./i18n";
import { refreshConnections } from "./components/Sidebar";
import { useApp } from "./store";

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
  const busy = useApp
    .getState()
    .tabs.some((tab) => tab.connectionId === connection.id && (tab.status !== "disconnected" || tab.run?.running));
  return busy ? i18n.t("connections:move.inUse", { name: connection.name }) : null;
}

/** Moves a connection to `folder` and shows it there. Returns its new id. */
export async function moveConnection(connection: ConnectionItem, folder: string): Promise<string> {
  const blocked = moveBlocked(connection);
  if (blocked) throw new Error(blocked);
  const app = useApp.getState();
  // The explorer's own session reads only the catalog: it closes, and expanding reconnects.
  if (app.explorerConnected.includes(connection.id)) {
    await call(commands.disconnect(`meta:${connection.id}`)).catch(() => undefined);
    app.setExplorerConnected(connection.id, false);
  }
  const moved = await call(commands.moveConnection(connection.id, folder));
  if (moved.id !== connection.id) {
    for (const tab of useApp.getState().tabs) {
      if (tab.connectionId === connection.id) app.updateTab(tab.id, { connectionId: moved.id });
    }
    app.setConnectionOpen(connection.id, false);
  }
  await refreshConnections();
  app.revealInExplorer({ connectionId: moved.id });
  return moved.id;
}
