// Settings dialog: personal preferences grouped by category, applied as they change (the app
// behind shows a new theme at once). Per-connection options (such as saving query history)
// live with each connection in the sidebar, not here.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { LanguagePreference } from "../i18n";
import { usePreferences, type PreferenceSetting, type ThemePreference } from "../preferences";
import { getVersion } from "@tauri-apps/api/app";
import { useApp, useDialogs } from "../store";
import { useUpdate } from "../update";
import { call, commands, type NamePatternPreview, type NetworkProfileItem } from "../api";
import { ConnectionTypesSettings } from "./ConnectionTypesSettings";
import { TunnelEditor, type TunnelEditorMode } from "./TunnelEditor";

type Category = "appearance" | "language" | "explorer" | "connections" | "types" | "history" | "network" | "updates" | "scripts" | "editor";
const CATEGORIES: Category[] = ["appearance", "language", "explorer", "connections", "types", "history", "network", "updates", "scripts", "editor"];

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
        {category === "connections" && <ConnectionSettings />}
        {category === "types" && <ConnectionTypesSettings />}
        {category === "history" && <HistorySettings />}
        {category === "network" && <NetworkSettings />}
        {category === "updates" && <UpdateSettings />}
        {category === "scripts" && <ScriptSettings />}
        {category === "editor" && <EditorSettings />}
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

function SaveError({ setting }: { setting: PreferenceSetting }) {
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
      <GridSettings />
    </>
  );
}

/** The editor's tabs (ADR-036): they always come back after a restart, disconnected; this
 *  only chooses whether their connections connect again (never run anything). */
function EditorSettings() {
  const { t } = useTranslation("settings");
  const preferences = usePreferences((s) => s.editor);
  const setEditor = usePreferences((s) => s.setEditor);
  const reconnect = usePreferences((s) => s.reconnectOnStart);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setReconnect = usePreferences((s) => s.setReconnectOnStart);
  return (
    <>
      <fieldset disabled={busy} className="settings-choice">
        <legend>{t("editor.display")}</legend>
        <label className="field"><span>{t("editor.font")}</span>
          <select name="editorFont" value={preferences.font} onChange={(e) => void setEditor({ ...preferences, font: e.target.value })}>
            {["system", "Consolas", "Cascadia Code", "Courier New"].map((font) => <option key={font} value={font}>{font === "system" ? t("editor.systemFont") : font}</option>)}
          </select>
        </label>
        <div className="form-pair">
          {(["fontSize", "lineHeight", "tabSize"] as const).map((key) => <label className="field" key={key}><span>{t(`editor.${key}`)}</span>
            <select name={`editor-${key}`} value={preferences[key]} onChange={(e) => {
              const value = Number(e.target.value);
              void setEditor({ ...preferences, [key]: value, ...(key === "fontSize" && value > preferences.lineHeight ? { lineHeight: [14, 16, 18, 20, 22, 24, 28, 32, 40, 48].find((height) => height >= value)! } : {}) });
            }}>
              {(key === "tabSize" ? [1, 2, 3, 4, 6, 8] : key === "fontSize" ? [10, 11, 12, 13, 14, 15, 16, 18, 20, 24, 28, 32] : [14, 16, 18, 20, 22, 24, 28, 32, 40, 48].filter((n) => n >= preferences.fontSize)).map((n) => <option key={n} value={n}>{n}</option>)}
            </select>
          </label>)}
        </div>
        {(["lineNumbers", "wordWrap"] as const).map((key) => <label className="check" key={key}><input type="checkbox" name={`editor-${key}`} checked={preferences[key]} onChange={(e) => void setEditor({ ...preferences, [key]: e.target.checked })} />{t(`editor.${key}`)}</label>)}
      </fieldset>
      <fieldset disabled={busy} className="settings-choice">
        <legend>{t("editor.formatter")}</legend>
        {(["keywordCase", "indent", "commaPosition"] as const).map((key) => <label className="field" key={key}><span>{t(`editor.${key}`)}</span>
          <select name={`editor-${key}`} value={preferences[key]} onChange={(e) => void setEditor({ ...preferences, [key]: key === "indent" ? Number(e.target.value) : e.target.value })}>
            {(key === "indent" ? [1, 2, 3, 4, 6, 8] : key === "keywordCase" ? ["preserve", "upper", "lower"] : ["trailing", "leading"]).map((value) => <option key={value} value={value}>{typeof value === "number" ? value : t(`editor.${value}`)}</option>)}
          </select>
        </label>)}
      </fieldset>
      <p className="muted small">{t("editor.snippets")}</p>
      <p className="muted small">{t("editor.restoreHint")}</p>
      <label className="check">
        <input type="checkbox" name="reconnectOnStart" checked={reconnect} disabled={busy} onChange={(e) => void setReconnect(e.target.checked)} />
        {t("editor.reconnect")}
      </label>
      <p className="muted small">{t("editor.reconnectHint")}</p>
      <SaveError setting="editor" />
    </>
  );
}

/** Longest pause before "Run anyway" the core accepts (`ScriptPreferences::MAX_CONFIRM_DELAY`). */
const MAX_CONFIRM_DELAY = 10;

/** How scripts run from the editor (ADR-035): what a run does when a statement fails, the pause
 *  before "Run anyway" in a risky statement's confirmation, and the opt-in notification when a
 *  long run finishes in the background. */
function ScriptSettings() {
  const { t } = useTranslation("settings");
  const script = usePreferences((s) => s.script);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setScript = usePreferences((s) => s.setScript);
  const [after, setAfter] = useState(String(script.notifyAfterSeconds));
  useEffect(() => setAfter(String(script.notifyAfterSeconds)), [script.notifyAfterSeconds]);
  const [delay, setDelay] = useState(String(script.confirmDelaySeconds));
  useEffect(() => setDelay(String(script.confirmDelaySeconds)), [script.confirmDelaySeconds]);
  const saveDelay = () => {
    const seconds = Math.trunc(Number(delay));
    if (delay.trim() !== "" && Number.isFinite(seconds) && seconds >= 0 && seconds <= MAX_CONFIRM_DELAY) {
      if (seconds !== script.confirmDelaySeconds) void setScript({ ...script, confirmDelaySeconds: seconds });
    } else {
      setDelay(String(script.confirmDelaySeconds));
    }
  };
  const saveAfter = () => {
    const seconds = Math.trunc(Number(after));
    if (Number.isFinite(seconds) && seconds >= 1 && seconds <= 86400) {
      if (seconds !== script.notifyAfterSeconds) void setScript({ ...script, notifyAfterSeconds: seconds });
    } else {
      setAfter(String(script.notifyAfterSeconds));
    }
  };
  return (
    <>
      <fieldset className="settings-choice" disabled={busy}>
        <legend>{t("scripts.onError")}</legend>
        {(["stop", "continue", "ask"] as const).map((mode) => (
          <label key={mode} className="check">
            <input type="radio" name="scriptOnError" value={mode} checked={script.onError === mode} onChange={() => void setScript({ ...script, onError: mode })} />
            {t(`scripts.mode.${mode}`)}
          </label>
        ))}
      </fieldset>
      <p className="muted small">{t("scripts.onErrorHint")}</p>
      <label className="field port">
        <span>{t("scripts.confirmDelay")}</span>
        <input
          type="number"
          name="scriptConfirmDelay"
          min={0}
          max={MAX_CONFIRM_DELAY}
          value={delay}
          disabled={busy}
          onChange={(e) => setDelay(e.target.value)}
          onBlur={saveDelay}
          onKeyDown={(e) => e.key === "Enter" && saveDelay()}
        />
      </label>
      <p className="muted small">{t("scripts.confirmDelayHint")}</p>
      <fieldset className="settings-choice" disabled={busy}>
        <legend>{t("scripts.notifications")}</legend>
        <label className="check">
          <input type="checkbox" name="scriptNotify" checked={script.notify} onChange={(e) => void setScript({ ...script, notify: e.target.checked })} />
          {t("scripts.notify")}
        </label>
        <label className="field port">
          <span>{t("scripts.notifyAfter")}</span>
          <input
            type="number"
            name="scriptNotifyAfter"
            min={1}
            max={86400}
            value={after}
            disabled={!script.notify}
            onChange={(e) => setAfter(e.target.value)}
            onBlur={saveAfter}
            onKeyDown={(e) => e.key === "Enter" && saveAfter()}
          />
        </label>
      </fieldset>
      <p className="muted small">{t("scripts.notifyHint")}</p>
      <SaveError setting="script" />
    </>
  );
}

/** How the result grid draws its cells: zebra rows, NULL and booleans. */
function GridSettings() {
  const { t } = useTranslation("settings");
  const grid = usePreferences((s) => s.grid);
  const busy = usePreferences((s) => s.loading || s.saving !== null);
  const setGrid = usePreferences((s) => s.setGrid);
  return (
    <>
      <fieldset className="settings-choice" disabled={busy}>
        <legend>{t("appearance.grid")}</legend>
        <label className="check">
          <input type="checkbox" name="zebra" checked={grid.zebra} onChange={(e) => void setGrid({ ...grid, zebra: e.target.checked })} />
          {t("appearance.zebra")}
        </label>
        <label className="check">
          <input type="checkbox" name="nullText" checked={grid.nullText} onChange={(e) => void setGrid({ ...grid, nullText: e.target.checked })} />
          {t("appearance.nullText")}
        </label>
        <label className="check">
          <input type="checkbox" name="boolCheckboxes" checked={grid.boolCheckboxes} onChange={(e) => void setGrid({ ...grid, boolCheckboxes: e.target.checked })} />
          {t("appearance.boolCheckboxes")}
        </label>
      </fieldset>
      <p className="muted small">{t("appearance.gridHint")}</p>
      <SaveError setting="grid" />
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
      <button onClick={() => { setHistoryOpen(true); useApp.getState().closeSettings(); }}>{t("history.open")}</button>
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
/** The pattern new connections are named with (ADR-030), checked and previewed by the core. */
function ConnectionSettings() {
  const { t } = useTranslation("settings");
  const [pattern, setPattern] = useState<string | null>(null);
  const [preview, setPreview] = useState<NamePatternPreview | null>(null);
  const [status, setStatus] = useState<{ kind: "idle" | "saved" } | { kind: "error"; message: string }>({ kind: "idle" });
  const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

  useEffect(() => {
    call(commands.getConnectionNamePattern())
      .then(setPattern)
      .catch((e: unknown) => setStatus({ kind: "error", message: errorText(e) }));
  }, []);
  useEffect(() => {
    if (pattern === null) return;
    let current = true;
    void commands.previewConnectionNamePattern(pattern).then((p) => current && setPreview(p));
    return () => {
      current = false;
    };
  }, [pattern]);

  async function save(next: string | null) {
    try {
      await call(commands.setConnectionNamePattern(next));
      if (next === null) setPattern(await call(commands.getConnectionNamePattern()));
      setStatus({ kind: "saved" });
    } catch (e) {
      setStatus({ kind: "error", message: errorText(e) });
    }
  }

  if (pattern === null) return status.kind === "error" ? <p className="error small" role="alert">{status.message}</p> : null;
  const valid = preview !== null && preview.error === null;
  return (
    <section aria-label={t("connections.namePattern")}>
      <h3>{t("connections.namePattern")}</h3>
      <p>{t("connections.namePatternIntro")}</p>
      <label className="field">
        <span>{t("connections.pattern")}</span>
        <input
          type="text"
          value={pattern}
          spellCheck={false}
          autoComplete="off"
          aria-invalid={!valid}
          onChange={(e) => {
            setPattern(e.target.value);
            setStatus({ kind: "idle" });
          }}
        />
      </label>
      <div className="pattern-variables" aria-label={t("connections.variables")}>
        {preview?.variables.map((name) => (
          <button
            key={name}
            className="ghost small"
            title={t(`connections.variable.${name}`)}
            onClick={() => {
              setPattern(`${pattern}{${name}}`);
              setStatus({ kind: "idle" });
            }}
          >
            {`{${name}}`}
          </button>
        ))}
      </div>
      <p className="muted small">{t("connections.syntax")}</p>
      {preview && preview.unknown.length > 0 && (
        <p className="error small" role="alert">
          {t("connections.unknown", { names: preview.unknown.map((n) => `{${n}}`).join(", ") })}
        </p>
      )}
      {preview?.error && preview.unknown.length === 0 && (
        <p className="error small" role="alert">
          {preview.error}
        </p>
      )}
      {valid && (
        <dl className="pattern-preview" aria-label={t("connections.preview")}>
          <dt>{t("connections.previewServer")}</dt>
          <dd>{preview.server || <span className="muted">{t("connections.previewEmpty")}</span>}</dd>
          <dt>{t("connections.previewFile")}</dt>
          <dd>{preview.file || <span className="muted">{t("connections.previewEmpty")}</span>}</dd>
        </dl>
      )}
      <div className="form-row">
        <button disabled={!valid} onClick={() => void save(pattern)}>
          {t("connections.save")}
        </button>
        <button className="ghost" onClick={() => void save(null)}>
          {t("connections.reset")}
        </button>
      </div>
      {status.kind === "saved" && <p className="small" role="status">{t("connections.saved")}</p>}
      {status.kind === "error" && <p className="error small" role="alert">{status.message}</p>}
      <p className="muted small">{t("connections.hint")}</p>
    </section>
  );
}

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
  const showNotes = useUpdate((s) => s.showNotes);
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
              <button disabled={installing} onClick={showNotes}>{t("updates.notesAction")}</button>{" "}
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
