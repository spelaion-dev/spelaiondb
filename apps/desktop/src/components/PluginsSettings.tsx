// Settings → Plugins (ADR-055): every plugin of ours, as the core sees it on this machine:
// inside the installer, installed, offered by the signed catalogue, with an update, refused (and
// why) or missing. Install, update and remove are one click each; removing a build that runs
// asks first. Everything shown and every decision is the core's (`src-tauri/src/plugins.rs`):
// no command here takes an address, a key or a catalogue, only a plugin's name, a version the
// overview listed, an id this page chose, or a file the user picked.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type EngineItem, type PluginItem } from "../api";
import type { PluginProcessItem } from "../bindings";
import { chooseOpenFile } from "../dialogs";
import { engineCatalog } from "../engines";
import { byteSize, installable, installText, needsInstalling, usePlugins } from "../plugins";
import { useApp } from "../store";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** The catalogue as this page last read it. */
type CatalogueState =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "off" }
  | { kind: "read" }
  | { kind: "failed"; detail: string };

let nextInstall = 1;

/** How often the running processes' numbers are read again while the page is open. */
const PROCESS_REFRESH_MS = 2000;

export function PluginsSettings() {
  const { t } = useTranslation("settings");
  const overview = usePlugins((s) => s.overview);
  const setOverview = usePlugins((s) => s.set);
  const [catalogue, setCatalogue] = useState<CatalogueState>({ kind: "idle" });
  const [engines, setEngines] = useState<EngineItem[]>([]);
  /** The last error of an action on a plugin, shown under its row. */
  const [rowErrors, setRowErrors] = useState<Record<string, string>>({});
  const [fileBusy, setFileBusy] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);
  const [fileDone, setFileDone] = useState(false);
  const [removing, setRemoving] = useState<{ plugin: PluginItem; version: string; openSessions: number } | null>(null);
  /** The running plugin processes (SDB-399), by plugin name: the core's numbers. */
  const [processes, setProcesses] = useState<Record<string, PluginProcessItem>>({});
  const [restarting, setRestarting] = useState<{ plugin: PluginItem; sessions: string[] } | null>(null);
  /** Plugins restarted from this page, to say what happened under their row. */
  const [restarted, setRestarted] = useState<Record<string, boolean>>({});
  const mounted = useRef(true);

  async function checkCatalogue() {
    setCatalogue({ kind: "checking" });
    try {
      const fresh = await call(commands.pluginsOverview());
      if (!mounted.current) return;
      setOverview(fresh);
      if (!fresh.updatesAllowed) {
        setCatalogue({ kind: "off" });
        return;
      }
      setOverview(await call(commands.pluginsRefreshCatalogue()));
      if (mounted.current) setCatalogue({ kind: "read" });
    } catch (e) {
      if (mounted.current) setCatalogue({ kind: "failed", detail: errorText(e) });
    }
  }

  useEffect(() => {
    mounted.current = true;
    void engineCatalog().then((all) => mounted.current && setEngines(all)).catch(() => undefined);
    void checkCatalogue();
    // The running processes, read again while the page is open; nothing is started for it.
    const readProcesses = () =>
      void call(commands.pluginProcesses())
        .then((list) => mounted.current && setProcesses(Object.fromEntries(list.map((process) => [process.name, process]))))
        .catch(() => undefined);
    readProcesses();
    const timer = window.setInterval(readProcesses, PROCESS_REFRESH_MS);
    return () => {
      mounted.current = false;
      window.clearInterval(timer);
    };
  }, []);

  const engineName = (id: string) => engines.find((engine) => engine.id === id)?.name ?? id;

  function rowError(name: string, message: string | null) {
    setRowErrors((current) => {
      const next = { ...current };
      if (message === null) delete next[name];
      else next[name] = message;
      return next;
    });
  }

  /** Starts an install. Its progress and its end reach every view through the adapter
   *  (`pluginInstallChanged`), so this page shows the install as the picker does. */
  async function install(plugin: PluginItem) {
    rowError(plugin.name, null);
    try {
      setOverview(await call(commands.pluginsInstall(nextInstall++, plugin.name)));
    } catch {
      // The failure is the plugin's install state now: read it, in case its event was missed.
      await usePlugins.getState().load();
    }
  }

  async function remove(plugin: PluginItem, version: string, confirmed: boolean) {
    rowError(plugin.name, null);
    try {
      const outcome = await call(commands.pluginsRemove(plugin.name, version, confirmed));
      switch (outcome.kind) {
        case "needsConfirmation":
          setRemoving({ plugin, version, openSessions: outcome.openSessions });
          return;
        case "removed":
          setOverview(outcome.overview);
          setRemoving(null);
          return;
      }
    } catch (e) {
      const message = t("plugins.removeError", { detail: errorText(e) });
      if (confirmed) throw new Error(message);
      rowError(plugin.name, message);
    }
  }

  /** Restarts a plugin's process (SDB-399): its sessions are lost and nothing is run again;
   *  with a transaction open in one of them it asks first. */
  async function restart(plugin: PluginItem, confirmed: boolean) {
    rowError(plugin.name, null);
    try {
      const outcome = await call(commands.pluginRestart(plugin.name, confirmed));
      switch (outcome.kind) {
        case "needsConfirmation":
          setRestarting({ plugin, sessions: outcome.sessions });
          return;
        case "restarted":
        case "notRunning":
          setRestarting(null);
          setRestarted((current) => ({ ...current, [plugin.name]: true }));
          setProcesses((current) => {
            const next = { ...current };
            delete next[plugin.name];
            return next;
          });
          return;
      }
    } catch (e) {
      const message = t("plugins.restartError", { detail: errorText(e) });
      if (confirmed) throw new Error(message);
      rowError(plugin.name, message);
    }
  }

  async function installFile() {
    setFileError(null);
    setFileDone(false);
    const path = await chooseOpenFile(t("plugins.fileTitle"), [{ name: t("plugins.fileFilter"), extensions: ["gz", "tgz", "signed"] }]);
    if (!path) return;
    setFileBusy(true);
    try {
      setOverview(await call(commands.pluginsInstallFile(path)));
      setFileDone(true);
    } catch (e) {
      setFileError(t("plugins.fileError", { detail: errorText(e) }));
    } finally {
      setFileBusy(false);
    }
  }

  return (
    <div className="plugins-settings">
      <p className="muted small">{t("plugins.intro")}</p>
      <div className="plugins-catalogue" role="status" aria-live="polite">
        <span className="muted small">
          {catalogue.kind === "checking" && t("plugins.checking")}
          {catalogue.kind === "off" && t("plugins.off")}
          {catalogue.kind === "read" &&
            (overview?.catalogueSerial != null ? t("plugins.catalogueRead", { serial: overview.catalogueSerial }) : t("plugins.noCatalogue"))}
          {catalogue.kind === "failed" && t("plugins.catalogueFailed", { detail: catalogue.detail })}
        </span>
        {catalogue.kind === "off" && (
          <button className="ghost" onClick={() => useApp.setState({ settingsCategory: "updates" })}>
            {t("plugins.openUpdates")}
          </button>
        )}
        {(catalogue.kind === "read" || catalogue.kind === "failed") && (
          <button className="ghost" onClick={() => void checkCatalogue()}>
            {t("plugins.checkAgain")}
          </button>
        )}
      </div>
      {overview === null ? (
        <p className="muted">{t("plugins.loading")}</p>
      ) : overview.plugins.length === 0 ? (
        <p className="muted">{t("plugins.none")}</p>
      ) : (
        <ul className="plugin-list" aria-label={t("plugins.listLabel")}>
          {overview.plugins.map((plugin) => (
            <PluginRow
              key={plugin.name}
              plugin={plugin}
              engineName={engineName}
              error={rowErrors[plugin.name] ?? null}
              onInstall={() => void install(plugin)}
              onCancel={(id) => void commands.pluginsCancelInstall(id)}
              onRemove={(version) => void remove(plugin, version, false)}
              process={processes[plugin.name] ?? null}
              restarted={restarted[plugin.name] ?? false}
              onRestart={() => void restart(plugin, false)}
            />
          ))}
        </ul>
      )}
      <h3>{t("plugins.fileHeading")}</h3>
      <p className="muted small">{t("plugins.fileHint")}</p>
      <div className="form-row">
        <button onClick={() => void installFile()} disabled={fileBusy}>
          {fileBusy ? t("plugins.installingFile") : t("plugins.installFile")}
        </button>
      </div>
      {fileDone && <p className="muted small" role="status">{t("plugins.fileDone")}</p>}
      {fileError && (
        <p className="error small" role="alert">
          {fileError}
        </p>
      )}
      {restarting && (
        <RestartDialog
          plugin={restarting.plugin}
          sessions={restarting.sessions}
          onCancel={() => setRestarting(null)}
          onRestart={() => restart(restarting.plugin, true)}
        />
      )}
      {removing && (
        <RemoveDialog
          plugin={removing.plugin}
          version={removing.version}
          openSessions={removing.openSessions}
          engineName={engineName}
          onCancel={() => setRemoving(null)}
          onRemove={() => remove(removing.plugin, removing.version, true)}
        />
      )}
    </div>
  );
}

function PluginRow({
  plugin,
  engineName,
  error,
  onInstall,
  onCancel,
  onRemove,
  process,
  restarted,
  onRestart,
}: {
  plugin: PluginItem;
  engineName(id: string): string;
  error: string | null;
  onInstall(): void;
  onCancel(id: number): void;
  onRemove(version: string): void;
  /** Its running process, if any (SDB-399). */
  process: PluginProcessItem | null;
  /** Restarted from this page since it opened. */
  restarted: boolean;
  onRestart(): void;
}) {
  const { t, i18n } = useTranslation("settings");
  const size = (bytes: number) => byteSize(bytes, i18n.language);
  const title = plugin.engines.map(engineName).join(", ") || plugin.name;
  const used = plugin.inUse;
  const offer = installable(plugin);
  // The download running, wherever it was started; its id stops it.
  const downloading = plugin.install.kind === "downloading" ? plugin.install : null;
  const installNote = installText(plugin, t, i18n.language);
  return (
    <li className="plugin-row" aria-label={title} data-plugin={plugin.name}>
      <div className="plugin-main">
        <div className="plugin-title">
          <span className="plugin-name">{title}</span>
          <span className="mono small muted">{plugin.name}</span>
        </div>
        <div className="small">
          {used
            ? t(used.source === "shipped" ? "plugins.shipped" : "plugins.installed", { version: used.version })
            : t("plugins.notInstalled")}
        </div>
        {plugin.update && plugin.offered && (
          <div className="small">{t("plugins.update", { version: plugin.update, size: size(plugin.offered.archiveSize ?? 0) })}</div>
        )}
        {needsInstalling(plugin) && plugin.offered && (
          <div className="small">{t("plugins.offered", { version: plugin.offered.version, size: size(plugin.offered.archiveSize ?? 0) })}</div>
        )}
        {plugin.message ? (
          <div className="small plugin-why">{plugin.message}</div>
        ) : (
          plugin.refused.map((refused) => (
            <div key={`${refused.source}-${refused.version}`} className="small plugin-why">
              {t(refused.source === "shipped" ? "plugins.refusedShipped" : "plugins.refusedInstalled", { version: refused.version, why: refused.why })}
            </div>
          ))
        )}
        {plugin.missing && !plugin.message && (
          <div className="small plugin-why">{t("plugins.missing", { version: plugin.missing.version, program: plugin.missing.program })}</div>
        )}
        {/* The running process's line says its sessions too (SDB-399). */}
        {plugin.openSessions > 0 && !process && <div className="small muted">{t("plugins.sessions", { count: plugin.openSessions })}</div>}
        {process && (
          <div className="small plugin-process" data-pid={process.pid}>
            {t("plugins.process", {
              memory: process.memoryBytes == null ? t("plugins.unknown") : size(process.memoryBytes),
              cpu: process.cpuPercent == null ? t("plugins.measuring") : `${Math.round(process.cpuPercent)}%`,
              count: process.sessions,
            })}
          </div>
        )}
        {restarted && !process && <div className="small muted" role="status">{t("plugins.restarted")}</div>}
        {downloading && installNote && (
          <div className="small plugin-progress" role="status">
            {installNote}
          </div>
        )}
        {plugin.installed.length > 0 && (
          <ul className="plugin-versions" aria-label={t("plugins.installedVersions")}>
            {plugin.installed.map((version) => (
              <li key={version} className="small">
                <span>{t("plugins.installedVersion", { version })}</span>
                <button className="ghost" onClick={() => onRemove(version)} disabled={downloading !== null}>
                  {t("plugins.remove")}
                </button>
              </li>
            ))}
          </ul>
        )}
        {plugin.install.kind === "failed" && installNote && (
          <div className="error small" role="alert">
            {installNote}
          </div>
        )}
        {error && (
          <div className="error small" role="alert">
            {error}
          </div>
        )}
      </div>
      <div className="plugin-actions">
        {process && !downloading && (
          <button onClick={onRestart} title={t("plugins.restartHint")}>
            {t("plugins.restart")}
          </button>
        )}
        {downloading ? (
          <button onClick={() => onCancel(downloading.install)}>{t("plugins.cancelInstall")}</button>
        ) : (
          offer && (
            <button onClick={onInstall} title={offer.address}>
              {used ? t("plugins.updateTo", { version: offer.version }) : t("plugins.install")}
            </button>
          )
        )}
      </div>
    </li>
  );
}

function RemoveDialog({
  plugin,
  version,
  openSessions,
  engineName,
  onCancel,
  onRemove,
}: {
  plugin: PluginItem;
  version: string;
  openSessions: number;
  engineName(id: string): string;
  onCancel(): void;
  onRemove(): Promise<void>;
}) {
  const { t } = useTranslation("settings");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);
  const engines = plugin.engines.map(engineName).join(", ");

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await onRemove();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="plugin-remove-title"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!busy) onCancel();
          }
        }}
      >
        <h2 id="plugin-remove-title">{t("plugins.removeTitle", { name: plugin.name, version })}</h2>
        <p>
          {openSessions > 0
            ? t("plugins.removeSessions", { count: openSessions, engines })
            : t("plugins.removeRunning", { engines })}
        </p>
        <p className="muted small">{t("plugins.removeHint")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel} disabled={busy}>
            {t("plugins.cancel")}
          </button>
          <button className="danger" onClick={() => void confirm()} disabled={busy}>
            {busy ? t("plugins.removing") : t("plugins.removeConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Restarting a plugin whose sessions hold open transactions (ADR-031): what is rolled back,
 *  in which tabs; Cancel is the safe choice and has the focus. */
function RestartDialog({
  plugin,
  sessions,
  onCancel,
  onRestart,
}: {
  plugin: PluginItem;
  sessions: string[];
  onCancel(): void;
  onRestart(): Promise<void>;
}) {
  const { t } = useTranslation("settings");
  const tabs = useApp((s) => s.tabs);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);
  const names = sessions.map((id) => tabs.find((tab) => tab.id === id)?.title ?? id);

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await onRestart();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="plugin-restart-title"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!busy) onCancel();
          }
        }}
      >
        <h2 id="plugin-restart-title">{t("plugins.restartTitle", { name: plugin.name })}</h2>
        <p>{t("plugins.restartTransactions", { count: names.length })}</p>
        <ul className="unsaved-edits-list">
          {names.map((name) => (
            <li key={name}>{name}</li>
          ))}
        </ul>
        <p className="muted small">{t("plugins.restartAvoid")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel} disabled={busy}>
            {t("plugins.cancel")}
          </button>
          <button className="danger" onClick={() => void confirm()} disabled={busy}>
            {busy ? t("plugins.restarting") : t("plugins.restartConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
