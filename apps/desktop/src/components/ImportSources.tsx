// The first step of importing connections (SDB-446): every client, with what the core found of
// it on this computer, asked for all at once and filled in as each answers. Clients with
// connections come first. A row opens that client's preview (`ConnectionImport`), from where the
// client keeps its connections or from a folder or export file chosen on the row. The counts are
// the core's (`ClientSourceItem`, read as it is): it reads no saved password to count them.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { ClientSourceItem } from "../bindings";
import { chooseFolder, chooseOpenFile } from "../dialogs";
import { errorText } from "../dialogState";
import { IMPORT_CLIENTS, type ImportClientId } from "./ConnectionImport";

/** One row while the core counts, once it answered, or when the command itself failed. */
type Row = { kind: "counting" } | { kind: "answered"; source: ClientSourceItem } | { kind: "failed"; message: string };

/** Whether a row has connections to show first. */
const hasConnections = (row: Row) => row.kind === "answered" && row.source.kind === "found" && row.source.count > 0;

export function ImportSources(props: { onPick(client: ImportClientId, path: string | null): void; onBack(): void }) {
  const { t, i18n } = useTranslation("connections");
  const [rows, setRows] = useState<Partial<Record<ImportClientId, Row>>>({});

  useEffect(() => {
    let active = true;
    for (const client of IMPORT_CLIENTS) {
      // The smoke test points each client at its own files, never the user's.
      const path = window.__spelaionImportDirs?.[client.id] ?? null;
      call(commands.clientImportSource(client.id, path))
        .then((source) => active && setRows((r) => ({ ...r, [client.id]: { kind: "answered", source } })))
        .catch((e: unknown) => active && setRows((r) => ({ ...r, [client.id]: { kind: "failed", message: errorText(e) } })));
    }
    return () => {
      active = false;
    };
  }, []);

  /** What the core found, in one line. */
  function status(row: Row): string {
    switch (row.kind) {
      case "counting":
        return t("importer.sources.counting");
      case "failed":
        return row.message;
      case "answered": {
        const source = row.source;
        switch (source.kind) {
          case "found":
            return t("importer.sources.found", { count: source.count, formattedCount: source.count.toLocaleString(i18n.language) });
          case "notFound":
            return t("importer.sources.notFound");
          case "needsFile":
            return t("importer.sources.needsFile");
          case "encrypted":
            return t("importer.sources.encrypted");
          case "failed":
            return t("importer.sources.failed", { reason: source.reason });
          default: {
            const unknown: never = source;
            return unknown;
          }
        }
      }
      default: {
        const unknown: never = row;
        return unknown;
      }
    }
  }

  async function pickFolder(client: ImportClientId) {
    const picked = await chooseFolder(t("importer.chooseFolder"));
    if (picked) props.onPick(client, picked);
  }

  async function pickExport(client: ImportClientId, label: string, extensions: string[]) {
    const picked = await chooseOpenFile(t("importer.chooseFile"), [
      { name: t("importer.exportFiles", { client: label }), extensions },
      { name: t("importer.allFiles"), extensions: ["*"] },
    ]);
    if (picked) props.onPick(client, picked);
  }

  // Clients with connections first; otherwise the order of IMPORT_CLIENTS.
  const ordered = [...IMPORT_CLIENTS]
    .map((client) => ({ client, row: rows[client.id] ?? ({ kind: "counting" } as Row) }))
    .sort((a, b) => Number(hasConnections(b.row)) - Number(hasConnections(a.row)));

  return (
    <div className="conn-import">
      <p className="muted small">{t("importer.sources.intro")}</p>
      <div className="conn-import-list" role="list" aria-label={t("importer.sources.list")}>
        {ordered.map(({ client, row }) => {
          const source = row.kind === "answered" ? row.source : null;
          // Where the client keeps something readable here, the row opens it as it is.
          const opens = source?.kind === "found" || source?.kind === "encrypted";
          const choices = source !== null && !opens;
          return (
            <div key={client.id} className="conn-import-item" role="listitem" aria-label={client.label} data-client={client.id} aria-busy={row.kind === "counting"}>
              <div className="conn-import-row">
                <div className="conn-import-main">
                  <strong>{client.label}</strong>
                  <span className={source?.kind === "failed" || row.kind === "failed" ? "error small" : "muted small"} role="status">
                    {status(row)}
                  </span>
                </div>
                <span className="spacer" />
                {opens && (
                  <button
                    className={hasConnections(row) ? "primary" : undefined}
                    onClick={() => props.onPick(client.id, window.__spelaionImportDirs?.[client.id] ?? null)}
                  >
                    {t("importer.sources.open", { client: client.label })}
                  </button>
                )}
                {choices && client.id !== "heidisql" && <button onClick={() => void pickFolder(client.id)}>{t("importer.chooseFolder")}</button>}
                {choices && client.exportExtensions && (
                  <button onClick={() => void pickExport(client.id, client.label, client.exportExtensions ?? [])}>{t("importer.chooseFile")}</button>
                )}
              </div>
            </div>
          );
        })}
      </div>
      <div className="wizard-footer">
        <button onClick={props.onBack}>{t("back")}</button>
      </div>
    </div>
  );
}
