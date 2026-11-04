// Native file dialogs. A native dialog cannot be driven by the smoke test (e2e/smoke.mjs),
// so it answers through `window.__spelaionSavePath`; nothing else sets it, and a script able
// to set it could already call every command directly.

import { open, save, type SaveDialogOptions } from "@tauri-apps/plugin-dialog";

declare global {
  interface Window {
    __spelaionSavePath?: string;
    __spelaionOpenFolder?: string;
  }
}

/** Asks where to save a file; `null` if the user cancelled. */
export async function chooseSavePath(options: SaveDialogOptions): Promise<string | null> {
  return window.__spelaionSavePath ?? (await save(options));
}

/** Asks for a folder; `null` if the user cancelled. */
export async function chooseFolder(title: string): Promise<string | null> {
  if (window.__spelaionOpenFolder) return window.__spelaionOpenFolder;
  const path = await open({ directory: true, multiple: false, title });
  return typeof path === "string" ? path : null;
}
