// What the core says about our plugins (ADR-055), shared by the Plugins page, the "add
// connection" picker and a connection that could not open. The UI only shows it: every
// decision (which build starts, what the catalogue offers, what may be installed or removed) is
// the core's, through the commands of `src-tauri/src/plugins.rs`.

import { create } from "zustand";
import { call, commands, events, type PluginsOverview } from "./api";
import type { PluginInstall } from "./bindings";
import { useApp } from "./store";

export { byteSize, installable, installText, needsInstalling, pluginNeeded } from "./pluginStatus";

interface PluginsState {
  /** The last overview the core gave; null until it is first read. */
  overview: PluginsOverview | null;
  /** Reads it again from this machine (fetches nothing). */
  load(): Promise<void>;
  /** What a command answered with. */
  set(overview: PluginsOverview): void;
  /** A plugin's install, as the adapter holds it (`pluginInstallChanged`). */
  setInstall(name: string, install: PluginInstall): void;
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
  setInstall: (name, install) =>
    set((state) =>
      state.overview
        ? {
            overview: {
              ...state.overview,
              plugins: state.overview.plugins.map((plugin) => (plugin.name === name ? { ...plugin, install } : plugin)),
            },
          }
        : state,
    ),
}));

let listening = false;

/**
 * Reads the overview once, and again whenever the core accepts a catalogue on its own or an
 * install ends; follows every install, wherever it was started, so each view shows it.
 */
export function watchPlugins() {
  if (listening) return;
  listening = true;
  void usePlugins.getState().load();
  void events.pluginsChanged.listen(() => void usePlugins.getState().load());
  void events.pluginInstallChanged.listen((event) => {
    const { name, install } = event.payload;
    usePlugins.getState().setInstall(name, install);
    // Idle again: what is installed may have changed.
    if (install.kind === "idle") void usePlugins.getState().load();
  });
}

/** Opens Settings on the Plugins page, closing the "add connection" wizard if it is open. */
export function openPluginsPage() {
  const app = useApp.getState();
  app.setConnectionDialog(null);
  app.openSettings("plugins");
}
