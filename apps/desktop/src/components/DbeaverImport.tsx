// Import from DBeaver: the connections of its workspace (or a folder the user picks), chosen
// one by one. The core reads DBeaver's files; passwords never come to the UI, and only go to
// the keychain when the user asks for it.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type DbeaverItem, type ImportReport } from "../api";
import { chooseFolder } from "../dialogs";
import { EngineIcon } from "./EngineIcon";
import { useApp } from "../store";
import { refreshConnections } from "./Sidebar";

declare global {
  interface Window {
    /** The smoke test points the import at its own DBeaver files, never the user's. */
    __spelunkDbeaverDir?: string;
  }
}

export function DbeaverImport(props: { onBack(): void; onDone(): void }) {
  const { t } = useTranslation("connections");
  const hasWorkspace = useApp((s) => Boolean(s.workspace?.workspace));
  const [dir, setDir] = useState<string | null>(window.__spelunkDbeaverDir ?? null);
  const [found, setFound] = useState<{ dir: string; items: DbeaverItem[] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [personal, setPersonal] = useState(!hasWorkspace);
  const [savePasswords, setSavePasswords] = useState(true);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<ImportReport | null>(null);

  useEffect(() => {
    let active = true;
    setError(null);
    setFound(null);
    call(commands.scanDbeaver(dir))
      .then((scan) => {
        if (!active) return;
        setFound({ dir: scan.dir, items: scan.connections });
        setChosen(new Set(scan.connections.filter((c) => c.engine).map((c) => c.id)));
      })
      .catch((e: unknown) => active && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      active = false;
    };
  }, [dir]);

  async function pickFolder() {
    const folder = await chooseFolder(t("dbeaver.chooseFolder"));
    if (folder) setDir(folder);
  }

  async function runImport() {
    if (!found) return;
    setBusy(true);
    setError(null);
    try {
      const outcome = await call(commands.importDbeaver(dir, [...chosen], personal, savePasswords));
      setReport(outcome);
      await refreshConnections();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  if (report) {
    return (
      <div className="dbeaver-import">
        <p role="status">
          {t("dbeaver.imported", { count: report.imported.length })}
          {report.tunnels.length > 0 ? ` ${t("dbeaver.tunnels", { count: report.tunnels.length })}` : ""}
        </p>
        {report.imported.length > 0 && (
          <ul className="dbeaver-report">
            {report.imported.map((name) => (
              <li key={name}>{name}</li>
            ))}
          </ul>
        )}
        {report.skipped.length > 0 && (
          <>
            <p className="muted small">{t("dbeaver.skipped")}</p>
            <ul className="dbeaver-report">
              {report.skipped.map((s) => (
                <li key={s.name}>
                  <strong>{s.name}</strong>: <span className="error">{s.reason}</span>
                </li>
              ))}
            </ul>
          </>
        )}
        <div className="wizard-footer">
          <div className="spacer" />
          <button className="primary" onClick={props.onDone}>
            {t("dbeaver.close")}
          </button>
        </div>
      </div>
    );
  }

  const importable = found?.items.filter((c) => c.engine) ?? [];
  return (
    <div className="dbeaver-import">
      <div className="dbeaver-source">
        <span className="muted small">{t("dbeaver.from")}</span>
        <code className="dbeaver-dir" title={found?.dir ?? dir ?? ""}>
          {found?.dir ?? dir ?? t("dbeaver.defaultFolder")}
        </code>
        <button onClick={() => void pickFolder()}>{t("dbeaver.chooseFolder")}</button>
      </div>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {!found && !error && <p className="muted">{t("dbeaver.reading")}</p>}
      {found && found.items.length === 0 && <p className="muted">{t("dbeaver.none")}</p>}
      {found && found.items.length > 0 && (
        <>
          <div className="dbeaver-list" role="list" aria-label={t("dbeaver.list")}>
            {found.items.map((c) => (
              <label key={c.id} className={`dbeaver-item ${c.engine ? "" : "unsupported"}`} role="listitem">
                <input
                  type="checkbox"
                  disabled={!c.engine}
                  checked={chosen.has(c.id)}
                  onChange={(e) => {
                    const next = new Set(chosen);
                    if (e.target.checked) next.add(c.id);
                    else next.delete(c.id);
                    setChosen(next);
                  }}
                  aria-label={c.name}
                />
                <EngineIcon engine={c.engine} />
                <span className="dbeaver-main">
                  <strong>{c.name}</strong>
                  {c.folder && <span className="muted small"> · {c.folder}</span>}
                  <span className="muted small dbeaver-detail">
                    {c.unsupported ?? [c.user && `${c.user}@`, c.target].filter(Boolean).join("")}
                    {c.ssh ? ` · SSH ${c.ssh}` : ""}
                    {c.env ? ` · ${c.env}` : ""}
                    {c.hasPassword ? ` · ${t("dbeaver.passwordSaved")}` : ""}
                  </span>
                </span>
              </label>
            ))}
          </div>
          <fieldset className="dbeaver-options">
            <legend>{t("fields.saveIn")}</legend>
            <label>
              <input type="radio" name="dbeaver-save-in" checked={!personal} disabled={!hasWorkspace} onChange={() => setPersonal(false)} />{" "}
              {t("fields.project")}
            </label>
            <label>
              <input type="radio" name="dbeaver-save-in" checked={personal} onChange={() => setPersonal(true)} /> {t("fields.personal")}
            </label>
            <label className="dbeaver-passwords">
              <input type="checkbox" checked={savePasswords} onChange={(e) => setSavePasswords(e.target.checked)} /> {t("dbeaver.savePasswords")}
            </label>
            <p className="muted small">{t("dbeaver.passwordsHint")}</p>
          </fieldset>
        </>
      )}
      <div className="wizard-footer">
        <button onClick={props.onBack}>{t("back")}</button>
        <div className="spacer" />
        <button
          className="primary"
          disabled={busy || chosen.size === 0 || importable.length === 0}
          onClick={() => void runImport()}
        >
          {t("dbeaver.import", { count: chosen.size })}
        </button>
      </div>
    </div>
  );
}
