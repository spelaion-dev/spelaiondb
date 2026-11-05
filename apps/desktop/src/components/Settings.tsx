// Settings dialog: personal preferences grouped by category, applied as they change (the app
// behind shows a new theme at once). Per-connection options (such as saving query history)
// live with each connection in the sidebar, not here.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { LanguagePreference } from "../i18n";
import { usePreferences, type ThemePreference } from "../preferences";
import { getVersion } from "@tauri-apps/api/app";
import { useApp, useDialogs } from "../store";
import { useUpdate } from "../update";
import { call, commands, type NetworkProfileItem } from "../api";
import { TunnelEditor, type TunnelEditorMode } from "./TunnelEditor";

type Category = "appearance" | "language" | "explorer" | "history" | "network" | "updates";
const CATEGORIES: Category[] = ["appearance", "language", "explorer", "history", "network", "updates"];

export function SettingsDialog({ onClose }: { onClose(): void }) {
  const { t } = useTranslation("settings");
  const [category, setCategory] = useState<Category>("appearance");

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Dialogs opened on top (trust, passwords…) handle their own Escape.
      const dialogs = useDialogs.getState();
      if (e.key === "Escape" && !dialogs.trust && !dialogs.password && dialogs.interactions.length === 0) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  return (
    <div className="modal-backdrop wizard-backdrop">
      <div className="modal wizard settings-dialog" role="dialog" aria-modal="true" aria-labelledby="settings-title">
        <div className="wizard-header">
          <h2 id="settings-title">{t("title")}</h2>
          <button className="ghost" aria-label={t("close")} onClick={onClose}>
            ×
          </button>
        </div>
        <Settings category={category} onCategory={setCategory} />
      </div>
    </div>
  );
}

function Settings({ category, onCategory: setCategory }: { category: Category; onCategory(category: Category): void }) {
  const { t } = useTranslation("settings");
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
        {category === "explorer" && <ExplorerSettings />}
        {category === "history" && <HistorySettings />}
        {category === "network" && <NetworkSettings />}
        {category === "updates" && <UpdateSettings />}
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

function SaveError({ setting }: { setting: "language" | "theme" | "explorer" }) {
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

function ExplorerSettings() {
  const { t } = useTranslation("settings");
  const explorer = usePreferences((s) => s.explorer);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setExplorer = usePreferences((s) => s.setExplorer);
  return (
    <>
      <label className="check">
        <input
          type="checkbox"
          name="showSystem"
          checked={explorer.showSystem}
          disabled={busy}
          onChange={(e) => void setExplorer({ ...explorer, showSystem: e.target.checked })}
        />
        {t("explorer.showSystem")}
      </label>
      <p className="muted small">{t("explorer.showSystemHint")}</p>
      <fieldset className="settings-choice" disabled={busy}>
        <legend>{t("explorer.sorting")}</legend>
        <label className="check">
          <input
            type="checkbox"
            name="sortAlphabetically"
            checked={explorer.sortAlphabetically}
            onChange={(e) => void setExplorer({ ...explorer, sortAlphabetically: e.target.checked })}
          />
          {t("explorer.sortAlphabetically")}
        </label>
        <label className="check">
          <input
            type="checkbox"
            name="sortIgnoreCase"
            checked={explorer.sortIgnoreCase}
            disabled={!explorer.sortAlphabetically}
            onChange={(e) => void setExplorer({ ...explorer, sortIgnoreCase: e.target.checked })}
          />
          {t("explorer.sortIgnoreCase")}
        </label>
        <label className="check">
          <input
            type="checkbox"
            name="foldersFirst"
            checked={explorer.foldersFirst}
            onChange={(e) => void setExplorer({ ...explorer, foldersFirst: e.target.checked })}
          />
          {t("explorer.foldersFirst")}
        </label>
      </fieldset>
      <p className="muted small">{t("explorer.sortingHint")}</p>
      <label className="check">
        <input
          type="checkbox"
          name="serverSideFilters"
          checked={explorer.serverSideFilters}
          disabled={busy}
          onChange={(e) => void setExplorer({ ...explorer, serverSideFilters: e.target.checked })}
        />
        {t("explorer.serverSideFilters")}
      </label>
      <p className="muted small">{t("explorer.serverSideFiltersHint")}</p>
      <SaveError setting="explorer" />
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

/** Network profiles (tunnel files, ADR-027) and the range of local ports tunnels use. */
function NetworkSettings() {
  return (
    <>
      <NetworkProfiles />
      <PortRange />
    </>
  );
}

/** Every network profile, to create, edit or delete them without opening a connection. */
function NetworkProfiles() {
  const { t } = useTranslation("settings");
  const hasWorkspace = Boolean(useApp((s) => s.workspace)?.workspace);
  const [profiles, setProfiles] = useState<NetworkProfileItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editor, setEditor] = useState<TunnelEditorMode | null>(null);

  function refresh() {
    call(commands.networkProfiles())
      .then((list) => {
        setProfiles(list);
        setError(null);
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
  }
  useEffect(refresh, []);

  return (
    <section aria-label={t("network.profiles")}>
      <h3>{t("network.profiles")}</h3>
      <p className="muted small">{t("network.profilesIntro")}</p>
      {error && <p className="error small" role="alert">{error}</p>}
      {profiles && profiles.length === 0 && <p className="muted">{t("network.noProfiles")}</p>}
      {profiles && profiles.length > 0 && (
        <ul className="profile-list">
          {profiles.map((profile) => {
            const route = [...(profile.proxy ? [profile.proxy] : []), ...profile.hops].join(" → ");
            return (
              <li key={`${profile.personal}:${profile.name}`} className="profile-row" aria-label={profile.name}>
                <div className="grow">
                  <strong>{profile.name}</strong>{" "}
                  <span className="muted small">{profile.personal ? t("network.personal") : t("network.project")}</span>
                  <div className="mono small">{route}</div>
                  <div className="muted small">
                    {profile.usedBy.length === 0 ? t("network.unused") : t("network.usedBy", { names: profile.usedBy.join(", ") })}
                  </div>
                </div>
                <button onClick={() => setEditor({ kind: "edit", name: profile.name, personal: profile.personal })}>{t("network.edit")}</button>
              </li>
            );
          })}
        </ul>
      )}
      <button onClick={() => setEditor({ kind: "add", personal: !hasWorkspace })}>{t("network.newProfile")}</button>
      {editor && (
        <TunnelEditor
          mode={editor}
          onClose={() => setEditor(null)}
          onSaved={() => {
            setEditor(null);
            refresh();
          }}
          onDeleted={() => {
            setEditor(null);
            refresh();
          }}
        />
      )}
    </section>
  );
}

/** The range of local ports SSH tunnels listen on, for a firewall that only allows some. */
function PortRange() {
  const { t } = useTranslation("settings");
  const [min, setMin] = useState("");
  const [max, setMax] = useState("");
  const [status, setStatus] = useState<{ kind: "idle" | "saved" } | { kind: "error"; message: string }>({ kind: "idle" });

  useEffect(() => {
    call(commands.tunnelPorts())
      .then((range) => {
        setMin(range ? String(range.min) : "");
        setMax(range ? String(range.max) : "");
      })
      .catch((e: unknown) => setStatus({ kind: "error", message: e instanceof Error ? e.message : String(e) }));
  }, []);

  async function save(range: { min: number; max: number } | null) {
    try {
      await call(commands.setTunnelPorts(range));
      if (!range) {
        setMin("");
        setMax("");
      }
      setStatus({ kind: "saved" });
    } catch (e) {
      setStatus({ kind: "error", message: e instanceof Error ? e.message : String(e) });
    }
  }

  const port = (text: string) => (text.trim() === "" ? null : Math.trunc(Number(text)));
  const typedMin = port(min);
  const typedMax = port(max);
  return (
    <section aria-label={t("network.ports")}>
      <h3>{t("network.ports")}</h3>
      <p>{t("network.portsIntro")}</p>
      <div className="form-pair">
        <label className="field port">
          <span>{t("network.portMin")}</span>
          <input type="number" min={1} max={65535} value={min} onChange={(e) => { setMin(e.target.value); setStatus({ kind: "idle" }); }} />
        </label>
        <label className="field port">
          <span>{t("network.portMax")}</span>
          <input type="number" min={1} max={65535} value={max} onChange={(e) => { setMax(e.target.value); setStatus({ kind: "idle" }); }} />
        </label>
      </div>
      <div className="form-row">
        <button disabled={typedMin === null || typedMax === null} onClick={() => typedMin !== null && typedMax !== null && void save({ min: typedMin, max: typedMax })}>
          {t("network.save")}
        </button>
        <button className="ghost" onClick={() => void save(null)}>
          {t("network.clear")}
        </button>
      </div>
      {status.kind === "saved" && <p className="small" role="status">{t("network.saved")}</p>}
      {status.kind === "error" && <p className="error small" role="alert">{status.message}</p>}
      <p className="muted small">{t("network.portsHint")}</p>
    </section>
  );
}

function UpdateSettings() {
  const { t } = useTranslation("settings");
  const [version, setVersion] = useState("");
  const status = useUpdate((s) => s.status);
  const checking = useUpdate((s) => s.checking);
  const installing = useUpdate((s) => s.installing);
  const install = useUpdate((s) => s.install);
  const checkAtStartup = useUpdate((s) => s.checkAtStartup);
  const error = useUpdate((s) => s.error);
  const check = useUpdate((s) => s.check);
  const setCheckAtStartup = useUpdate((s) => s.setCheckAtStartup);
  useEffect(() => {
    void getVersion().then(setVersion);
  }, []);
  // Know at once whether this build can update at all; with no endpoint nothing is requested.
  useEffect(() => {
    if (useUpdate.getState().status === null) void useUpdate.getState().check();
  }, []);
  const unavailable = status?.kind === "unavailable";
  return (
    <>
      <p>{t("updates.version", { version })}</p>
      {unavailable ? (
        <p className="muted small" role="status">{t("updates.unavailable")}</p>
      ) : (
        <>
          <label className="check">
            <input type="checkbox" checked={checkAtStartup} onChange={(e) => void setCheckAtStartup(e.target.checked)} />
            {t("updates.atStartup")}
          </label>
          <div>
            <button disabled={checking || installing} onClick={() => void check()}>
              {checking ? t("updates.checking") : t("updates.check")}
            </button>
          </div>
          <p className="small" role="status">
            {status?.kind === "upToDate" && t("updates.upToDate")}
            {status?.kind === "available" && t("updates.available", { version: status.version })}
          </p>
          {/* Installed from here too, without closing the dialog for the notice behind it. */}
          {status?.kind === "available" && (
            <div>
              <button className="primary" disabled={installing} onClick={() => void install()}>
                {installing ? t("updates.installing") : t("updates.install")}
              </button>
            </div>
          )}
          <p className="muted small">{t("updates.hint")}</p>
        </>
      )}
      {error && (
        <p className="error small" role="alert">
          {t("updates.error")} {error}
        </p>
      )}
    </>
  );
}
