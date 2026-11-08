// The export dialog (ADR-034): what to export (the loaded rows, a selection, or every row by
// running the query or the table's SELECT again in a new session), the format and the CSV
// options. The core writes the file; only counts and the options pass through here.

import { useEffect, useMemo, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands, type CellRange, type TextFormat } from "../api";
import type { ExportFileOptions, ExportOutcome, ExportProgressItem, RerunCheckItem } from "../bindings";
import { chooseFolder, chooseSavePath } from "../dialogs";
import { useApp, useDialogs, type ResultMeta } from "../store";

/** What the dialog exports. */
export type ExportRequest =
  /** A result of an editor or table data tab. `sql` produced it (null when unknown);
   *  `table` is the table data tab's view, whose every row the core can export. */
  | {
      kind: "result";
      session: string;
      connectionId: string | null;
      result: ResultMeta;
      sql: string | null;
      table: { schema: string; name: string; filter: string; orderBy: string } | null;
      /** Columns hidden in the grid. */
      hidden: number[];
    }
  /** A selection of a loaded result: its rows and shown columns. */
  | { kind: "selection"; session: string; result: ResultMeta; range: CellRange }
  /** Tables and views chosen in the explorer; `session` is the explorer's. */
  | { kind: "tables"; session: string; connectionId: string; tables: { schema: string; name: string }[] };

export const useExport = create<{ request: ExportRequest | null; open(request: ExportRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

const FORMATS: { format: TextFormat; extension: string }[] = [
  { format: "csv", extension: "csv" },
  { format: "tsv", extension: "tsv" },
  { format: "json", extension: "json" },
  { format: "sqlInsert", extension: "sql" },
  { format: "markdown", extension: "md" },
  { format: "html", extension: "html" },
];

const DELIMITERS = [",", ";", "\t", "|"];
type Encoding = ExportFileOptions["encoding"];
const ENCODINGS: Encoding[] = ["utf8Bom", "utf8", "utf16Le", "windows1252"];

type Scope = "loaded" | "all" | "selection";

type Phase =
  | { kind: "options" }
  | { kind: "running"; rows: number; bytes: number; table: number | null }
  | { kind: "done"; text: string }
  | { kind: "failed"; text: string; fallback: string | null };

/** Ids the core keys running exports by (to cancel them). */
let nextExport = Math.floor(Math.random() * 1_000_000);

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function ExportDialog() {
  const request = useExport((s) => s.request);
  if (!request) return null;
  return <ExportForm key={`${request.kind}-${request.session}-${"result" in request ? request.result.id : request.tables.length}`} request={request} />;
}

function ExportForm({ request }: { request: ExportRequest }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const close = useExport((s) => s.close);
  const connections = useApp((s) => s.connections);
  const connectionId = request.kind === "selection" ? null : request.connectionId;
  const connection = connections.find((c) => c.id === connectionId) ?? null;
  const result = request.kind === "tables" ? null : request.result;
  const canStream = request.kind === "result" && (request.sql !== null || request.table !== null);
  const [check, setCheck] = useState<RerunCheckItem | null>(null);
  const loadedAll = !!result?.complete && !result.paging?.hasMore && !(request.kind === "result" && request.table);
  const [scope, setScope] = useState<Scope>(
    request.kind === "selection" ? "selection" : request.kind === "tables" ? "all" : loadedAll || !canStream ? "loaded" : "all",
  );
  const [shownOnly, setShownOnly] = useState(true);
  const [format, setFormat] = useState<TextFormat>("csv");
  const [delimiter, setDelimiter] = useState(",");
  const [otherDelimiter, setOtherDelimiter] = useState("");
  const [header, setHeader] = useState(true);
  const [nullText, setNullText] = useState("");
  const [encoding, setEncoding] = useState<Encoding>("utf8Bom");
  const [escapeFormulas, setEscapeFormulas] = useState(true);
  const [phase, setPhase] = useState<Phase>({ kind: "options" });
  const [exportId, setExportId] = useState<number | null>(null);
  const [lastPath, setLastPath] = useState<string | null>(null);

  // The core says whether the query may run again and whether the tab holds a transaction.
  useEffect(() => {
    if (request.kind !== "result" || !canStream) return;
    let current = true;
    void call(commands.exportRerunCheck(request.session, request.table ? null : request.sql))
      .then((c) => {
        if (!current) return;
        setCheck(c);
        if (c.refused && scope === "all") setScope("loaded");
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
    // Only when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const running = phase.kind === "running";
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !running) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, close]);

  const text = format === "csv" || format === "tsv";
  const extension = FORMATS.find((f) => f.format === format)?.extension ?? "txt";
  const chosenDelimiter = delimiter === "other" ? otherDelimiter : delimiter;
  const hidden = request.kind === "result" ? request.hidden : [];
  const columns =
    request.kind === "result" && shownOnly && hidden.length > 0
      ? request.result.columns.map((_, i) => i).filter((i) => !hidden.includes(i))
      : null;
  const options = (): ExportFileOptions => ({
    format,
    header,
    delimiter: format === "csv" ? chosenDelimiter : "\t",
    nullText,
    escapeFormulas,
    encoding: text ? encoding : "utf8",
    columns,
  });
  const allRefused = request.kind === "result" && (!canStream || !!check?.refused);
  const base =
    request.kind === "tables"
      ? (request.tables[0]?.name ?? "export")
      : request.kind === "result" && request.table
        ? request.table.name
        : "result";

  function bytes(n: number): string {
    if (n < 1024) return `${numbers.format(n)} B`;
    if (n < 1024 * 1024) return `${numbers.format(Math.round(n / 1024))} KB`;
    return `${new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }).format(n / (1024 * 1024))} MB`;
  }

  function doneText(rows: number, path: string, replaced: number): string {
    const done = t("export.done", { count: rows, formattedCount: numbers.format(rows), path });
    return replaced > 0 ? `${done} ${t("export.replaced", { count: replaced, formattedCount: numbers.format(replaced) })}` : done;
  }

  async function askPath(): Promise<string | null> {
    if (request.kind === "tables" && request.tables.length > 1) return chooseFolder(t("export.chooseFolder"));
    return chooseSavePath({
      defaultPath: `${base}.${extension}`,
      filters: [{ name: t(`results.export.${format}`), extensions: [extension] }],
    });
  }

  async function exportLoaded(path: string, range: CellRange | null) {
    if (!result) return;
    setPhase({ kind: "running", rows: 0, bytes: 0, table: null });
    try {
      const summary = await call(commands.exportResult(result.id, request.session, path, options(), range));
      setPhase({ kind: "done", text: doneText(summary.rows ?? 0, path, summary.replaced ?? 0) });
    } catch (e) {
      setPhase({ kind: "failed", text: message(e), fallback: null });
    }
  }

  async function start(path?: string, confirmed = false) {
    const target = path ?? (await askPath().catch((e: unknown) => {
      setPhase({ kind: "failed", text: message(e), fallback: null });
      return null;
    }));
    if (!target) return;
    setLastPath(target);
    if (request.kind === "selection") return exportLoaded(target, request.range);
    if (request.kind === "result" && scope === "loaded") return exportLoaded(target, null);
    // A new run: a connection type that confirms execution asks first (the core refuses
    // an unconfirmed one too).
    const sql = request.kind === "result" && !request.table ? request.sql : null;
    if (sql && !confirmed && connection?.kind?.safeguards.confirmExecution) {
      if (!(await useDialogs.getState().askRisks(connection.name, [{ sql, risk: "execution" }]))) return;
      confirmed = true;
    }
    const id = ++nextExport;
    setExportId(id);
    setPhase({ kind: "running", rows: 0, bytes: 0, table: null });
    const channel = new Channel<ExportProgressItem>();
    channel.onmessage = (p) => setPhase((current) => (current.kind === "running" ? { kind: "running", rows: p.rows ?? 0, bytes: p.bytes ?? 0, table: p.table ?? null } : current));
    try {
      if (request.kind === "tables" && request.tables.length > 1) {
        const outcome = await call(commands.exportTables(request.session, id, request.tables, target, extension, options(), channel));
        const rows = outcome.files.reduce((sum, f) => sum + (f.summary.rows ?? 0), 0);
        const written = t("export.tablesDone", { count: outcome.files.length, formattedCount: numbers.format(outcome.files.length), rows: numbers.format(rows), folder: target });
        if (outcome.cancelled) setPhase({ kind: "done", text: `${t("export.cancelled")} ${written}` });
        else if (outcome.error != null) {
          const failed = request.tables[outcome.failedTable ?? 0]?.name ?? "";
          setPhase({ kind: "failed", text: `${t("export.tableFailed", { table: failed, reason: outcome.error })} ${written}`, fallback: null });
        } else setPhase({ kind: "done", text: written });
        return;
      }
      let outcome: ExportOutcome;
      if (request.kind === "tables") {
        const table = request.tables[0];
        if (!table) return;
        outcome = await call(commands.exportTableData(request.session, id, { ...table, filter: "", orderBy: "" }, target, options(), channel));
      } else if (request.table) {
        outcome = await call(commands.exportTableData(request.session, id, request.table, target, options(), channel));
      } else {
        outcome = await call(commands.exportQuery(request.session, id, request.sql ?? "", target, options(), confirmed, channel));
      }
      const fallback = result?.complete ? target : null;
      switch (outcome.kind) {
        case "done":
          setPhase({ kind: "done", text: doneText(outcome.summary.rows ?? 0, target, outcome.summary.replaced ?? 0) });
          break;
        case "cancelled":
          setPhase({ kind: "done", text: t("export.cancelledNoFile") });
          break;
        case "needsConfirmation":
          if (!sql || !(await useDialogs.getState().askRisks(connection?.name ?? "", [{ sql, risk: "execution" }]))) {
            setPhase({ kind: "options" });
            return;
          }
          await start(target, true);
          return;
        case "notARead":
          setPhase({ kind: "failed", text: t("export.notARead", { reason: outcome.message }), fallback });
          break;
        case "failed":
          setPhase({ kind: "failed", text: t("export.failedAgain", { reason: outcome.message }), fallback });
          break;
      }
    } catch (e) {
      setPhase({ kind: "failed", text: message(e), fallback: null });
    } finally {
      setExportId(null);
    }
  }

  const title =
    request.kind === "tables"
      ? request.tables.length === 1
        ? t("export.titleTable", { name: request.tables[0]?.name ?? "" })
        : t("export.titleTables", { count: request.tables.length, formattedCount: numbers.format(request.tables.length) })
      : request.kind === "selection"
        ? t("export.titleSelection")
        : t("export.title");

  const tableName = phase.kind === "running" && phase.table !== null && request.kind === "tables" ? request.tables[phase.table]?.name : null;

  return (
    <div className="modal-backdrop">
      <div className="modal export-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        {phase.kind === "options" && (
          <>
            {request.kind === "result" && (
              <fieldset className="export-scope">
                <legend>{t("export.rows")}</legend>
                <label className="check">
                  <input type="radio" name="export-scope" checked={scope === "loaded"} disabled={!request.result.complete} onChange={() => setScope("loaded")} />
                  {t("export.loaded", { count: request.result.rows, formattedCount: numbers.format(request.result.rows) })}
                </label>
                <label className="check">
                  <input type="radio" name="export-scope" checked={scope === "all"} disabled={allRefused} onChange={() => setScope("all")} />
                  {request.table ? t("export.allTable") : t("export.allQuery")}
                </label>
                {!canStream && <p className="muted small">{t("export.noQuery")}</p>}
                {check?.refused && (
                  <p className="muted small" role="note">
                    {t("export.refused", { reason: check.refused })}
                  </p>
                )}
                {scope === "all" && (
                  <p className="muted small" role="note">
                    {t("export.newSession")}
                  </p>
                )}
                {scope === "all" && check?.openTransaction && (
                  <p className="warning small" role="alert">
                    {t("export.openTransaction")}
                  </p>
                )}
              </fieldset>
            )}
            {request.kind === "selection" && (
              <p className="muted">
                {t("export.selection", {
                  count: request.range.rows,
                  formattedCount: numbers.format(request.range.rows),
                  columns: numbers.format(request.range.columns.length),
                })}
              </p>
            )}
            {request.kind === "tables" && request.tables.length > 1 && <p className="muted small">{t("export.tablesHint")}</p>}
            {hidden.length > 0 && (
              <label className="check">
                <input type="checkbox" checked={shownOnly} onChange={(e) => setShownOnly(e.target.checked)} />
                {t("export.shownOnly", { count: hidden.length, formattedCount: numbers.format(hidden.length) })}
              </label>
            )}
            <label className="field">
              {t("export.format")}
              <select value={format} onChange={(e) => setFormat(e.target.value as TextFormat)}>
                {FORMATS.map((f) => (
                  <option key={f.format} value={f.format}>
                    {t(`results.export.${f.format}`)}
                  </option>
                ))}
              </select>
            </label>
            {text && (
              <fieldset className="export-csv">
                <legend>{t("export.csvOptions")}</legend>
                {format === "csv" && (
                  <label className="field">
                    {t("export.delimiter")}
                    <select value={delimiter} onChange={(e) => setDelimiter(e.target.value)}>
                      {DELIMITERS.map((d) => (
                        <option key={d} value={d}>
                          {t(`export.delimiters.${d === "\t" ? "tab" : d === "," ? "comma" : d === ";" ? "semicolon" : "pipe"}`)}
                        </option>
                      ))}
                      <option value="other">{t("export.delimiters.other")}</option>
                    </select>
                    {delimiter === "other" && (
                      <input aria-label={t("export.otherDelimiter")} value={otherDelimiter} maxLength={1} onChange={(e) => setOtherDelimiter(e.target.value)} />
                    )}
                  </label>
                )}
                <label className="check">
                  <input type="checkbox" checked={header} onChange={(e) => setHeader(e.target.checked)} />
                  {t("export.header")}
                </label>
                <label className="field">
                  {t("export.nullText")}
                  <input value={nullText} placeholder={t("export.nullEmpty")} onChange={(e) => setNullText(e.target.value)} />
                </label>
                <label className="field">
                  {t("export.encoding")}
                  <select value={encoding} onChange={(e) => setEncoding(e.target.value as Encoding)}>
                    {ENCODINGS.map((value) => (
                      <option key={value} value={value}>
                        {t(`export.encodings.${value}`)}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="check" title={t("export.escapeHint")}>
                  <input type="checkbox" checked={escapeFormulas} onChange={(e) => setEscapeFormulas(e.target.checked)} />
                  {t("export.escapeFormulas")}
                </label>
              </fieldset>
            )}
            <div className="modal-actions">
              <button onClick={close}>{t("export.close")}</button>
              <button className="primary" disabled={format === "csv" && chosenDelimiter.length !== 1} onClick={() => void start()}>
                {t("export.start")}
              </button>
            </div>
          </>
        )}
        {phase.kind === "running" && (
          <>
            <p role="status" className="export-progress">
              {tableName != null && request.kind === "tables"
                ? `${t("export.tableProgress", { index: numbers.format((phase.table ?? 0) + 1), total: numbers.format(request.tables.length), table: tableName })} · `
                : ""}
              {t("export.progress", { count: phase.rows, formattedCount: numbers.format(phase.rows), size: bytes(phase.bytes) })}
            </p>
            <div className="modal-actions">
              <button className="danger" disabled={exportId === null} onClick={() => exportId !== null && void call(commands.cancelExport(exportId)).catch(() => undefined)}>
                {t("export.cancel")}
              </button>
            </div>
          </>
        )}
        {phase.kind === "done" && (
          <>
            <p role="status" className="export-result">
              {phase.text}
            </p>
            <div className="modal-actions">
              <button className="primary" onClick={close}>
                {t("export.close")}
              </button>
            </div>
          </>
        )}
        {phase.kind === "failed" && (
          <>
            <p role="alert" className="error export-result">
              {phase.text}
            </p>
            <div className="modal-actions">
              <button onClick={() => setPhase({ kind: "options" })}>{t("export.back")}</button>
              {phase.fallback && lastPath && (
                <button
                  className="primary"
                  onClick={() => {
                    setScope("loaded");
                    void exportLoaded(lastPath, null);
                  }}
                >
                  {t("export.loadedInstead", { count: result?.rows ?? 0, formattedCount: numbers.format(result?.rows ?? 0) })}
                </button>
              )}
              <button onClick={close}>{t("export.close")}</button>
            </div>
          </>
        )}
      </div>
    </div>
  );
}
