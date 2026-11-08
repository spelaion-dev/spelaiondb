// Import connections from another client (DBeaver, DataGrip…): the connections in its files
// (or a folder the user picks), chosen one by one, with what each one would duplicate and what
// would not be imported. The core reads the client's files and decides what is a duplicate;
// passwords never come to the UI, and only go to the keychain when the user asks for it.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ImportItem, type ImportReport } from "../api";
import { chooseFolder } from "../dialogs";
import { EngineIcon } from "./EngineIcon";
import { useApp } from "../store";
import { refreshConnections } from "./Sidebar";

/** The clients connections can be imported from (the core's `ImportClient`). */
export type ImportClientId = "dbeaver" | "datagrip";

export const IMPORT_CLIENTS: { id: ImportClientId; label: string }[] = [
  { id: "dbeaver", label: "DBeaver" },
  { id: "datagrip", label: "DataGrip" },
];

declare global {
  interface Window {
    /** The smoke test points each import at its own files, never the user's. */
    __spelaionImportDirs?: Partial<Record<ImportClientId, string>>;
  }
}

/** What to do with a chosen connection that already exists. */
type DuplicateChoice = "import" | "replace";

export function ConnectionImport(props: { client: ImportClientId; onBack(): void; onDone(): void }) {
  const { t } = useTranslation("connections");
  const client = IMPORT_CLIENTS.find((c) => c.id === props.client)?.label ?? props.client;
  const hasWorkspace = useApp((s) => Boolean(s.workspace?.workspace));
  const [dir, setDir] = useState<string | null>(window.__spelaionImportDirs?.[props.client] ?? null);
  const [found, setFound] = useState<{ dir: string; items: ImportItem[] } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [duplicates, setDuplicates] = useState<Record<string, DuplicateChoice>>({});
  const [personal, setPersonal] = useState(!hasWorkspace);
  const [savePasswords, setSavePasswords] = useState(true);
  const [folder, setFolder] = useState("");
  const [keepFolders, setKeepFolders] = useState(true);
  const [busy, setBusy] = useState(false);
  const [report, setReport] = useState<ImportReport | null>(null);

  const existingOf = (c: ImportItem) => (personal ? c.existingPersonal : c.existingProject);

  useEffect(() => {
    let active = true;
    setError(null);
    setFound(null);
    call(commands.scanConnections(props.client, dir))
      .then((scan) => {
        if (!active) return;
        setFound({ dir: scan.dir, items: scan.connections });
      })
      .catch((e: unknown) => active && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      active = false;
    };
  }, [dir, props.client]);

  // What is chosen at first: every connection that can be imported and does not exist yet
  // where the connections go. Duplicates start left out, so nothing is replaced unasked.
  useEffect(() => {
    if (!found) return;
    setChosen(new Set(found.items.filter((c) => c.engine && !existingOf(c)).map((c) => c.id)));
  }, [found, personal]);

  async function pickFolder() {
    const picked = await chooseFolder(t("importer.chooseFolder"));
    if (picked) setDir(picked);
  }

  async function runImport() {
    if (!found) return;
    setBusy(true);
    setError(null);
    try {
      const choices = found.items
        .filter((c) => chosen.has(c.id))
        .map((c) => ({ id: c.id, action: existingOf(c) ? (duplicates[c.id] ?? "import") : "import" }));
      const outcome = await call(
        commands.importConnections(props.client, dir, choices, {
          personal,
          savePasswords,
          folder,
          keepClientFolders: keepFolders,
        }),
      );
      setReport(outcome);
      await refreshConnections();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  if (report) {
    const done = report.imported.length + report.replaced.length;
    return (
      <div className="conn-import">
        <p role="status">
          {t("importer.imported", { count: report.imported.length })}
          {report.replaced.length > 0 ? ` ${t("importer.replacedCount", { count: report.replaced.length })}` : ""}
          {report.tunnels.length > 0 ? ` ${t("importer.tunnels", { count: report.tunnels.length })}` : ""}
        </p>
        {done > 0 && (
          <ul className="conn-import-report">
            {report.imported.map((name) => (
              <li key={`i-${name}`}>{name}</li>
            ))}
            {report.replaced.map((name) => (
              <li key={`r-${name}`}>
                {name} <span className="muted small">· {t("importer.replacedTag")}</span>
              </li>
            ))}
          </ul>
        )}
        {report.passwordErrors.length > 0 && (
          <>
            <p className="muted small">{t("importer.passwordErrors")}</p>
            <ul className="conn-import-report">
              {report.passwordErrors.map((s) => (
                <li key={s.name}>
                  <strong>{s.name}</strong>: <span className="error">{s.reason}</span>
                </li>
              ))}
            </ul>
          </>
        )}
        {report.skipped.length > 0 && (
          <>
            <p className="muted small">{t("importer.skipped")}</p>
            <ul className="conn-import-report">
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
            {t("importer.close")}
          </button>
        </div>
      </div>
    );
  }

  const importable = found?.items.filter((c) => c.engine) ?? [];
  return (
    <div className="conn-import">
      <div className="conn-import-source">
        <span className="muted small">{t("importer.from")}</span>
        <code className="conn-import-dir" title={found?.dir ?? dir ?? ""}>
          {found?.dir ?? dir ?? t("importer.defaultFolder", { client })}
        </code>
        <button onClick={() => void pickFolder()}>{t("importer.chooseFolder")}</button>
      </div>
      {error && (
        <p className="error" role="alert">
          {error}
        </p>
      )}
      {!found && !error && <p className="muted">{t("importer.reading", { client })}</p>}
      {found && found.items.length === 0 && <p className="muted">{t("importer.none")}</p>}
      {found && found.items.length > 0 && (
        <>
          <div className="conn-import-list" role="list" aria-label={t("importer.list", { client })}>
            {found.items.map((c) => {
              const existing = existingOf(c);
              return (
                <div key={c.id} className={`conn-import-item ${c.engine ? "" : "unsupported"}`} role="listitem">
                  <label className="conn-import-row">
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
                    <span className="conn-import-main">
                      <span>
                        <strong>{c.name}</strong>
                        {c.folder && <span className="muted small"> · {c.folder}</span>}
                      </span>
                      <span className="muted small conn-import-detail">
                        {c.unsupported ?? [c.user && `${c.user}@`, c.target].filter(Boolean).join("")}
                        {c.ssh ? ` · SSH ${c.ssh}` : ""}
                        {c.proxy ? ` · SOCKS ${c.proxy}` : ""}
                        {c.tls ? ` · TLS ${c.tls}` : ""}
                        {c.readOnly ? ` · ${t("importer.readOnly")}` : ""}
                        {c.env ? ` · ${c.env}` : ""}
                        {c.hasPassword ? ` · ${t("importer.passwordSaved")}` : ""}
                      </span>
                    </span>
                  </label>
                  {c.engine && existing && (
                    <div className="conn-import-duplicate">
                      <span className="small">{t("importer.exists", { name: existing.name })}</span>
                      <select
                        aria-label={t("importer.duplicateAction", { name: c.name })}
                        value={chosen.has(c.id) ? (duplicates[c.id] ?? "import") : "skip"}
                        onChange={(e) => {
                          const value = e.target.value;
                          const next = new Set(chosen);
                          if (value === "skip") next.delete(c.id);
                          else {
                            next.add(c.id);
                            setDuplicates({ ...duplicates, [c.id]: value as DuplicateChoice });
                          }
                          setChosen(next);
                        }}
                      >
                        <option value="skip">{t("importer.skip")}</option>
                        <option value="import">{t("importer.importCopy")}</option>
                        <option value="replace">{t("importer.replace", { name: existing.name })}</option>
                      </select>
                    </div>
                  )}
                  {c.unmapped.length > 0 && (
                    <details className="conn-import-unmapped">
                      <summary className="small">{t("importer.unmapped", { count: c.unmapped.length })}</summary>
                      <ul className="small">
                        {c.unmapped.map((line) => (
                          <li key={line}>{line}</li>
                        ))}
                      </ul>
                    </details>
                  )}
                </div>
              );
            })}
          </div>
          <fieldset className="conn-import-options">
            <legend>{t("fields.saveIn")}</legend>
            <label>
              <input type="radio" name="import-save-in" checked={!personal} disabled={!hasWorkspace} onChange={() => setPersonal(false)} />{" "}
              {t("fields.project")}
            </label>
            <label>
              <input type="radio" name="import-save-in" checked={personal} onChange={() => setPersonal(true)} /> {t("fields.personal")}
            </label>
          </fieldset>
          <fieldset className="conn-import-options">
            <legend>{t("importer.folder")}</legend>
            <label className="conn-import-folder">
              <span className="small">{t("importer.folderField")}</span>
              <input
                value={folder}
                onChange={(e) => setFolder(e.target.value)}
                placeholder={t("importer.topLevel")}
                spellCheck={false}
                autoComplete="off"
              />
            </label>
            <label>
              <input type="checkbox" checked={keepFolders} onChange={(e) => setKeepFolders(e.target.checked)} />{" "}
              {t("importer.keepFolders", { client })}
            </label>
          </fieldset>
          <fieldset className="conn-import-options">
            <legend>{t("importer.passwords")}</legend>
            <label>
              <input type="checkbox" checked={savePasswords} onChange={(e) => setSavePasswords(e.target.checked)} />{" "}
              {t("importer.savePasswords", { client })}
            </label>
            <p className="muted small">{t("importer.passwordsHint")}</p>
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
          {busy ? t("importer.importing") : t("importer.import", { count: chosen.size })}
        </button>
      </div>
    </div>
  );
}
