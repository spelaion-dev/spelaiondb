// The import dialog (ADR-038): a file or pasted text, read with the chosen options, previewed
// with the type the core detected for each column; then the table it goes to and where each
// column goes; then the statements the core wrote, which run only after they were shown. The
// core reads the source, writes and checks every statement and loads the rows: only the user's
// choices and the core's answers pass through here, and the preview's few rows are cut short.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands } from "../api";
import type {
  ColumnDetails_Serialize,
  ImportOptionsItem,
  ImportPlanItem,
  ImportPreviewItem,
  ImportProgressItem,
  ImportRequestItem,
  ImportSourceItem,
  IfExistsItem,
} from "../bindings";
import { connectTabFromExplorer, explorerDatabase, refreshExplorer } from "../connect";
import { listedConnection, tabKey, type ConnectionKey } from "../connectionKey";
import { isTextField, useFocusedPaste } from "../focusedPaste";
import { readClipboardText } from "../clipboardText";
import { chooseOpenFile } from "../dialogs";
import { errorText as message, loadedValue, type Loaded } from "../dialogState";
import { Icon } from "./Icons";
import { startJob } from "../jobs";
import { shortcutText, withShortcut } from "../shortcuts";
import { useApp, useDialogs } from "../store";
import { confirmShown, useJobPhase } from "../useDialogState";

/** What the dialog imports into. */
export interface ImportRequest {
  /** An open session of the connection (the explorer's, or a tab's): the import opens its own
   *  from it, and reads the catalog on it. */
  session: string;
  /** The connection's identity (ADR-075). */
  connection: ConnectionKey;
  /** The schema to start in; `null`: the first the connection lists. */
  schema: string | null;
  /** An existing table to import into; `null`: a new table. */
  table: string | null;
  /** Text to start from (the clipboard). */
  text: string | null;
}

export const useImport = create<{ request: ImportRequest | null; open(request: ImportRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

type Step = "source" | "target" | "review";

/** The rows read and committed, while the import runs. */
type Progress = { read: number; committed: number };
/** A finished import, and the table it loaded (to open it). */
type Done = { text: string; schema: string; table: string };

/** Reading the source: the last preview stays shown while it is read again. */
type SourceRead = { kind: "idle" } | { kind: "reading" } | { kind: "failed"; message: string };

/** Where a column of the source goes: a column of the table, a new one, or nowhere. */
interface Mapping {
  target: string;
  name: string;
  sqlType: string;
}
const NEW = "\u0000new";
const SKIP = "\u0000skip";

/** The source step's own key: pastes the clipboard's text as the source. */
const PASTE_SHORTCUT = "Ctrl+V";

/** Rows of the preview shown. */
const SHOWN_ROWS = 8;
const DELIMITERS = [",", ";", "\t", "|"];
const ENCODINGS: ImportOptionsItem["encoding"][] = ["auto", "utf8", "utf16Le", "utf16Be", "windows1252"];
const FORMATS: ImportOptionsItem["format"][] = ["csv", "tsv", "json", "markdown", "xlsx"];



export function ImportDialog() {
  const request = useImport((s) => s.request);
  if (!request) return null;
  return <ImportForm key={`${request.session}-${request.schema ?? ""}-${request.table ?? ""}`} request={request} />;
}

function ImportForm({ request }: { request: ImportRequest }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const close = useImport((s) => s.close);
  const connection = useApp((s) => listedConnection(s.connections, request.connection)) ?? null;
  const [step, setStep] = useState<Step>("source");
  const [source, setSource] = useState<ImportSourceItem | null>(request.text != null ? { kind: "text", text: request.text } : null);
  const [preview, setPreview] = useState<ImportPreviewItem | null>(null);
  const [options, setOptions] = useState<ImportOptionsItem | null>(null);
  const [sourceRead, setSourceRead] = useState<SourceRead>({ kind: "idle" });
  const reading = sourceRead.kind === "reading";
  const sourceError = sourceRead.kind === "failed" ? sourceRead.message : null;
  const setSourceError = (text: string) => setSourceRead({ kind: "failed", message: text });
  const [included, setIncluded] = useState<boolean[]>([]);
  const [schemas, setSchemas] = useState<string[]>([]);
  const [schema, setSchema] = useState<string>(request.schema ?? "");
  const [mode, setMode] = useState<"existing" | "new">(request.table ? "existing" : "new");
  const [tables, setTables] = useState<string[]>([]);
  const [table, setTable] = useState<string>(request.table ?? "");
  const [columns, setColumns] = useState<ColumnDetails_Serialize[]>([]);
  const [newTable, setNewTable] = useState("");
  const [ifExists, setIfExists] = useState<IfExistsItem>("fail");
  const [mappings, setMappings] = useState<Mapping[]>([]);
  const [commitEvery, setCommitEvery] = useState("50000");
  const [planned, setPlanned] = useState<Loaded<ImportPlanItem>>({ kind: "idle" });
  const plan = loadedValue(planned);
  const job = useJobPhase<Progress, Done, { text: string }>();
  const { phase, running } = job;
  const importId = phase.kind === "running" ? phase.id : null;
  const sequence = useRef(0);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Only the topmost dialog closes: the confirmation opened over this one takes Escape.
      if (e.key === "Escape" && !running && !useDialogs.getState().risk) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, close]);

  // The core reads the source again whenever it or the options change.
  async function read(next: ImportSourceItem, with_: ImportOptionsItem | null) {
    const mine = ++sequence.current;
    setSourceRead({ kind: "reading" });
    try {
      const shown = await call(commands.importPreview(request.session, next, with_));
      if (mine !== sequence.current) return;
      const sameColumns = preview?.columns.length === shown.columns.length;
      setPreview(shown);
      setOptions(shown.options);
      if (!sameColumns) setIncluded(shown.columns.map(() => true));
      if (!newTable) setNewTable(shown.suggestedTable);
      // A clipboard message given while it read stays.
      setSourceRead((now) => (now.kind === "reading" ? { kind: "idle" } : now));
    } catch (e) {
      if (mine !== sequence.current) return;
      setPreview(null);
      setSourceError(message(e));
    }
  }

  useEffect(() => {
    if (source) void read(source, null);
    // Only the source the dialog opened with.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The connection's schemas, and the tables of the chosen one.
  useEffect(() => {
    let current = true;
    void call(commands.listSchemas(request.session, null, false))
      .then((list) => {
        if (!current) return;
        const names = list.filter((s) => !s.system).map((s) => s.name);
        setSchemas(names);
        if (!schema && names[0]) setSchema(names[0]);
      })
      .catch((e: unknown) => current && setPlanned({ kind: "failed", message: message(e) }));
    return () => {
      current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [request.session]);

  useEffect(() => {
    if (!schema) return;
    let current = true;
    void call(commands.listObjects(request.session, schema, null, false))
      .then((objects) => {
        if (!current) return;
        setTables(objects.filter((o) => o.kind === "table" || o.kind === "partitioned_table").map((o) => o.name));
      })
      .catch(() => current && setTables([]));
    return () => {
      current = false;
    };
  }, [request.session, schema]);

  useEffect(() => {
    if (mode !== "existing" || !schema || !table) {
      setColumns([]);
      return;
    }
    let current = true;
    void call(commands.describeTable(request.session, schema, table, null))
      .then((details) => current && setColumns(details.columns))
      .catch(() => current && setColumns([]));
    return () => {
      current = false;
    };
  }, [request.session, mode, schema, table]);

  // Each column's target follows the source's columns and the table chosen.
  useEffect(() => {
    if (!preview) return;
    setMappings(
      preview.columns.map((c) => {
        const match = columns.find((col) => col.name.toLowerCase() === c.name.toLowerCase());
        const target = mode === "new" ? NEW : match ? match.name : SKIP;
        return { target, name: c.name, sqlType: c.sqlType };
      }),
    );
  }, [preview, columns, mode]);

  function setOption<K extends keyof ImportOptionsItem>(key: K, value: ImportOptionsItem[K]) {
    if (!options || !source) return;
    const next = { ...options, [key]: value };
    setOptions(next);
    void read(source, next);
  }

  async function chooseFile() {
    try {
      const path = await chooseOpenFile(t("import.chooseFile"), [
        { name: t("import.dataFiles"), extensions: ["csv", "tsv", "tab", "txt", "json", "md", "xlsx", "xlsm"] },
        { name: t("import.allFiles"), extensions: ["*"] },
      ]);
      if (!path) return;
      const next: ImportSourceItem = { kind: "file", path };
      setSource(next);
      setNewTable("");
      setPreview(null);
      void read(next, null);
    } catch (e) {
      setSourceError(message(e));
    }
  }

  function takeText(text: string) {
    if (!text.trim()) {
      setSourceError(t("import.clipboardEmpty"));
      return;
    }
    const next: ImportSourceItem = { kind: "text", text };
    setSource(next);
    setNewTable("");
    setPreview(null);
    void read(next, null);
  }

  async function paste() {
    let text: string;
    try {
      text = await readClipboardText();
    } catch {
      setSourceError(t("import.clipboardUnavailable"));
      return;
    }
    takeText(text);
  }

  // Ctrl+V outside a dialog's text fields (this one's, or one opened over it) never reaches what
  // is behind it (the explorer would paste a connection, the editor the text); on the source step
  // it takes the text from the paste event. Judged by the focused element: WebKitGTK sends the
  // event to wherever the page's selection was left, which can be the editor behind the dialog.
  useFocusedPaste((e, focused) => {
    if (isTextField(focused) && focused?.closest('[aria-modal="true"]')) return false;
    if (step === "source") takeText(e.clipboardData?.getData("text/plain") ?? "");
    return true;
  });

  function importRequest(): ImportRequestItem | null {
    if (!source || !options || !preview) return null;
    const every = Number.parseInt(commitEvery, 10);
    return {
      source,
      options,
      target:
        mode === "existing"
          ? { kind: "existing", schema, table }
          : { kind: "new", schema, table: newTable.trim(), ifExists },
      columns: preview.columns.map((_, i) => {
        const m = mappings[i];
        if (!included[i] || !m || m.target === SKIP) return { kind: "skip" };
        if (m.target === NEW) return { kind: "new", name: m.name.trim(), sqlType: m.sqlType.trim() };
        return { kind: "existing", column: m.target };
      }),
      commitEvery: Number.isFinite(every) && every >= 0 ? every : 50000,
    };
  }

  async function review() {
    const built = importRequest();
    if (!built) return;
    setPlanned({ kind: "loading" });
    setStep("review");
    try {
      setPlanned({ kind: "ready", value: await call(commands.importPlan(request.session, built)) });
    } catch (e) {
      setPlanned({ kind: "failed", message: message(e) });
    }
  }

  async function start(confirmed: boolean) {
    const built = importRequest();
    if (!built || !plan) return;
    if (!confirmed) {
      // The risks point into the statements, then the insert, then what runs after it.
      const asked = await confirmShown(connection?.name ?? "", { sql: plan.insert, statements: [...plan.statements, plan.insert, ...plan.finish], risks: plan.risks });
      if (asked === null) return;
      confirmed = asked;
    }
    const id = startJob();
    job.start(id, { read: 0, committed: 0 });
    const channel = new Channel<ImportProgressItem>();
    channel.onmessage = (p) => job.progress(id, { read: p.read ?? 0, committed: p.committed ?? 0 });
    try {
      const outcome = await call(commands.runImport(request.session, id.channel, built, confirmed, channel));
      switch (outcome.kind) {
        case "done":
        case "stoppedAtLimit": {
          const done = outcome.result;
          const seconds = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }).format((done.millis ?? 0) / 1000);
          job.done({
            text: t("import.done", { count: done.rows ?? 0, formattedCount: numbers.format(done.rows ?? 0), table: done.table, seconds }),
            schema: done.schema,
            table: done.table,
          });
          if (done.createdTable) refreshExplorer(request.connection);
          break;
        }
        case "needsConfirmation":
          job.reset();
          await start(false);
          return;
        case "cancelled":
          job.failed({
            text: t("import.cancelled", { count: outcome.kept ?? 0, formattedCount: numbers.format(outcome.kept ?? 0) }),
          });
          break;
        case "failed":
        case "notARead":
          job.failed({ text: t("import.failed", { reason: outcome.message }) });
          break;
        // The import's session is opened from the tab's, with its password.
        case "passwordRequired":
          job.failed({ text: t("import.failed", { reason: t("backup.passwordRequired") }) });
          break;
        default: {
          const unexpected: never = outcome;
          return unexpected;
        }
      }
    } catch (e) {
      job.failed({ text: t("import.failed", { reason: message(e) }) });
    }
  }

  function openTable(target: { schema: string; table: string }) {
    const tab = useApp.getState().addTableTab(tabKey(request.connection), target.schema, target.table);
    connectTabFromExplorer(tab, request.session, explorerDatabase(request.connection, target.schema));
    close();
  }

  const title = request.table ? t("import.titleTable", { name: request.table }) : t("import.title");
  const chosen = preview ? preview.columns.filter((_, i) => included[i]) : [];
  const mapped = preview
    ? preview.columns.some((_, i) => included[i] && mappings[i] && mappings[i].target !== SKIP)
    : false;
  const targetReady = !!schema && (mode === "existing" ? !!table : newTable.trim() !== "") && mapped;
  const sourceName = !source ? null : source.kind === "file" ? source.path : t("import.pasted", { count: source.text.split("\n").length, formattedCount: numbers.format(source.text.split("\n").length) });

  return (
    <div className="modal-backdrop">
      <div className="modal import-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        <ol className="import-steps" aria-label={t("import.steps")}>
          {(["source", "target", "review"] as const).map((s) => (
            <li key={s} aria-current={step === s ? "step" : undefined}>
              {t(`import.step.${s}`)}
            </li>
          ))}
        </ol>

        {step === "source" && (
          <>
            <div className="import-source">
              <button onClick={() => void chooseFile()}>{t("import.chooseFile")}</button>
              <button title={withShortcut(t("import.paste"), shortcutText(PASTE_SHORTCUT))} onClick={() => void paste()}>
                {t("import.paste")}
              </button>
              {sourceName && (
                <span className="import-source-name" title={sourceName}>
                  {sourceName}
                </span>
              )}
            </div>
            {!source && <p className="muted">{t("import.noSource")}</p>}
            {options && (
              <details className="import-options">
                <summary>{t("import.options")}</summary>
                <div className="import-options-grid">
                  <label className="field">
                    {t("import.format")}
                    <select value={options.format} onChange={(e) => setOption("format", e.target.value as ImportOptionsItem["format"])}>
                      {FORMATS.map((f) => (
                        <option key={f} value={f}>
                          {t(`import.formats.${f}`)}
                        </option>
                      ))}
                    </select>
                  </label>
                  {preview && preview.sheets.length > 0 && (
                    <label className="field">
                      {t("import.sheet")}
                      <select name="import-sheet" value={options.sheet || preview.sheets[0]} onChange={(e) => setOption("sheet", e.target.value)}>
                        {preview.sheets.map((name) => (
                          <option key={name} value={name}>
                            {name}
                          </option>
                        ))}
                      </select>
                    </label>
                  )}
                  {source?.kind === "file" && options.format !== "xlsx" && (
                    <label className="field">
                      {t("import.encoding")}
                      <select value={options.encoding} onChange={(e) => setOption("encoding", e.target.value as ImportOptionsItem["encoding"])}>
                        {ENCODINGS.map((value) => (
                          <option key={value} value={value}>
                            {value === "auto" && preview ? t("import.encodingAuto", { found: t(`import.encodings.${preview.encoding}`) }) : t(`import.encodings.${value}`)}
                          </option>
                        ))}
                      </select>
                    </label>
                  )}
                  {options.format === "csv" && (
                    <label className="field">
                      {t("import.delimiter")}
                      <select value={DELIMITERS.includes(options.delimiter) ? options.delimiter : ","} onChange={(e) => setOption("delimiter", e.target.value)}>
                        {DELIMITERS.map((d) => (
                          <option key={d} value={d}>
                            {t(`export.delimiters.${d === "\t" ? "tab" : d === "," ? "comma" : d === ";" ? "semicolon" : "pipe"}`)}
                          </option>
                        ))}
                      </select>
                    </label>
                  )}
                  {(options.format === "csv" || options.format === "tsv") && (
                    <>
                      <label className="field">
                        {t("import.quote")}
                        <input className="char-input" value={options.quote} maxLength={1} onChange={(e) => setOption("quote", e.target.value)} />
                      </label>
                      <label className="field">
                        {t("import.escape")}
                        <input className="char-input" value={options.escape} maxLength={1} placeholder={t("import.escapeDoubled")} onChange={(e) => setOption("escape", e.target.value)} />
                      </label>
                    </>
                  )}
                  <label className="field">
                    {t("import.nullText")}
                    <input value={options.nullText} placeholder={t("import.nullNone")} onChange={(e) => setOption("nullText", e.target.value)} />
                  </label>
                  <label className="field">
                    {t("import.dateFormat")}
                    <input value={options.dateFormat} placeholder="%Y-%m-%d" onChange={(e) => setOption("dateFormat", e.target.value)} />
                  </label>
                  <label className="field">
                    {t("import.timestampFormat")}
                    <input value={options.timestampFormat} placeholder="%Y-%m-%d %H:%M:%S" onChange={(e) => setOption("timestampFormat", e.target.value)} />
                  </label>
                  <label className="field">
                    {t("import.timeZone")}
                    <input value={options.timeZone} placeholder={t("import.timeZoneNone")} onChange={(e) => setOption("timeZone", e.target.value)} />
                  </label>
                </div>
                <div className="import-checks">
                  {options.format !== "markdown" && (
                    <label className="check">
                      <input type="checkbox" checked={options.header} onChange={(e) => setOption("header", e.target.checked)} />
                      {t("import.header")}
                    </label>
                  )}
                  <label className="check">
                    <input type="checkbox" checked={options.emptyAsNull} onChange={(e) => setOption("emptyAsNull", e.target.checked)} />
                    {t("import.emptyAsNull")}
                  </label>
                  <label className="check">
                    <input type="checkbox" checked={options.trim} onChange={(e) => setOption("trim", e.target.checked)} />
                    {t("import.trim")}
                  </label>
                </div>
              </details>
            )}
            {reading && <p className="muted" role="status">{t("import.reading")}</p>}
            {sourceError && (
              <p className="error" role="alert">
                {t("import.couldNotRead", { reason: sourceError })}
              </p>
            )}
            {preview && (
              <>
                <p className="muted small">
                  {preview.more
                    ? t("import.sampledMore", { count: preview.sampled, formattedCount: numbers.format(preview.sampled) })
                    : t("import.sampled", { count: preview.sampled, formattedCount: numbers.format(preview.sampled) })}
                </p>
                <div className="import-preview" role="region" aria-label={t("import.preview")}>
                  <table>
                    <thead>
                      <tr>
                        {preview.columns.map((c, i) => (
                          <th key={i} className={included[i] ? undefined : "removed"}>
                            <div className="import-column-head">
                              <span className="import-column-name">{c.name}</span>
                              <button
                                className="ghost icon-button"
                                aria-label={included[i] ? t("import.removeColumn", { name: c.name }) : t("import.restoreColumn", { name: c.name })}
                                title={included[i] ? t("import.removeColumn", { name: c.name }) : t("import.restoreColumn", { name: c.name })}
                                onClick={() => setIncluded((now) => now.map((v, j) => (j === i ? !v : v)))}
                              >
                                <Icon name={included[i] ? "x" : "plus"} />
                              </button>
                            </div>
                            <span className="import-kind">{t(`import.kinds.${c.kind}`)}</span>
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {preview.rows.slice(0, SHOWN_ROWS).map((row, r) => (
                        <tr key={r}>
                          {row.map((value, i) => (
                            <td key={i} className={included[i] ? undefined : "removed"}>
                              {value === null ? <span className="null">NULL</span> : value}
                            </td>
                          ))}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              </>
            )}
            <div className="modal-actions">
              <button onClick={close}>{t("import.close")}</button>
              <button className="primary" disabled={!preview || reading || chosen.length === 0} onClick={() => setStep("target")}>
                {t("import.next")}
              </button>
            </div>
          </>
        )}

        {step === "target" && preview && (
          <>
            <fieldset className="import-target">
              <legend>{t("import.target")}</legend>
              <label className="field">
                {t("import.schema")}
                <select value={schema} onChange={(e) => setSchema(e.target.value)}>
                  {schemas.map((s) => (
                    <option key={s} value={s}>
                      {s}
                    </option>
                  ))}
                </select>
              </label>
              <label className="check">
                <input type="radio" name="import-mode" checked={mode === "existing"} onChange={() => setMode("existing")} />
                {t("import.existingTable")}
              </label>
              {mode === "existing" && (
                <label className="field">
                  {t("import.table")}
                  <select value={table} onChange={(e) => setTable(e.target.value)}>
                    <option value="">{t("import.chooseTable")}</option>
                    {tables.map((name) => (
                      <option key={name} value={name}>
                        {name}
                      </option>
                    ))}
                  </select>
                </label>
              )}
              <label className="check">
                <input type="radio" name="import-mode" checked={mode === "new"} onChange={() => setMode("new")} />
                {t("import.newTable")}
              </label>
              {mode === "new" && (
                <>
                  <label className="field">
                    {t("import.tableName")}
                    <input type="text" name="import-table" value={newTable} onChange={(e) => setNewTable(e.target.value)} />
                  </label>
                  <label className="field">
                    {t("import.ifExists")}
                    <select value={ifExists} onChange={(e) => setIfExists(e.target.value as IfExistsItem)}>
                      <option value="fail">{t("import.ifExistsFail")}</option>
                      <option value="append">{t("import.ifExistsAppend")}</option>
                      <option value="replace">{t("import.ifExistsReplace")}</option>
                    </select>
                  </label>
                </>
              )}
            </fieldset>
            <div className="import-mapping" role="region" aria-label={t("import.mapping")}>
              <table>
                <thead>
                  <tr>
                    <th>{t("import.sourceColumn")}</th>
                    <th>{t("import.targetColumn")}</th>
                    <th>{t("import.type")}</th>
                  </tr>
                </thead>
                <tbody>
                  {preview.columns.map((c, i) => {
                    if (!included[i]) return null;
                    const m = mappings[i];
                    if (!m) return null;
                    const update = (patch: Partial<Mapping>) => setMappings((now) => now.map((v, j) => (j === i ? { ...v, ...patch } : v)));
                    return (
                      <tr key={i}>
                        <td className="import-column-name">{c.name}</td>
                        <td>
                          {mode === "new" ? (
                            <input type="text" aria-label={t("import.columnName", { name: c.name })} value={m.name} onChange={(e) => update({ name: e.target.value })} />
                          ) : (
                            <div className="import-target-cell">
                              <select aria-label={t("import.targetFor", { name: c.name })} value={m.target} onChange={(e) => update({ target: e.target.value })}>
                                <option value={SKIP}>{t("import.skip")}</option>
                                {columns.map((col) => (
                                  <option key={col.name} value={col.name}>
                                    {col.name}
                                  </option>
                                ))}
                                <option value={NEW}>{t("import.newColumn")}</option>
                              </select>
                              {m.target === NEW && (
                                <input type="text" aria-label={t("import.columnName", { name: c.name })} value={m.name} onChange={(e) => update({ name: e.target.value })} />
                              )}
                            </div>
                          )}
                        </td>
                        <td>
                          {m.target === NEW ? (
                            <input type="text" className="code" aria-label={t("import.typeFor", { name: c.name })} value={m.sqlType} onChange={(e) => update({ sqlType: e.target.value })} />
                          ) : m.target === SKIP ? (
                            <span className="muted">{t("import.skipped")}</span>
                          ) : (
                            <span className="muted code">{columns.find((col) => col.name === m.target)?.nativeType ?? ""}</span>
                          )}
                        </td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            </div>
            {preview?.keptWhole ? (
              // The core loads the rows aside and puts them in place in one step (ADR-066).
              <p className="muted small">{t("import.keptWhole")}</p>
            ) : (
              <>
                <label className="field">
                  {t("import.commitEvery")}
                  <input type="text" inputMode="numeric" value={commitEvery} onChange={(e) => setCommitEvery(e.target.value.replace(/[^0-9]/g, ""))} />
                </label>
                <p className="muted small">{t("import.commitHint")}</p>
              </>
            )}
            <div className="modal-actions">
              <button onClick={() => setStep("source")}>{t("import.back")}</button>
              <button className="primary" disabled={!targetReady} onClick={() => void review()}>
                {t("import.reviewSql")}
              </button>
            </div>
          </>
        )}

        {step === "review" && (
          <>
            {(planned.kind === "idle" || planned.kind === "loading") && <p className="muted" role="status">{t("import.planning")}</p>}
            {planned.kind === "failed" && (
              <p className="error" role="alert">
                {t("import.couldNotPlan", { reason: planned.message })}
              </p>
            )}
            {plan && (
              <>
                <p className="muted">
                  {plan.statements.length > 0 ? t("import.reviewIntro", { count: plan.statements.length }) : t("import.reviewIntroRows")}
                </p>
                <pre className="import-sql">{[...plan.statements, plan.insert].join(";\n\n")}</pre>
                {plan.finish.length > 0 && (
                  <>
                    <p className="muted">{t("import.reviewFinish")}</p>
                    <pre className="import-sql">{plan.finish.join(";\n\n")}</pre>
                  </>
                )}
                <p className="muted small">{t("import.bound")}</p>
                {plan.leftovers.length > 0 && (
                  <p className="banner" role="note">
                    {t("import.leftovers", { names: plan.leftovers.join(", ") })}
                  </p>
                )}
              </>
            )}
            {phase.kind === "running" && (
              <p role="status" className="import-progress">
                {t("import.progress", {
                  count: phase.progress.read,
                  formattedCount: numbers.format(phase.progress.read),
                  committed: numbers.format(phase.progress.committed),
                })}
              </p>
            )}
            {phase.kind === "done" && (
              <p role="status" className="import-result">
                {phase.text}
              </p>
            )}
            {phase.kind === "failed" && (
              <p role="alert" className="error import-result">
                {phase.text}
              </p>
            )}
            <div className="modal-actions">
              {phase.kind === "running" ? (
                <button disabled={importId === null} onClick={() => importId?.cancel()}>
                  {t("import.cancel")}
                </button>
              ) : phase.kind === "done" ? (
                <>
                  <button onClick={close}>{t("import.close")}</button>
                  <button className="primary" onClick={() => openTable(phase)}>
                    {t("import.openTable")}
                  </button>
                </>
              ) : (
                <>
                  <button
                    onClick={() => {
                      job.reset();
                      setStep("target");
                    }}
                  >
                    {t("import.back")}
                  </button>
                  <button className="primary" disabled={!plan} onClick={() => void start(false)}>
                    {phase.kind === "failed" ? t("import.again") : t("import.start")}
                  </button>
                </>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
