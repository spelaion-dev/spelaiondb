// Personal interface preferences (theme, language, the explorer and the result grid). They are stored by the core in the
// personal state.db; this module only applies them to the document and keeps the UI state.

import { create } from "zustand";
import { call, commands, type ExplorerPreferences, type GridPreferences, type ScriptPreferencesItem } from "./api";
import type { EditorPreferencesItem } from "./bindings";
import { applyLanguage, type LanguagePreference } from "./i18n";

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

export type PreferenceSetting = "language" | "theme" | "explorer" | "grid" | "script" | "editor";

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
  /** Connect the restored tabs at startup (ADR-036): connect only, never run. Off by default. */
  reconnectOnStart: boolean;
  loading: boolean;
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

export const usePreferences = create<PreferencesStore>((set, get) => ({
  language: "system",
  theme: "system",
  explorer: { showSystem: false, sortAlphabetically: true, sortIgnoreCase: true, foldersFirst: false, connectedOnly: false, serverSideFilters: true, linkWithEditor: false },
  grid: { zebra: true, nullText: true, boolCheckboxes: false },
  script: { onError: "stop", notify: false, notifyAfterSeconds: 30, confirmDelaySeconds: 0 },
  editor: { font: "system", fontSize: 13, lineHeight: 18, tabSize: 2, lineNumbers: true, wordWrap: false, keywordCase: "preserve", indent: 2, commaPosition: "trailing" },
  reconnectOnStart: false,
  loading: true,
  saving: null,
  error: null,
  async load() {
    const [language, theme, explorer, grid, script, reconnect, editor] = await Promise.allSettled([
      call(commands.getLanguage()),
      call(commands.getTheme()),
      call(commands.getExplorerPreferences()),
      call(commands.getGridPreferences()),
      call(commands.getScriptPreferences()),
      call(commands.getReconnectOnStart()),
      call(commands.getEditorPreferences()),
    ]);
    let error: PreferenceError | null = null;
    if (editor.status === "fulfilled") set({ editor: editor.value });
    else error = { setting: "editor", phase: "load", detail: errorText(editor.reason) };
    if (reconnect.status === "fulfilled") set({ reconnectOnStart: reconnect.value });
    else error = { setting: "editor", phase: "load", detail: errorText(reconnect.reason) };
    if (script.status === "fulfilled") set({ script: script.value });
    else error = { setting: "script", phase: "load", detail: errorText(script.reason) };
    if (grid.status === "fulfilled") set({ grid: grid.value });
    else error = { setting: "grid", phase: "load", detail: errorText(grid.reason) };
    if (explorer.status === "fulfilled") set({ explorer: explorer.value });
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
  async setLanguage(next) {
    if (get().saving) return;
    set({ saving: "language", error: null });
    try {
      await call(commands.setLanguage(next));
      set({ language: next });
      await applyLanguage(next);
    } catch (e) {
      set({ error: { setting: "language", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
  },
  async setTheme(next) {
    if (get().saving) return;
    set({ saving: "theme", error: null });
    try {
      await call(commands.setTheme(next));
      set({ theme: next });
      applyTheme(next);
    } catch (e) {
      set({ error: { setting: "theme", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
  },
  async setExplorer(next) {
    if (get().saving) return;
    // The tree follows at once; if saving fails it goes back to what was saved.
    const previous = get().explorer;
    set({ explorer: next, saving: "explorer", error: null });
    try {
      await call(commands.setExplorerPreferences(next));
    } catch (e) {
      set({ explorer: previous, error: { setting: "explorer", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
  },
  async setScript(next) {
    if (get().saving) return;
    const previous = get().script;
    set({ script: next, saving: "script", error: null });
    try {
      await call(commands.setScriptPreferences(next));
    } catch (e) {
      set({ script: previous, error: { setting: "script", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
  },
  async setEditor(next) {
    if (get().saving) return false;
    const previous = get().editor;
    set({ editor: next, saving: "editor", error: null });
    try { await call(commands.setEditorPreferences(next)); return true; }
    catch (e) { set({ editor: previous, error: { setting: "editor", phase: "save", detail: errorText(e) } }); return false; }
    finally { set({ saving: null }); }
  },
  async setReconnectOnStart(next) {
    if (get().saving) return;
    const previous = get().reconnectOnStart;
    set({ reconnectOnStart: next, saving: "editor", error: null });
    try {
      await call(commands.setReconnectOnStart(next));
    } catch (e) {
      set({ reconnectOnStart: previous, error: { setting: "editor", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
  },
  async setGrid(next) {
    if (get().saving) return;
    // The grid repaints at once; if saving fails it goes back to what was saved.
    const previous = get().grid;
    set({ grid: next, saving: "grid", error: null });
    try {
      await call(commands.setGridPreferences(next));
    } catch (e) {
      set({ grid: previous, error: { setting: "grid", phase: "save", detail: errorText(e) } });
    } finally {
      set({ saving: null });
    }
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
