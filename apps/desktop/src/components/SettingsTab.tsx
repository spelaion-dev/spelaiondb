// Settings tab: personal preferences grouped by category. Per-connection options (such as
// saving query history) live with each connection in the sidebar, not here.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import type { LanguagePreference } from "../i18n";
import { usePreferences, type ThemePreference } from "../preferences";
import { useApp } from "../store";

type Category = "appearance" | "language" | "history";
const CATEGORIES: Category[] = ["appearance", "language", "history"];

export function SettingsTab() {
  const { t } = useTranslation("settings");
  const [category, setCategory] = useState<Category>("appearance");

  return (
    <div className="settings">
      <nav className="settings-nav" aria-label={t("categories")}>
        {CATEGORIES.map((c) => (
          <button key={c} aria-current={c === category ? "page" : undefined} onClick={() => setCategory(c)}>
            {t(`${c}.title`)}
          </button>
        ))}
      </nav>
      <section className="settings-page" aria-labelledby={`settings-${category}`}>
        <h2 id={`settings-${category}`}>{t(`${category}.title`)}</h2>
        {category === "appearance" && <AppearanceSettings />}
        {category === "language" && <LanguageSettings />}
        {category === "history" && <HistorySettings />}
      </section>
    </div>
  );
}

function Choice<T extends string>(props: {
  legend: string;
  name: string;
  value: T;
  options: { value: T; label: string }[];
  disabled: boolean;
  onChange(value: T): void;
}) {
  return (
    <fieldset className="settings-choice" disabled={props.disabled}>
      <legend>{props.legend}</legend>
      {props.options.map((o) => (
        <label key={o.value} className="check">
          <input
            type="radio"
            name={props.name}
            value={o.value}
            checked={props.value === o.value}
            onChange={() => props.onChange(o.value)}
          />
          {o.label}
        </label>
      ))}
    </fieldset>
  );
}

function SaveError({ setting }: { setting: "language" | "theme" }) {
  const { t } = useTranslation("settings");
  const error = usePreferences((s) => s.error);
  if (error?.setting !== setting) return null;
  return (
    <p className="error small" role="alert">
      {t(error.phase === "load" ? "loadError" : "saveError")} {error.detail}
    </p>
  );
}

function AppearanceSettings() {
  const { t } = useTranslation("settings");
  const theme = usePreferences((s) => s.theme);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setTheme = usePreferences((s) => s.setTheme);
  return (
    <>
      <Choice<ThemePreference>
        legend={t("appearance.theme")}
        name="theme"
        value={theme}
        disabled={busy}
        options={[
          { value: "system", label: t("appearance.system") },
          { value: "light", label: t("appearance.light") },
          { value: "dark", label: t("appearance.dark") },
        ]}
        onChange={(v) => void setTheme(v)}
      />
      <p className="muted small">{t("appearance.hint")}</p>
      <SaveError setting="theme" />
    </>
  );
}

function LanguageSettings() {
  const { t } = useTranslation("settings");
  const language = usePreferences((s) => s.language);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setLanguage = usePreferences((s) => s.setLanguage);
  return (
    <>
      <Choice<LanguagePreference>
        legend={t("language.label")}
        name="language"
        value={language}
        disabled={busy}
        options={[
          { value: "system", label: t("language.system") },
          // Language names are written in their own language, whatever the current one.
          { value: "en", label: "English" },
          { value: "es", label: "Español" },
        ]}
        onChange={(v) => void setLanguage(v)}
      />
      <p className="muted small">{t("language.hint")}</p>
      <SaveError setting="language" />
    </>
  );
}

function HistorySettings() {
  const { t } = useTranslation("settings");
  const setHistoryOpen = useApp((s) => s.setHistoryOpen);
  return (
    <>
      <p>{t("history.stored")}</p>
      <p>{t("history.perConnection")}</p>
      <p className="muted small">{t("history.audit")}</p>
      <button onClick={() => setHistoryOpen(true)}>{t("history.open")}</button>
    </>
  );
}
