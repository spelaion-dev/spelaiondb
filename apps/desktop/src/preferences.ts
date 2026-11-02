// Personal interface preferences (theme and language). They are stored by the core in the
// personal state.db; this module only applies them to the document and keeps the UI state.

import { create } from "zustand";
import { call, commands } from "./api";
import { applyLanguage, type LanguagePreference } from "./i18n";

export type ThemePreference = "system" | "light" | "dark";

/** Fired on `window` whenever the effective theme changes (the grid repaints its canvas). */
export const THEME_CHANGE_EVENT = "spelunk-themechange";

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

export interface PreferenceError {
  setting: "language" | "theme";
  phase: "load" | "save";
  detail: string;
}

interface PreferencesStore {
  language: LanguagePreference;
  theme: ThemePreference;
  loading: boolean;
  saving: "language" | "theme" | null;
  error: PreferenceError | null;
  load(): Promise<void>;
  setLanguage(next: LanguagePreference): Promise<void>;
  setTheme(next: ThemePreference): Promise<void>;
  clearError(): void;
}

export const usePreferences = create<PreferencesStore>((set, get) => ({
  language: "system",
  theme: "system",
  loading: true,
  saving: null,
  error: null,
  async load() {
    const [language, theme] = await Promise.allSettled([call(commands.getLanguage()), call(commands.getTheme())]);
    let error: PreferenceError | null = null;
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
  clearError: () => set({ error: null }),
}));

// Preferences set to "system" follow the operating system while the app is open.
darkScheme.addEventListener("change", () => applyTheme(usePreferences.getState().theme));
window.addEventListener("languagechange", () => {
  const { language } = usePreferences.getState();
  if (language === "system") void applyLanguage("system");
});
applyTheme("system");
