// Native file dialogs. A native dialog cannot be driven by the smoke test (e2e/smoke.mjs),
// so it answers through `window.__spelaionSavePath` / `__spelaionOpenPath`; nothing else sets
// them, and a script able to set them could already call every command directly.
//
// On Linux the app opens GTK's file chooser itself (`choose_path`, src-tauri/src/file_dialog.rs):
// the dialog plugin gives it no parent window there, so a tiling Wayland compositor does not
// float it over the app, and GTK leaves a saved name's extension out of step with the chosen
// file type. Windows and macOS keep the plugin's dialogs.

import { type DialogFilter, open, save, type SaveDialogOptions } from "@tauri-apps/plugin-dialog";
import { call, commands } from "./api";
import { folderOf, savePlace, startFolders } from "./dialogPaths";
import { useApp } from "./store";

declare global {
  interface Window {
    __spelaionSavePath?: string;
    __spelaionOpenPath?: string;
    __spelaionOpenFolder?: string;
  }
}

const linuxDialogs = /Linux/.test(navigator.userAgent) && !/Android/.test(navigator.userAgent);

/** The open workspace's folder, where dialogs start when nothing nearer is known. */
function workspaceFolder(): string | null {
  return useApp.getState().workspace?.workspace ?? null;
}

/** Asks where to save a file; `null` if the user cancelled. On Linux it starts in the folder of
 *  `defaultPath` (a bare name: the workspace's), and the name gets the chosen type's extension. */
export async function chooseSavePath(options: SaveDialogOptions): Promise<string | null> {
  if (window.__spelaionSavePath != null) return window.__spelaionSavePath;
  if (linuxDialogs) {
    const { folders, fileName } = savePlace(options.defaultPath, workspaceFolder());
    return call(commands.choosePath({ kind: "save", title: options.title ?? null, folders, fileName, filters: options.filters ?? [] }));
  }
  return save(options);
}

/** GTK (Linux, BSD) turns each filter extension into a case-sensitive `*.ext` pattern: an
 *  "All files" filter (`*`) becomes `*.*`, which hides files without an extension, and `db`
 *  hides `DATA.DB`. */
const gtkDialogs = /Linux|BSD/.test(navigator.userAgent) && !/Android/.test(navigator.userAgent);

/** The filters of an open dialog that offers "All files": none under GTK, where no filter is
 *  the only way to show every file. */
export function openFilters(filters: DialogFilter[]): DialogFilter[] | undefined {
  return gtkDialogs ? undefined : filters;
}

/** Asks for a folder; `null` if the user cancelled. On Linux it starts in `near` itself, else
 *  in the workspace. */
export async function chooseFolder(title: string, near?: string | null): Promise<string | null> {
  if (window.__spelaionOpenFolder) return window.__spelaionOpenFolder;
  if (linuxDialogs) {
    const folders = startFolders(null, workspaceFolder());
    return call(commands.choosePath({ kind: "folder", title, folders: near ? [near, ...folders] : folders, fileName: null, filters: [] }));
  }
  const path = await open({ directory: true, multiple: false, title });
  return typeof path === "string" ? path : null;
}

/** The workspace's parent folder: where "open folder" starts, among the workspace's siblings. */
export function workspaceParent(): string | null {
  const workspace = workspaceFolder();
  return workspace ? folderOf(workspace) : null;
}

/** The filters of the `.sql` file dialogs. */
export function sqlFileFilters(sqlLabel: string, allLabel: string): DialogFilter[] {
  return [{ name: sqlLabel, extensions: ["sql"] }, { name: allLabel, extensions: ["*"] }];
}

/** Asks for a file to open; `null` if the user cancelled. On Linux it starts in the folder of
 *  `near` (the file the dialog is about, such as the field's current value), else in the
 *  workspace. */
export async function chooseOpenFile(title: string | undefined, filters: DialogFilter[], near?: string | null): Promise<string | null> {
  if (window.__spelaionOpenPath) return window.__spelaionOpenPath;
  if (linuxDialogs) {
    const folders = startFolders(near, workspaceFolder());
    return call(commands.choosePath({ kind: "open", title: title ?? null, folders, fileName: null, filters: openFilters(filters) ?? [] }));
  }
  const path = await open({ multiple: false, directory: false, title, filters: openFilters(filters) });
  return typeof path === "string" ? path : null;
}
