// Personal interface preferences (theme, language, the explorer and the result grid). They are stored by the core in the
// personal state.db; this module only applies them to the document and keeps the UI state.

import { create } from "zustand";
import { call, commands, type ExplorerPreferences, type GridPreferences, type ScriptPreferencesItem } from "./api";
import type { EditorPreferencesItem, MapPreferencesItem, SidePanelItem, SizeStepsItem } from "./bindings";
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

/**
 * Calls `onChange` each time the page's device pixel ratio changes: the interface size (the
 * webview's zoom, SDB-389) or a monitor of another scale. Canvases drawn at that ratio repaint
 * on it. Returns the function that stops watching.
 */
export function onPixelRatioChange(onChange: () => void): () => void {
  let query: MediaQueryList | null = null;
  const watch = () => {
    query = window.matchMedia(`(resolution: ${window.devicePixelRatio || 1}dppx)`);
    query.addEventListener("change", changed, { once: true });
  };
  const changed = () => {
    watch();
    onChange();
  };
  watch();
  return () => query?.removeEventListener("change", changed);
}

/** The zoom step after `current` in `steps` (`direction` 1) or before it (-1); `current` when
 *  there is none that way. A value between two steps goes to the nearest one that way. */
export function nextStep(steps: readonly number[], current: number, direction: 1 | -1): number {
  if (direction === 1) return steps.find((step) => step > current) ?? current;
  return [...steps].reverse().find((step) => step < current) ?? current;
}

/** A size in pixels at the editor's zoom (SDB-389), whole pixels. */
export function zoomed(size: number, zoom: number): number {
  return Math.round((size * zoom) / 100);
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export type PreferenceSetting = "language" | "theme" | "interfaceSize" | "explorer" | "grid" | "script" | "editor" | "map" | "sidePanel";

export interface PreferenceError {
  setting: PreferenceSetting;
  phase: "load" | "save";
  detail: string;
}

interface PreferencesStore {
  language: LanguagePreference;
  theme: ThemePreference;
  /** The interface size in percent (SDB-389): the whole window is drawn at it, by the
   *  webview's zoom, so the sizes of every other setting are at the normal size and grow
   *  with it. */
  interfaceSize: number;
  /** The steps the interface size and the editor's zoom move through, as the core lists them. */
  steps: SizeStepsItem;
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
  /** The side panel as it was left (SDB-451): hidden or not, its width, and the widths the
   *  core allows. Read once at startup; the page saves it as it changes. */
  sidePanel: SidePanelItem;
  /** Saves whether the side panel is hidden and its width (the core refuses one out of range). */
  setSidePanel(hidden: boolean, width: number): Promise<void>;
  loading: boolean;
  /** The setting being saved, `null` when nothing is. Changes made meanwhile wait for it and
   *  are saved after it, in order: none is dropped (SDB-83). */
  saving: PreferenceSetting | null;
  error: PreferenceError | null;
  load(): Promise<void>;
  setLanguage(next: LanguagePreference): Promise<void>;
  setTheme(next: ThemePreference): Promise<void>;
  setInterfaceSize(next: number): Promise<void>;
  /** Moves the editor's zoom one step in (1) or out (-1), or back to 100 (0); remembered. */
  zoomEditor(direction: 1 | -1 | 0): Promise<boolean>;
  setExplorer(next: ExplorerPreferences): Promise<void>;
  setGrid(next: GridPreferences): Promise<void>;
  setScript(next: ScriptPreferencesItem): Promise<void>;
  setReconnectOnStart(next: boolean): Promise<void>;
  clearError(): void;
}

const DEFAULT_EXPLORER: ExplorerPreferences = { showSystem: false, sortAlphabetically: true, sortIgnoreCase: true, foldersFirst: false, connectedOnly: false, serverSideFilters: true, linkWithEditor: false };
const DEFAULT_GRID: GridPreferences = { zebra: true, nullText: true, boolCheckboxes: false, resultTabs: 20, colorize: false, headerType: true, headerComment: false, fontSize: 12, viewerWrap: true };
const DEFAULT_SCRIPT: ScriptPreferencesItem = { onError: "stop", notify: false, notifyAfterSeconds: 30, confirmDelaySeconds: 0, statementSavepoints: false };
const DEFAULT_EDITOR: EditorPreferencesItem = { font: "system", fontSize: 13, lineHeight: 18, zoom: 100, tabSize: 2, lineNumbers: true, wordWrap: false, keywordCase: "preserve", indent: 2, commaPosition: "trailing" };
// Only until the core's lists arrive: the same values, and nothing is offered meanwhile
// that the core would refuse (the size and zoom are sent as one of these).
const DEFAULT_STEPS: SizeStepsItem = { interfaceSizes: [90, 100, 110, 125, 150], defaultInterfaceSize: 100, editorZooms: [50, 67, 75, 80, 90, 100, 110, 125, 150, 175, 200], defaultEditorZoom: 100 };
const DEFAULT_MAP: MapPreferencesItem = { tiles: false, tileName: "", tileUrl: "", tileAttribution: "", tileAttributionUrl: "" };
// Only until the core's answer arrives: its defaults and limits.
const DEFAULT_SIDE_PANEL: SidePanelItem = { hidden: false, width: 290, minWidth: 210, maxWidth: 700, defaultWidth: 290 };

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

export const usePreferences = create<PreferencesStore>((set, get) => ({
  language: "system",
  theme: "system",
  interfaceSize: 100,
  steps: DEFAULT_STEPS,
  explorer: DEFAULT_EXPLORER,
  grid: DEFAULT_GRID,
  script: DEFAULT_SCRIPT,
  editor: DEFAULT_EDITOR,
  map: DEFAULT_MAP,
  reconnectOnStart: false,
  sidePanel: DEFAULT_SIDE_PANEL,
  loading: true,
  saving: null,
  error: null,
  async load() {
    const [language, theme, explorer, grid, script, reconnect, editor, map, interfaceSize, steps, sidePanel] = await Promise.allSettled([
      call(commands.getLanguage()),
      call(commands.getTheme()),
      call(commands.getExplorerPreferences()),
      call(commands.getGridPreferences()),
      call(commands.getScriptPreferences()),
      call(commands.getReconnectOnStart()),
      call(commands.getEditorPreferences()),
      call(commands.getMapPreferences()),
      call(commands.getInterfaceSize()),
      commands.sizeSteps(),
      call(commands.getSidePanel()),
    ]);
    let error: PreferenceError | null = null;
    if (steps.status === "fulfilled") set({ steps: steps.value });
    if (sidePanel.status === "fulfilled") set({ sidePanel: sidePanel.value });
    else error = { setting: "sidePanel", phase: "load", detail: errorText(sidePanel.reason) };
    if (interfaceSize.status === "fulfilled") set({ interfaceSize: interfaceSize.value });
    else error = { setting: "interfaceSize", phase: "load", detail: errorText(interfaceSize.reason) };
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
  setInterfaceSize(next) {
    // The core saves it and draws the window at it; the page follows once it has.
    set({ error: null });
    return enqueue("interfaceSize", async () => {
      try {
        await call(commands.setInterfaceSize(next));
        set({ interfaceSize: next });
      } catch (e) {
        set({ error: { setting: "interfaceSize", phase: "save", detail: errorText(e) } });
      }
    });
  },
  zoomEditor(direction) {
    const { editor, steps, setEditor } = get();
    const zoom = direction === 0 ? steps.defaultEditorZoom : nextStep(steps.editorZooms, editor.zoom, direction);
    if (zoom === editor.zoom) return Promise.resolve(true);
    return setEditor({ ...editor, zoom });
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
  setSidePanel(hidden, width) {
    set((s) => ({ sidePanel: { ...s.sidePanel, hidden, width }, error: null }));
    return enqueue("sidePanel", async () => {
      try {
        await call(commands.setSidePanel(hidden, width));
      } catch (e) {
        set({ error: { setting: "sidePanel", phase: "save", detail: errorText(e) } });
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
