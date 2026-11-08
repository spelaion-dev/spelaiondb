// Native file dialogs. A native dialog cannot be driven by the smoke test (e2e/smoke.mjs),
// so it answers through `window.__spelaionSavePath` / `__spelaionOpenPath`; nothing else sets
// them, and a script able to set them could already call every command directly.

import { type DialogFilter, open, save, type SaveDialogOptions } from "@tauri-apps/plugin-dialog";

declare global {
  interface Window {
    __spelaionSavePath?: string;
    __spelaionOpenPath?: string;
    __spelaionOpenFolder?: string;
  }
}

/** Asks where to save a file; `null` if the user cancelled. */
export async function chooseSavePath(options: SaveDialogOptions): Promise<string | null> {
  return window.__spelaionSavePath ?? (await save(options));
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

/** Asks for a folder; `null` if the user cancelled. */
export async function chooseFolder(title: string): Promise<string | null> {
  if (window.__spelaionOpenFolder) return window.__spelaionOpenFolder;
  const path = await open({ directory: true, multiple: false, title });
  return typeof path === "string" ? path : null;
}

/** The filters of the `.sql` file dialogs. */
export function sqlFileFilters(sqlLabel: string, allLabel: string): DialogFilter[] {
  return [{ name: sqlLabel, extensions: ["sql"] }, { name: allLabel, extensions: ["*"] }];
}

/** Asks for a file to open; `null` if the user cancelled. */
export async function chooseOpenFile(title: string, filters: DialogFilter[]): Promise<string | null> {
  if (window.__spelaionOpenPath) return window.__spelaionOpenPath;
  const path = await open({ multiple: false, directory: false, title, filters: openFilters(filters) });
  return typeof path === "string" ? path : null;
}
