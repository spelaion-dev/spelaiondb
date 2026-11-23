// The user's changes to the keyboard shortcuts as the page holds them (SDB-447, ADR-064): a
// personal preference the core keeps and checks, only the differences from the registry's
// defaults. Applying them sets the registry's effective keys, which every handler, the SQL
// editor's keymap, the menus, the palette and the tooltips read, and re-renders whatever shows
// a shortcut.

import { create } from "zustand";
import { call, commands } from "./api";
import { setShortcutOverrides, shortcutOverrides, type ShortcutChange } from "./commandRegistry";
import i18n, { SHORTCUTS_CHANGED } from "./i18n";

interface ShortcutPreferences {
  /** Changes on every change applied: the SQL editor rebuilds its keymap on it. */
  revision: number;
  /** Why the changes could not be read, if they could not: the defaults are in effect. */
  loadError: string | null;
  load(): Promise<void>;
  /** Saves `changes` in place of the current ones (the core checks them), then applies them;
   *  rejects with the core's reason, and nothing changes. */
  save(changes: ShortcutChange[]): Promise<void>;
}

function apply(changes: readonly ShortcutChange[]): void {
  setShortcutOverrides(changes);
  i18n.emit(SHORTCUTS_CHANGED);
}

export const useShortcuts = create<ShortcutPreferences>((set, get) => ({
  revision: 0,
  loadError: null,
  async load() {
    try {
      const stored = await call(commands.shortcutOverrides());
      apply(stored.map((change) => ({ command: change.command, keys: change.keys ?? null })));
      set({ revision: get().revision + 1, loadError: null });
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
    }
  },
  async save(changes) {
    await call(commands.setShortcutOverrides(changes.map((change) => ({ command: change.command, keys: change.keys }))));
    apply(changes);
    set({ revision: get().revision + 1 });
  },
}));

/** The user's changes in effect now. */
export function currentShortcutChanges(): ShortcutChange[] {
  return shortcutOverrides();
}
