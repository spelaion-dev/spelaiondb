// What a launch from outside asked for (SDB-397): this app's own command line, a second
// launch's, or files opened from macOS's Finder. The core has read and checked it; here it
// is only opened. Nothing connects and nothing runs: a folder opens as from the folder menu
// (repository trust follows, ADR-022), files open as tabs without a connection, and a
// connection is only shown in the explorer.

import i18n from "./i18n";
import type { LaunchRequestItem } from "./bindings";
import { isOpenFolder, switchFolder } from "./folders";
import { openSavedPlan } from "./planFiles";
import { openFilePath } from "./sqlFiles";
import { useApp } from "./store";

/** Opens what `request` asks for, in order: the folder, the files, the plan, the connection. */
export async function openLaunchRequest(request: LaunchRequestItem): Promise<void> {
  const app = useApp.getState();
  const notes: string[] = request.refused.map((refused) => i18n.t("shell:launch.refused", { argument: refused.argument, reason: refused.reason }));
  if (request.folder && !isOpenFolder(request.folder)) {
    // As if chosen in the folder menu: it asks first about what closing the sessions loses.
    const switched = await switchFolder(request.folder);
    if (!switched && !isOpenFolder(request.folder)) {
      notes.push(i18n.t("shell:launch.folderNotOpened", { path: request.folder }));
      // The files and the connection belong to that folder's work: not opened here.
      useApp.getState().setLaunchNotice(notes.join("\n"));
      return;
    }
  }
  for (const path of request.files) {
    // A tab of its own, with no connection: it never connects on its own.
    await openFilePath(path, { connectionId: null });
  }
  if (request.plan) await openSavedPlan(request.plan);
  if (request.connection) {
    const connection = useApp.getState().connections.find((c) => c.id === request.connection);
    if (connection) {
      app.setSidebarSection("connections");
      app.revealInExplorer({ connectionId: connection.id });
    } else {
      notes.push(i18n.t("shell:launch.noConnection", { id: request.connection }));
    }
  }
  if (notes.length > 0) useApp.getState().setLaunchNotice(notes.join("\n"));
}
