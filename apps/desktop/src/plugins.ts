// What the core says about our plugins (ADR-055), shared by the Plugins page, the "add
// connection" picker and a connection that could not open. The UI only shows it: every
// decision (which build starts, what the catalogue offers, what may be installed or removed) is
// the core's, through the commands of `src-tauri/src/plugins.rs`.

import { create } from "zustand";
import { call, commands, events, type PluginItem, type PluginsOverview } from "./api";
import { useApp } from "./store";

interface PluginsState {
  /** The last overview the core gave; null until it is first read. */
  overview: PluginsOverview | null;
  /** Reads it again from this machine (fetches nothing). */
  load(): Promise<void>;
  /** What a command answered with. */
  set(overview: PluginsOverview): void;
}

export const usePlugins = create<PluginsState>((set) => ({
  overview: null,
  load: async () => {
    try {
      set({ overview: await call(commands.pluginsOverview()) });
    } catch {
      // The overview reads only this machine; without it nothing is marked as needing a plugin.
    }
  },
  set: (overview) => set({ overview }),
}));

let listening = false;

/** Reads the overview once, and again whenever the core accepts a catalogue on its own. */
export function watchPlugins() {
  if (listening) return;
  listening = true;
  void usePlugins.getState().load();
  void events.pluginsChanged.listen(() => void usePlugins.getState().load());
}

/**
 * The plugin of ours that serves `engine` when no build of it is started here: the engine
 * needs it installed. Null for an engine compiled into the app, or whose plugin runs.
 */
export function pluginNeeded(overview: PluginsOverview | null, engine: string): PluginItem | null {
  if (!overview) return null;
  return overview.plugins.find((plugin) => plugin.inUse === null && plugin.engines.includes(engine)) ?? null;
}

/** Opens Settings on the Plugins page, closing the "add connection" wizard if it is open. */
export function openPluginsPage() {
  const app = useApp.getState();
  app.setConnectionDialog(null);
  app.openSettings("plugins");
}

/** A size in bytes, for an archive to download. */
export function byteSize(bytes: number, locale: string): string {
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 10 ? 0 : 1;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits, minimumFractionDigits: digits }).format(value)} ${units[unit]}`;
}
