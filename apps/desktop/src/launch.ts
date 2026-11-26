// What a launch from outside asked for (SDB-397): this app's own command line, a second
// launch's, or files opened from macOS's Finder. The core has read and checked it; here it
// is only opened. Nothing connects and nothing runs: a folder is added beside the open ones,
// closing nothing and asking nothing (ADR-075; repository trust follows, ADR-022), files open
// as tabs without a connection, and a connection is only shown in the explorer.

import i18n from "./i18n";
import type { ConnectionItem } from "./api";
import type { LaunchRequestItem } from "./bindings";
import { addFolder, isOpenFolder } from "./folders";
import { openSavedPlan } from "./planFiles";
import { openFilePath } from "./sqlFiles";
import { useApp } from "./store";
import { keyOf } from "./connectionKey";

/** The connection `id` a launch names: the launched folder's (`root`), else a personal one,
 *  else the only open folder's of that id. Never a guess between two folders' (ADR-075). */
function launchedConnection(connections: ConnectionItem[], id: string, root: string | null): ConnectionItem | undefined {
  const named = connections.filter((c) => c.id === id);
  return named.find((c) => root !== null && c.folder === root) ?? named.find((c) => c.folder === null) ?? (named.length === 1 ? named[0] : undefined);
}

/** Opens what `request` asks for, in order: the folder, the files, the plan, the connection. */
export async function openLaunchRequest(request: LaunchRequestItem): Promise<void> {
  const app = useApp.getState();
  const notes: string[] = request.refused.map((refused) => i18n.t("shell:launch.refused", { argument: refused.argument, reason: refused.reason }));
  let root: string | null = null;
  if (request.folder) {
    // Added beside the open folders (or found open already): nothing closes, nothing is asked.
    root = await addFolder(request.folder);
    if (root === null && !isOpenFolder(request.folder)) {
      notes.push(i18n.t("shell:launch.folderNotOpened", { path: request.folder }));
      // The files and the connection belong to that folder's work: not opened here.
      useApp.getState().setLaunchNotice(notes.join("\n"));
      return;
    }
  }
  for (const path of request.files) {
    // A tab of its own, with no connection: it never connects on its own.
    await openFilePath(path, { connection: null });
  }
  if (request.plan) await openSavedPlan(request.plan);
  if (request.connection) {
    const connection = launchedConnection(useApp.getState().connections, request.connection, root);
    if (connection) {
      app.setSidebarSection("connections");
      app.revealInExplorer({ connection: keyOf(connection) });
    } else {
      notes.push(i18n.t("shell:launch.noConnection", { id: request.connection }));
    }
  }
  if (notes.length > 0) useApp.getState().setLaunchNotice(notes.join("\n"));
}
