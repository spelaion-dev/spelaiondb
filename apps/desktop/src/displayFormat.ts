// How the grid shows values (SDB-385): the user's date, time and number formats, the display
// time zone and how binary reads. The core keeps and checks them and formats every window it
// serves; this only mirrors what was saved, so Settings can edit it and the grids know when
// to ask for their rows again.

import { create } from "zustand";
import { call, commands } from "./api";
import type { DisplayFormatItem } from "./bindings";

export type DisplayFormat = DisplayFormatItem;

/** Everything as the database prints it: what the core starts with. */
export const AS_STORED: DisplayFormat = {
  asStored: true,
  date: "",
  time: "",
  timestamp: "",
  grouping: "none",
  decimal: "dot",
  timeZone: "",
  binary: "hex",
};

interface DisplayStore {
  format: DisplayFormat;
  /** Changes whenever the core formats windows another way: grids fetch their rows again. */
  revision: number;
  loaded: boolean;
  /** Why the last change was not saved, as the core said it. */
  error: string | null;
  load(): Promise<void>;
  /** Saves the format; the core refuses one it cannot use, and then nothing changes. */
  save(next: DisplayFormat): Promise<boolean>;
  /** Reads it again: the core stored another one, saved in another window or imported
   *  (SDB-535). The grids ask for their rows again when it differs. */
  changedElsewhere(): Promise<void>;
}

export const useDisplayFormat = create<DisplayStore>((set, get) => ({
  format: AS_STORED,
  revision: 0,
  loaded: false,
  error: null,
  async load() {
    if (get().loaded) return;
    try {
      const format = await call(commands.getDisplayFormat());
      set({ format, loaded: true });
    } catch {
      // The grid shows values as the database prints them, which is always right.
      set({ loaded: true });
    }
  },
  async save(next) {
    try {
      await call(commands.setDisplayFormat(next));
      set((s) => ({ format: next, revision: s.revision + 1, error: null }));
      return true;
    } catch (e) {
      set({ error: e instanceof Error ? e.message : String(e) });
      return false;
    }
  },
  async changedElsewhere() {
    try {
      const format = await call(commands.getDisplayFormat());
      if (JSON.stringify(format) !== JSON.stringify(get().format)) set((s) => ({ format, revision: s.revision + 1, loaded: true }));
    } catch {
      // What it shows stays; the core formats every window it serves with what it stored.
    }
  },
}));

/** Whether any value is shown otherwise than the database prints it. */
export function formatsValues(format: DisplayFormat): boolean {
  return !format.asStored && (format.date !== "" || format.time !== "" || format.timestamp !== "" ||
    format.grouping !== "none" || format.decimal !== "dot" || format.timeZone !== "" || format.binary !== "hex");
}
