// Native file dialogs. A native dialog cannot be driven by the smoke test (e2e/smoke.mjs),
// so it answers through `window.__spelunkSavePath`; nothing else sets it, and a script able
// to set it could already call every command directly.

import { open, save, type SaveDialogOptions } from "@tauri-apps/plugin-dialog";

declare global {
  interface Window {
    __spelunkSavePath?: string;
    __spelunkOpenFolder?: string;
  }
}

/** Asks where to save a file; `null` if the user cancelled. */
export async function chooseSavePath(options: SaveDialogOptions): Promise<string | null> {
  return window.__spelunkSavePath ?? (await save(options));
}

/** Asks for a folder; `null` if the user cancelled. */
export async function chooseFolder(title: string): Promise<string | null> {
  if (window.__spelunkOpenFolder) return window.__spelunkOpenFolder;
  const path = await open({ directory: true, multiple: false, title });
  return typeof path === "string" ? path : null;
}
