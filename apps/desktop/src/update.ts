// Automatic updates (ADR-021): the core side checks, downloads and verifies; this module keeps
// what the UI shows. Nothing is installed until the user clicks "Install and restart".

import { create } from "zustand";
import { call, commands, type UpdateStatus } from "./api";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

interface UpdateStore {
  /** `null` until the first check (or when the user turned startup checks off). */
  status: UpdateStatus | null;
  /** Check at startup (a personal setting, on by default). */
  checkAtStartup: boolean;
  checking: boolean;
  installing: boolean;
  error: string | null;
  /** The notice was closed with "Later": shown again on the next start. */
  dismissed: boolean;
  notesOpen: boolean;
  start(): Promise<void>;
  check(): Promise<void>;
  install(): Promise<void>;
  setCheckAtStartup(enabled: boolean): Promise<void>;
  dismiss(): void;
  showNotes(): void;
  hideNotes(): void;
}

export const useUpdate = create<UpdateStore>((set, get) => ({
  status: null,
  checkAtStartup: true,
  checking: false,
  installing: false,
  error: null,
  dismissed: false,
  notesOpen: false,
  async start() {
    try {
      const enabled = await call(commands.getUpdateCheck());
      set({ checkAtStartup: enabled });
      // A failed check at startup stays quiet: the notice is for updates, not for the network.
      if (enabled) await get().check();
      set({ error: null });
    } catch {
      set({ error: null });
    }
  },
  async check() {
    if (get().checking || get().installing) return;
    set({ checking: true, error: null });
    try {
      set({ status: await call(commands.updateCheck()), dismissed: false });
    } catch (e) {
      set({ error: errorText(e) });
    } finally {
      set({ checking: false });
    }
  },
  async install() {
    if (get().installing) return;
    set({ installing: true, error: null });
    try {
      // On success the app restarts and this never returns.
      await call(commands.updateInstall());
    } catch (e) {
      set({ error: errorText(e), installing: false });
    }
  },
  async setCheckAtStartup(enabled) {
    try {
      await call(commands.setUpdateCheck(enabled));
      set({ checkAtStartup: enabled, error: null });
    } catch (e) {
      set({ error: errorText(e) });
    }
  },
  dismiss() {
    set({ dismissed: true });
  },
  showNotes() {
    set({ notesOpen: true });
  },
  hideNotes() {
    set({ notesOpen: false });
  },
}));
