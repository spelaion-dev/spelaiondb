import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import enApp from "./locales/en/app.json";
import esApp from "./locales/es/app.json";
import enEditor from "./locales/en/editor.json";
import esEditor from "./locales/es/editor.json";
import enShell from "./locales/en/shell.json";
import esShell from "./locales/es/shell.json";
import enConnections from "./locales/en/connections.json";
import esConnections from "./locales/es/connections.json";
import enSettings from "./locales/en/settings.json";
import esSettings from "./locales/es/settings.json";

export type LanguagePreference = "system" | "en" | "es";

/** Select the first supported system locale and fall back to English. */
export function systemLanguage(): string {
  const languages = navigator.languages?.length ? navigator.languages : [navigator.language];
  return languages.find((lang) => /^(en|es)(-|$)/i.test(lang)) ?? "en";
}

export function applyLanguage(preference: LanguagePreference): Promise<unknown> {
  const language = preference === "system" ? systemLanguage() : preference;
  return i18n.changeLanguage(language).then(() => {
    document.documentElement.lang = i18n.language ?? "en";
  });
}

void i18n.use(initReactI18next).init({
  resources: {
    en: { app: enApp, connections: enConnections, editor: enEditor, settings: enSettings, shell: enShell },
    es: { app: esApp, connections: esConnections, editor: esEditor, settings: esSettings, shell: esShell },
  },
  lng: systemLanguage(),
  fallbackLng: "en",
  supportedLngs: ["en", "es"],
  nonExplicitSupportedLngs: true,
  ns: ["app", "connections", "editor", "settings", "shell"],
  defaultNS: "app",
  initAsync: false,
  interpolation: { escapeValue: false }, // React escapes interpolated text when rendering.
});
document.documentElement.lang = i18n.language ?? "en";

export default i18n;
