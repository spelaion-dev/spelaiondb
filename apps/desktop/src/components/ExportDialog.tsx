// The export dialog (ADR-034): what to export (the loaded rows, a selection, or every row by
// running the query or the table's SELECT again in a new session), the format and its options
// (CSV, SQL, JSON, the file's name and gzip: SDB-391). The core writes the file and names it;
// only counts and the options pass through here. It opens with the last export's choices,
// which the core keeps and checks as personal settings (SDB-66).

import { useEffect, useMemo, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands, type CellRange, type TextFormat } from "../api";
import type { ExportDefaultsItem, ExportFileOptions, ExportSummaryItem, ExportProgressItem, FileNamePatternItem, JobOutcome, JsonOptionsItem, RerunCheckItem, SqlOptionsItem } from "../bindings";
import { chooseFolder, chooseSavePath } from "../dialogs";
import { errorText as message } from "../dialogState";
import { useApp, useDialogs, type ResultMeta } from "../store";
import { ended, partialText } from "../resultEnd";
import { startJob } from "../jobs";
import { useJobPhase } from "../useDialogState";

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
  { format: "xlsx", extension: "xlsx" },
  { format: "tsv", extension: "tsv" },
  { format: "json", extension: "json" },
  { format: "sqlInsert", extension: "sql" },
  { format: "markdown", extension: "md" },
  { format: "html", extension: "html" },
  { format: "ndjson", extension: "ndjson" },
  { format: "parquet", extension: "parquet" },
  { format: "arrowIpc", extension: "arrow" },
];

const DELIMITERS = [",", ";", "\t", "|"];
/** Most rows an INSERT may hold: the core's `MAX_ROWS_PER_INSERT`, which checks it again. */
const MAX_ROWS_PER_INSERT = 10_000;
type SqlOptions = SqlOptionsItem;
type JsonOptions = JsonOptionsItem;
type Encoding = ExportFileOptions["encoding"];
const ENCODINGS: Encoding[] = ["utf8Bom", "utf8", "utf16Le", "windows1252"];

type Scope = "loaded" | "all" | "selection";

/** The rows and bytes written, and the table being written of several. */
type Progress = { rows: number; bytes: number; table: number | null };
/** A failed export, and the file its loaded rows can still be written to. */
type Failure = { text: string; fallback: string | null };

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
  // A partial result (SDB-367) is not every row: exporting all of them runs the query again.
  const partial = result ? partialText(result.end, result.total ?? result.rows, t, numbers) : null;
  const loadedAll = !!result && result.end.kind === "whole" && !result.paging?.hasMore && !(request.kind === "result" && request.table);
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
  const [gzip, setGzip] = useState(false);
  const [names, setNames] = useState<FileNamePatternItem>({ pattern: "{table}", timestampFormat: "%Y%m%d-%H%M%S" });
  const [sql, setSql] = useState<SqlOptions>({ rowsPerInsert: 1, omitSchema: false, keywordCase: "upper", nameCase: "asIs", upsert: false });
  const [json, setJson] = useState<JsonOptions>({ isoDates: false, jsonAsJson: true });
  // Idle is the options form.
  const job = useJobPhase<Progress, { text: string }, Failure>();
  const { phase, running } = job;
  const exportId = phase.kind === "running" ? phase.id : null;
  const [lastPath, setLastPath] = useState<string | null>(null);
  // The options are shown once the last export's choices are in (or could not be read).
  const [ready, setReady] = useState(false);

  // The last export's choices, as the core read them back (each unusable one at its default).
  useEffect(() => {
    let current = true;
    void call(commands.exportDefaults())
      .then((d) => {
        if (!current) return;
        setFormat(d.format);
        if (DELIMITERS.includes(d.delimiter)) setDelimiter(d.delimiter);
        else {
          setDelimiter("other");
          setOtherDelimiter(d.delimiter);
        }
        setHeader(d.header);
        setNullText(d.nullText);
        setEncoding(d.encoding);
        setEscapeFormulas(d.escapeFormulas);
        setGzip(d.gzip);
        setNames(d.names);
        setSql(d.sql);
        setJson(d.json);
      })
      .catch(() => undefined)
      .finally(() => {
        if (current) setReady(true);
      });
    return () => {
      current = false;
    };
  }, []);

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

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !running) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, close]);

  const text = format === "csv" || format === "tsv";
  // Parquet compresses inside the file and a workbook is a zip already; the other formats
  // become `.gz` files (as the core says).
  const workbook = format === "xlsx";
  const baseExtension = FORMATS.find((f) => f.format === format)?.extension ?? "txt";
  const useGzip = gzip && !workbook;
  const extension = useGzip && format !== "parquet" ? `${baseExtension}.gz` : baseExtension;
  // An upsert needs a table, whose key the core reads from the catalog: a table's every row.
  const upsertPossible = request.kind === "tables" || (request.kind === "result" && request.table !== null && scope === "all");
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
    gzip: useGzip,
    sql: { ...sql, upsert: sql.upsert && upsertPossible },
    json,
  });
  /** What the dialog shows, kept as the next export's defaults. */
  const defaults = (): ExportDefaultsItem => ({
    format,
    delimiter: chosenDelimiter,
    header,
    nullText,
    encoding,
    escapeFormulas,
    gzip,
    names,
    sql,
    json,
  });
  const allRefused = request.kind === "result" && (!canStream || !!check?.refused);
  /** The table a single file is named after, if any (the core names a query's `result`). */
  const namedTable =
    request.kind === "tables"
      ? (request.tables[0] ?? null)
      : request.kind === "result" && request.table
        ? { schema: request.table.schema, name: request.table.name }
        : null;

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
    // The name the pattern gives, written by the core; the save dialog picks the folder.
    const name = await call(commands.exportFileName(request.session, names, format, useGzip, namedTable));
    return chooseSavePath({
      defaultPath: name,
      filters: [{ name: t(`results.export.${format}`), extensions: [extension] }],
    });
  }

  async function exportLoaded(path: string, range: CellRange | null) {
    if (!result) return;
    // Written from the loaded rows at once: nothing to cancel.
    job.start(null, { rows: 0, bytes: 0, table: null });
    try {
      const summary = await call(commands.exportResult(result.id, request.session, path, options(), range));
      job.done({ text: doneText(summary.rows ?? 0, path, summary.replaced ?? 0) });
    } catch (e) {
      job.failed({ text: message(e), fallback: null });
    }
  }

  async function start(path?: string, confirmed = false) {
    const target = path ?? (await askPath().catch((e: unknown) => {
      job.failed({ text: message(e), fallback: null });
      return null;
    }));
    if (!target) return;
    setLastPath(target);
    // The choices of an export that starts are the next one's defaults; the core refuses
    // options it could not use again, and the export then says what is wrong with them.
    if (path === undefined) await call(commands.setExportDefaults(defaults())).catch(() => undefined);
    if (request.kind === "selection") return exportLoaded(target, request.range);
    if (request.kind === "result" && scope === "loaded") return exportLoaded(target, null);
    // A new run: a connection type that confirms execution asks first (the core refuses
    // an unconfirmed one too).
    const sql = request.kind === "result" && !request.table ? request.sql : null;
    if (sql && !confirmed && connection?.kind?.safeguards.confirmExecution) {
      if (!(await useDialogs.getState().askRisks(connection.name, [{ sql, risk: "execution" }]))) return;
      confirmed = true;
    }
    const id = startJob();
    job.start(id, { rows: 0, bytes: 0, table: null });
    const channel = new Channel<ExportProgressItem>();
    channel.onmessage = (p) => job.progress(id, { rows: p.rows ?? 0, bytes: p.bytes ?? 0, table: p.table ?? null });
    try {
      if (request.kind === "tables" && request.tables.length > 1) {
        const outcome = await call(commands.exportTables(request.session, id.channel, request.tables, target, names, options(), channel));
        // The files written stay, whichever way it ended.
        const files = outcome.kind === "done" || outcome.kind === "stoppedAtLimit" ? outcome.result : outcome.kind === "cancelled" || outcome.kind === "failed" ? outcome.kept.files : [];
        const rows = files.reduce((sum, f) => sum + (f.summary.rows ?? 0), 0);
        // As XLSX the tables are the sheets of one workbook.
        const written = workbook
          ? t("export.workbookDone", { count: files.length, formattedCount: numbers.format(files.length), rows: numbers.format(rows), path: files[0]?.path ?? target })
          : t("export.tablesDone", { count: files.length, formattedCount: numbers.format(files.length), rows: numbers.format(rows), folder: target });
        switch (outcome.kind) {
          case "done":
          case "stoppedAtLimit":
            job.done({ text: written });
            break;
          case "cancelled":
            job.done({ text: `${t("export.cancelled")} ${written}` });
            break;
          case "failed": {
            const failed = request.tables[outcome.kept.table]?.name ?? "";
            job.failed({ text: `${t("export.tableFailed", { table: failed, reason: outcome.message })} ${written}`, fallback: null });
            break;
          }
          // The core writes these reads itself: nothing is asked or refused before they run.
          case "notARead":
            job.failed({ text: t("export.notARead", { reason: outcome.message }), fallback: null });
            break;
          case "needsConfirmation":
            job.failed({ text: t("export.failedAgain", { reason: t("runFile.notConfirmed") }), fallback: null });
            break;
          case "passwordRequired":
            job.failed({ text: t("export.failedAgain", { reason: t("backup.passwordRequired") }), fallback: null });
            break;
          default: {
            const unexpected: never = outcome;
            return unexpected;
          }
        }
        return;
      }
      let outcome: JobOutcome<ExportSummaryItem, null>;
      if (request.kind === "tables") {
        const table = request.tables[0];
        if (!table) return;
        outcome = await call(commands.exportTableData(request.session, id.channel, { ...table, filter: "", orderBy: "" }, target, options(), channel));
      } else if (request.table) {
        outcome = await call(commands.exportTableData(request.session, id.channel, request.table, target, options(), channel));
      } else {
        outcome = await call(commands.exportQuery(request.session, id.channel, request.sql ?? "", target, options(), confirmed, channel));
      }
      const fallback = ended(result) ? target : null;
      switch (outcome.kind) {
        case "done":
        case "stoppedAtLimit":
          job.done({ text: doneText(outcome.result.rows ?? 0, target, outcome.result.replaced ?? 0) });
          break;
        case "cancelled":
          job.done({ text: t("export.cancelledNoFile") });
          break;
        case "needsConfirmation":
          if (!sql || !(await useDialogs.getState().askRisks(connection?.name ?? "", [{ sql, risk: "execution" }]))) {
            job.reset();
            return;
          }
          await start(target, true);
          return;
        case "notARead":
          job.failed({ text: t("export.notARead", { reason: outcome.message }), fallback });
          break;
        case "failed":
          job.failed({ text: t("export.failedAgain", { reason: outcome.message }), fallback });
          break;
        // The export's session is opened from the tab's, with its password.
        case "passwordRequired":
          job.failed({ text: t("export.failedAgain", { reason: t("backup.passwordRequired") }), fallback });
          break;
        default: {
          const unexpected: never = outcome;
          return unexpected;
        }
      }
    } catch (e) {
      job.failed({ text: message(e), fallback: null });
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

  const tableName = phase.kind === "running" && phase.progress.table !== null && request.kind === "tables" ? request.tables[phase.progress.table]?.name : null;

  return (
    <div className="modal-backdrop">
      <div className="modal export-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        {phase.kind === "idle" && ready && (
          <>
            {request.kind === "result" && (
              <fieldset className="export-scope">
                <legend>{t("export.rows")}</legend>
                <label className="check">
                  <input type="radio" name="export-scope" checked={scope === "loaded"} disabled={!ended(request.result)} onChange={() => setScope("loaded")} />
                  {t("export.loaded", { count: request.result.rows, formattedCount: numbers.format(request.result.rows) })}
                </label>
                {partial && (
                  <p className="warning small" role="note">
                    {t("results.partialExport", { partial })}
                  </p>
                )}
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
            {request.kind === "tables" && request.tables.length > 1 && <p className="muted small">{t(workbook ? "export.tablesWorkbookHint" : "export.tablesHint")}</p>}
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
            {format === "sqlInsert" && (
              <fieldset className="export-csv">
                <legend>{t("export.sqlOptions")}</legend>
                <label className="field">
                  {t("export.rowsPerInsert")}
                  <input
                    type="number"
                    min={1}
                    max={MAX_ROWS_PER_INSERT}
                    value={sql.rowsPerInsert}
                    onChange={(e) => setSql({ ...sql, rowsPerInsert: Math.max(1, Math.min(MAX_ROWS_PER_INSERT, Math.trunc(Number(e.target.value) || 1))) })}
                  />
                </label>
                <label className="field">
                  {t("export.keywordCase")}
                  <select value={sql.keywordCase} onChange={(e) => setSql({ ...sql, keywordCase: e.target.value })}>
                    <option value="upper">{t("export.keywordCases.upper")}</option>
                    <option value="lower">{t("export.keywordCases.lower")}</option>
                  </select>
                </label>
                <label className="field">
                  {t("export.nameCase")}
                  <select value={sql.nameCase} onChange={(e) => setSql({ ...sql, nameCase: e.target.value })}>
                    <option value="asIs">{t("export.nameCases.asIs")}</option>
                    <option value="lower">{t("export.nameCases.lower")}</option>
                    <option value="upper">{t("export.nameCases.upper")}</option>
                  </select>
                </label>
                <label className="check">
                  <input type="checkbox" checked={sql.omitSchema} onChange={(e) => setSql({ ...sql, omitSchema: e.target.checked })} />
                  {t("export.omitSchema")}
                </label>
                <label className="check" title={t("export.upsertHint")}>
                  <input type="checkbox" checked={sql.upsert && upsertPossible} disabled={!upsertPossible} onChange={(e) => setSql({ ...sql, upsert: e.target.checked })} />
                  {t("export.upsert")}
                </label>
                <p className="muted small">{t(upsertPossible ? "export.upsertHint" : "export.upsertTableOnly")}</p>
              </fieldset>
            )}
            {(format === "json" || format === "ndjson") && (
              <fieldset className="export-csv">
                <legend>{t("export.jsonOptions")}</legend>
                <label className="check">
                  <input type="checkbox" checked={json.isoDates} onChange={(e) => setJson({ ...json, isoDates: e.target.checked })} />
                  {t("export.isoDates")}
                </label>
                <label className="check">
                  <input type="checkbox" checked={json.jsonAsJson} onChange={(e) => setJson({ ...json, jsonAsJson: e.target.checked })} />
                  {t("export.jsonAsJson")}
                </label>
              </fieldset>
            )}
            <details className="export-file">
              <summary>{t("export.fileOptions")}</summary>
              <label className="field">
                {t("export.namePattern")}
                <input name="export-name-pattern" value={names.pattern} spellCheck={false} onChange={(e) => setNames({ ...names, pattern: e.target.value })} />
              </label>
              <p className="muted small">{t("export.namePatternHint")}</p>
              <label className="field">
                {t("export.timestampFormat")}
                <input name="export-timestamp-format" value={names.timestampFormat} spellCheck={false} onChange={(e) => setNames({ ...names, timestampFormat: e.target.value })} />
              </label>
              <label className="check">
                <input type="checkbox" checked={useGzip} disabled={workbook} onChange={(e) => setGzip(e.target.checked)} />
                {t("export.gzip")}
              </label>
              {gzip && format === "parquet" && <p className="muted small">{t("export.gzipParquet")}</p>}
              {workbook && <p className="muted small">{t("export.gzipWorkbook")}</p>}
            </details>
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
                ? `${t("export.tableProgress", { index: numbers.format((phase.progress.table ?? 0) + 1), total: numbers.format(request.tables.length), table: tableName })} · `
                : ""}
              {t("export.progress", { count: phase.progress.rows, formattedCount: numbers.format(phase.progress.rows), size: bytes(phase.progress.bytes) })}
            </p>
            <div className="modal-actions">
              {/* Stopping work is secondary: danger only confirms what loses data. */}
              <button disabled={exportId === null} onClick={() => exportId?.cancel()}>
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
              <button onClick={job.reset}>{t("export.back")}</button>
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
