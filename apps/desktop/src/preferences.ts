// Personal interface preferences (theme, language, the explorer and the result grid). They are stored by the core in the
// personal state.db; this module only applies them to the document and keeps the UI state.

import { create } from "zustand";
import { call, commands, type ExplorerPreferences, type GridPreferences, type ScriptPreferencesItem } from "./api";
import type { EditorPreferencesItem, MapPreferencesItem } from "./bindings";
import { applyLanguage, type LanguagePreference } from "./i18n";
import { createSaveQueue, savedSetting } from "./saveQueue";

export type ThemePreference = "system" | "light" | "dark";

/** Fired on `window` whenever the effective theme changes (the grid repaints its canvas). */
export const THEME_CHANGE_EVENT = "spelaion-themechange";

const darkScheme = window.matchMedia("(prefers-color-scheme: dark)");

function effectiveTheme(preference: ThemePreference): "light" | "dark" {
  if (preference === "system") return darkScheme.matches ? "dark" : "light";
  return preference;
}

/** Sets `data-theme` on <html> to `light` or `dark`; CSS variables follow it. */
export function applyTheme(preference: ThemePreference): void {
  const theme = effectiveTheme(preference);
  if (document.documentElement.dataset.theme === theme) return;
  document.documentElement.dataset.theme = theme;
  window.dispatchEvent(new Event(THEME_CHANGE_EVENT));
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export type PreferenceSetting = "language" | "theme" | "explorer" | "grid" | "script" | "editor" | "map";

export interface PreferenceError {
  setting: PreferenceSetting;
  phase: "load" | "save";
  detail: string;
}

interface PreferencesStore {
  language: LanguagePreference;
  theme: ThemePreference;
  /** How the explorer shows its tree. */
  explorer: ExplorerPreferences;
  /** How the result grid draws its cells. */
  grid: GridPreferences;
  /** How scripts run: the default error mode and the opt-in notification (ADR-035). */
  script: ScriptPreferencesItem;
  /** Display and formatting preferences applied to every mounted editor. */
  editor: EditorPreferencesItem;
  setEditor(next: EditorPreferencesItem): Promise<boolean>;
  /** The map's background (ADR-045): tiles from the user's server, off by default. */
  map: MapPreferencesItem;
  /** Saves the map's preferences; `false` when the core refused them (the error says why).
   *  `optimistic` shows the change at once and takes it back if saving fails (a checkbox). */
  setMap(next: MapPreferencesItem, optimistic?: boolean): Promise<boolean>;
  /** Connect the restored tabs at startup (ADR-036): connect only, never run. Off by default. */
  reconnectOnStart: boolean;
  loading: boolean;
  /** The setting being saved, `null` when nothing is. Changes made meanwhile wait for it and
   *  are saved after it, in order: none is dropped (SDB-83). */
  saving: PreferenceSetting | null;
  error: PreferenceError | null;
  load(): Promise<void>;
  setLanguage(next: LanguagePreference): Promise<void>;
  setTheme(next: ThemePreference): Promise<void>;
  setExplorer(next: ExplorerPreferences): Promise<void>;
  setGrid(next: GridPreferences): Promise<void>;
  setScript(next: ScriptPreferencesItem): Promise<void>;
  setReconnectOnStart(next: boolean): Promise<void>;
  clearError(): void;
}

const DEFAULT_EXPLORER: ExplorerPreferences = { showSystem: false, sortAlphabetically: true, sortIgnoreCase: true, foldersFirst: false, connectedOnly: false, serverSideFilters: true, linkWithEditor: false };
const DEFAULT_GRID: GridPreferences = { zebra: true, nullText: true, boolCheckboxes: false };
const DEFAULT_SCRIPT: ScriptPreferencesItem = { onError: "stop", notify: false, notifyAfterSeconds: 30, confirmDelaySeconds: 0 };
const DEFAULT_EDITOR: EditorPreferencesItem = { font: "system", fontSize: 13, lineHeight: 18, tabSize: 2, lineNumbers: true, wordWrap: false, keywordCase: "preserve", indent: 2, commaPosition: "trailing" };
const DEFAULT_MAP: MapPreferencesItem = { tiles: false, tileName: "", tileUrl: "", tileAttribution: "" };

// Every save goes through one queue. The settings shown at once keep what was last saved, to go
// back to it when their newest change fails to save.
const enqueue = createSaveQueue<PreferenceSetting>((saving) => usePreferences.setState({ saving }));
const saved = {
  explorer: savedSetting(DEFAULT_EXPLORER),
  grid: savedSetting(DEFAULT_GRID),
  script: savedSetting(DEFAULT_SCRIPT),
  editor: savedSetting(DEFAULT_EDITOR),
  map: savedSetting(DEFAULT_MAP),
  reconnectOnStart: savedSetting(false),
};

export const usePreferences = create<PreferencesStore>((set) => ({
  language: "system",
  theme: "system",
  explorer: DEFAULT_EXPLORER,
  grid: DEFAULT_GRID,
  script: DEFAULT_SCRIPT,
  editor: DEFAULT_EDITOR,
  map: DEFAULT_MAP,
  reconnectOnStart: false,
  loading: true,
  saving: null,
  error: null,
  async load() {
    const [language, theme, explorer, grid, script, reconnect, editor, map] = await Promise.allSettled([
      call(commands.getLanguage()),
      call(commands.getTheme()),
      call(commands.getExplorerPreferences()),
      call(commands.getGridPreferences()),
      call(commands.getScriptPreferences()),
      call(commands.getReconnectOnStart()),
      call(commands.getEditorPreferences()),
      call(commands.getMapPreferences()),
    ]);
    let error: PreferenceError | null = null;
    if (map.status === "fulfilled") { saved.map.loaded(map.value); set({ map: map.value }); }
    else error = { setting: "map", phase: "load", detail: errorText(map.reason) };
    if (editor.status === "fulfilled") { saved.editor.loaded(editor.value); set({ editor: editor.value }); }
    else error = { setting: "editor", phase: "load", detail: errorText(editor.reason) };
    if (reconnect.status === "fulfilled") { saved.reconnectOnStart.loaded(reconnect.value); set({ reconnectOnStart: reconnect.value }); }
    else error = { setting: "editor", phase: "load", detail: errorText(reconnect.reason) };
    if (script.status === "fulfilled") { saved.script.loaded(script.value); set({ script: script.value }); }
    else error = { setting: "script", phase: "load", detail: errorText(script.reason) };
    if (grid.status === "fulfilled") { saved.grid.loaded(grid.value); set({ grid: grid.value }); }
    else error = { setting: "grid", phase: "load", detail: errorText(grid.reason) };
    if (explorer.status === "fulfilled") { saved.explorer.loaded(explorer.value); set({ explorer: explorer.value }); }
    else error = { setting: "explorer", phase: "load", detail: errorText(explorer.reason) };
    if (theme.status === "fulfilled") {
      const selected: ThemePreference = theme.value === "light" || theme.value === "dark" ? theme.value : "system";
      set({ theme: selected });
      applyTheme(selected);
    } else {
      error = { setting: "theme", phase: "load", detail: errorText(theme.reason) };
    }
    if (language.status === "fulfilled") {
      const selected: LanguagePreference = language.value === "en" || language.value === "es" ? language.value : "system";
      set({ language: selected });
      await applyLanguage(selected);
    } else {
      error = { setting: "language", phase: "load", detail: errorText(language.reason) };
    }
    set({ loading: false, error });
  },
  // Language and theme apply once saved; the others show at once and go back if saving fails.
  setLanguage(next) {
    set({ error: null });
    return enqueue("language", async () => {
      try {
        await call(commands.setLanguage(next));
        set({ language: next });
        await applyLanguage(next);
      } catch (e) {
        set({ error: { setting: "language", phase: "save", detail: errorText(e) } });
      }
    });
  },
  setTheme(next) {
    set({ error: null });
    return enqueue("theme", async () => {
      try {
        await call(commands.setTheme(next));
        set({ theme: next });
        applyTheme(next);
      } catch (e) {
        set({ error: { setting: "theme", phase: "save", detail: errorText(e) } });
      }
    });
  },
  setExplorer(next) {
    // The tree follows at once.
    const ticket = saved.explorer.change();
    set({ explorer: next, error: null });
    return enqueue("explorer", async () => {
      try {
        await call(commands.setExplorerPreferences(next));
        saved.explorer.succeeded(next);
      } catch (e) {
        const back = saved.explorer.failed(ticket);
        set({ ...(back ? { explorer: back } : {}), error: { setting: "explorer", phase: "save", detail: errorText(e) } });
      }
    });
  },
  setScript(next) {
    const ticket = saved.script.change();
    set({ script: next, error: null });
    return enqueue("script", async () => {
      try {
        await call(commands.setScriptPreferences(next));
        saved.script.succeeded(next);
      } catch (e) {
        const back = saved.script.failed(ticket);
        set({ ...(back ? { script: back } : {}), error: { setting: "script", phase: "save", detail: errorText(e) } });
      }
    });
  },
  setEditor(next) {
    const ticket = saved.editor.change();
    set({ editor: next, error: null });
    return enqueue("editor", async () => {
      try {
        await call(commands.setEditorPreferences(next));
        saved.editor.succeeded(next);
        return true;
      } catch (e) {
        const back = saved.editor.failed(ticket);
        set({ ...(back ? { editor: back } : {}), error: { setting: "editor", phase: "save", detail: errorText(e) } });
        return false;
      }
    });
  },
  setMap(next, optimistic = false) {
    // A form's values show once the core accepted them; a checkbox shows at once.
    const ticket = saved.map.change();
    set({ error: null, ...(optimistic ? { map: next } : {}) });
    return enqueue("map", async () => {
      try {
        await call(commands.setMapPreferences(next));
        saved.map.succeeded(next);
        set({ map: next });
        return true;
      } catch (e) {
        const back = optimistic ? saved.map.failed(ticket) : undefined;
        set({ ...(back ? { map: back } : {}), error: { setting: "map", phase: "save", detail: errorText(e) } });
        return false;
      }
    });
  },
  setReconnectOnStart(next) {
    const ticket = saved.reconnectOnStart.change();
    set({ reconnectOnStart: next, error: null });
    return enqueue("editor", async () => {
      try {
        await call(commands.setReconnectOnStart(next));
        saved.reconnectOnStart.succeeded(next);
      } catch (e) {
        const back = saved.reconnectOnStart.failed(ticket);
        set({ ...(back !== undefined ? { reconnectOnStart: back } : {}), error: { setting: "editor", phase: "save", detail: errorText(e) } });
      }
    });
  },
  setGrid(next) {
    // The grid repaints at once.
    const ticket = saved.grid.change();
    set({ grid: next, error: null });
    return enqueue("grid", async () => {
      try {
        await call(commands.setGridPreferences(next));
        saved.grid.succeeded(next);
      } catch (e) {
        const back = saved.grid.failed(ticket);
        set({ ...(back ? { grid: back } : {}), error: { setting: "grid", phase: "save", detail: errorText(e) } });
      }
    });
  },
  clearError: () => set({ error: null }),
}));

// Preferences set to "system" follow the operating system while the app is open.
darkScheme.addEventListener("change", () => applyTheme(usePreferences.getState().theme));
window.addEventListener("languagechange", () => {
  const { language } = usePreferences.getState();
  if (language === "system") void applyLanguage("system");
});
applyTheme("system");
