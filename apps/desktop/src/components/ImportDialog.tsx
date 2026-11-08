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
  ImportOutcome,
  ImportPlanItem,
  ImportPreviewItem,
  ImportProgressItem,
  ImportRequestItem,
  ImportSourceItem,
  IfExistsItem,
  RiskKind,
} from "../bindings";
import { connectTabFromExplorer, EXPLORER_REFRESH_EVENT, explorerDatabase } from "../connect";
import { chooseOpenFile } from "../dialogs";
import { Icon } from "./Icons";
import { useApp, useDialogs } from "../store";

/** What the dialog imports into. */
export interface ImportRequest {
  /** An open session of the connection (the explorer's, or a tab's): the import opens its own
   *  from it, and reads the catalog on it. */
  session: string;
  connectionId: string;
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

type Phase =
  | { kind: "idle" }
  | { kind: "running"; read: number; committed: number }
  | { kind: "done"; text: string; schema: string; table: string }
  | { kind: "failed"; text: string };

/** Where a column of the source goes: a column of the table, a new one, or nowhere. */
interface Mapping {
  target: string;
  name: string;
  sqlType: string;
}
const NEW = "\u0000new";
const SKIP = "\u0000skip";

/** Rows of the preview shown. */
const SHOWN_ROWS = 8;
const DELIMITERS = [",", ";", "\t", "|"];
const ENCODINGS: ImportOptionsItem["encoding"][] = ["auto", "utf8", "utf16Le", "utf16Be", "windows1252"];
const FORMATS: ImportOptionsItem["format"][] = ["csv", "tsv", "json", "markdown"];

let nextImport = Math.floor(Math.random() * 1_000_000);

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function ImportDialog() {
  const request = useImport((s) => s.request);
  if (!request) return null;
  return <ImportForm key={`${request.session}-${request.schema ?? ""}-${request.table ?? ""}`} request={request} />;
}

function ImportForm({ request }: { request: ImportRequest }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const close = useImport((s) => s.close);
  const connection = useApp((s) => s.connections.find((c) => c.id === request.connectionId)) ?? null;
  const [step, setStep] = useState<Step>("source");
  const [source, setSource] = useState<ImportSourceItem | null>(request.text != null ? { kind: "text", text: request.text } : null);
  const [preview, setPreview] = useState<ImportPreviewItem | null>(null);
  const [options, setOptions] = useState<ImportOptionsItem | null>(null);
  const [sourceError, setSourceError] = useState<string | null>(null);
  const [reading, setReading] = useState(false);
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
  const [plan, setPlan] = useState<ImportPlanItem | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [importId, setImportId] = useState<number | null>(null);
  const running = phase.kind === "running";
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
    setReading(true);
    setSourceError(null);
    try {
      const shown = await call(commands.importPreview(request.session, next, with_));
      if (mine !== sequence.current) return;
      const sameColumns = preview?.columns.length === shown.columns.length;
      setPreview(shown);
      setOptions(shown.options);
      if (!sameColumns) setIncluded(shown.columns.map(() => true));
      if (!newTable) setNewTable(shown.suggestedTable);
    } catch (e) {
      if (mine !== sequence.current) return;
      setPreview(null);
      setSourceError(message(e));
    } finally {
      if (mine === sequence.current) setReading(false);
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
      .catch((e: unknown) => current && setPlanError(message(e)));
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
        { name: t("import.dataFiles"), extensions: ["csv", "tsv", "tab", "txt", "json", "md"] },
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

  async function paste() {
    try {
      const text = await navigator.clipboard.readText();
      if (!text.trim()) {
        setSourceError(t("import.clipboardEmpty"));
        return;
      }
      const next: ImportSourceItem = { kind: "text", text };
      setSource(next);
      setNewTable("");
      setPreview(null);
      void read(next, null);
    } catch {
      setSourceError(t("import.clipboardUnavailable"));
    }
  }

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
    setPlan(null);
    setPlanError(null);
    setStep("review");
    try {
      setPlan(await call(commands.importPlan(request.session, built)));
    } catch (e) {
      setPlanError(message(e));
    }
  }

  async function start(confirmed: boolean) {
    const built = importRequest();
    if (!built || !plan) return;
    if (!confirmed && plan.risks.length > 0) {
      const all = [...plan.statements, plan.insert];
      const statements: { sql: string; risk: RiskKind }[] = plan.risks.map((r) => ({ sql: all[r.statement] ?? plan.insert, risk: r.risk }));
      if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return;
      confirmed = true;
    }
    const id = ++nextImport;
    setImportId(id);
    setPhase({ kind: "running", read: 0, committed: 0 });
    const channel = new Channel<ImportProgressItem>();
    channel.onmessage = (p) => setPhase((now) => (now.kind === "running" ? { kind: "running", read: p.read ?? 0, committed: p.committed ?? 0 } : now));
    try {
      const outcome: ImportOutcome = await call(commands.runImport(request.session, id, built, confirmed, channel));
      switch (outcome.kind) {
        case "done": {
          const seconds = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }).format((outcome.millis ?? 0) / 1000);
          setPhase({
            kind: "done",
            text: t("import.done", { count: outcome.rows ?? 0, formattedCount: numbers.format(outcome.rows ?? 0), table: outcome.table, seconds }),
            schema: outcome.schema,
            table: outcome.table,
          });
          if (outcome.createdTable) window.dispatchEvent(new CustomEvent(EXPLORER_REFRESH_EVENT, { detail: request.connectionId }));
          break;
        }
        case "needsConfirmation":
          setPhase({ kind: "idle" });
          await start(false);
          return;
        case "cancelled":
          setPhase({
            kind: "failed",
            text: t("import.cancelled", { count: outcome.committed ?? 0, formattedCount: numbers.format(outcome.committed ?? 0) }),
          });
          break;
        case "failed":
          setPhase({ kind: "failed", text: t("import.failed", { reason: outcome.message }) });
          break;
      }
    } catch (e) {
      setPhase({ kind: "failed", text: t("import.failed", { reason: message(e) }) });
    } finally {
      setImportId(null);
    }
  }

  function openTable(target: { schema: string; table: string }) {
    const tab = useApp.getState().addTableTab(request.connectionId, target.schema, target.table);
    connectTabFromExplorer(tab, request.session, explorerDatabase(request.connectionId, target.schema));
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
              <button onClick={() => void paste()}>{t("import.paste")}</button>
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
                  {source?.kind === "file" && (
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
            <label className="field">
              {t("import.commitEvery")}
              <input type="text" inputMode="numeric" value={commitEvery} onChange={(e) => setCommitEvery(e.target.value.replace(/[^0-9]/g, ""))} />
            </label>
            <p className="muted small">{t("import.commitHint")}</p>
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
            {!plan && !planError && <p className="muted" role="status">{t("import.planning")}</p>}
            {planError && (
              <p className="error" role="alert">
                {t("import.couldNotPlan", { reason: planError })}
              </p>
            )}
            {plan && (
              <>
                <p className="muted">
                  {plan.statements.length > 0 ? t("import.reviewIntro", { count: plan.statements.length }) : t("import.reviewIntroRows")}
                </p>
                <pre className="import-sql">{[...plan.statements, plan.insert].join(";\n\n")}</pre>
                <p className="muted small">{t("import.bound")}</p>
              </>
            )}
            {phase.kind === "running" && (
              <p role="status" className="import-progress">
                {t("import.progress", {
                  count: phase.read,
                  formattedCount: numbers.format(phase.read),
                  committed: numbers.format(phase.committed),
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
                <button disabled={importId === null} onClick={() => importId !== null && void call(commands.cancelImport(importId)).catch(() => undefined)}>
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
                      setPhase({ kind: "idle" });
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
