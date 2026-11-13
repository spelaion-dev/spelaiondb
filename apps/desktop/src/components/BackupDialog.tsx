// Backup and restore through the tunnel (ADR-049): the vendor's own tool (pg_dump, pg_restore,
// psql, mysqldump, mysql), found and started by the core against the connection's tunnel. This
// dialog only gathers the choices, shows the tool the core found, the file it will write and the
// tool's live log, and asks before a restore; the core writes every argument, keeps the password
// out of them and of the log, and refuses a read-only connection.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands } from "../api";
import type {
  BackupEventItem,
  BackupOptionsItem,
  BackupPreferencesItem,
  BackupPreviewItem,
  BackupRequestItem,
  ColumnStatisticsItem,
  DuckdbFormatItem,
  EmbeddedBackupItem,
  DumpContentItem,
  MysqlConsistencyItem,
  PgFormatItem,
  RestoreFormatItem,
  RestorePreviewItem,
  RestoreRequestItem,
  ToolItem,
} from "../bindings";
import { chooseFolder, chooseOpenFile, openFilters } from "../dialogs";
import { formatBytes } from "./ServerObservation";

export type BackupFamily = "postgres" | "mysql" | "sqlite" | "duckdb";

/** How an engine is backed up: its client tools, or its own commands (SQLite, DuckDB). */
export function backupFamily(engine: string): BackupFamily | null {
  if (engine === "postgres") return "postgres";
  if (engine === "mysql" || engine === "mariadb") return "mysql";
  if (engine === "sqlite") return "sqlite";
  if (engine === "duckdb") return "duckdb";
  return null;
}

/** Whether the family restores from this dialog: a SQLite copy is a database file of its own. */
export function familyRestores(family: BackupFamily): boolean {
  return family !== "sqlite";
}

export interface BackupRequest {
  /** An open session of the connection (the explorer's): the run uses its tunnel and password. */
  session: string;
  connectionId: string;
  connectionName: string;
  family: BackupFamily;
  mode: "backup" | "restore";
}

export const useBackup = create<{ request: BackupRequest | null; open(request: BackupRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

type Phase = { kind: "idle" } | { kind: "running"; bytes: number; total: number | null } | { kind: "done"; text: string } | { kind: "failed"; text: string };

/** The log lines kept: the newest. */
const LOG_LINES = 2000;
const PG_FORMATS: PgFormatItem[] = ["custom", "plain", "directory", "tar"];
const CONTENTS: DumpContentItem[] = ["schemaAndData", "schemaOnly", "dataOnly"];
const CONSISTENCY: MysqlConsistencyItem[] = ["singleTransaction", "lockAllTables", "noLocks"];
const STATISTICS: ColumnStatisticsItem[] = ["auto", "off", "on"];
const RESTORE_FORMATS: RestoreFormatItem[] = ["auto", "plain", "custom", "directory", "tar"];

let nextRun = Math.floor(Math.random() * 1_000_000);

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function BackupDialog() {
  const request = useBackup((s) => s.request);
  if (!request) return null;
  return <BackupForm key={`${request.session}-${request.mode}`} request={request} />;
}

interface PgState {
  format: PgFormatItem;
  compression: string;
  encoding: string;
  content: DumpContentItem;
  inserts: boolean;
  noPrivileges: boolean;
  noOwner: boolean;
  clean: boolean;
  create: boolean;
}

interface MysqlState {
  consistency: MysqlConsistencyItem;
  addDrop: boolean;
  noCreate: boolean;
  noData: boolean;
  extendedInsert: boolean;
  routines: boolean;
  events: boolean;
  triggers: boolean;
  includeViews: boolean;
  stripDefiner: boolean;
  hexBlob: boolean;
  columnStatistics: ColumnStatisticsItem;
}

function BackupForm({ request }: { request: BackupRequest }) {
  const { t, i18n } = useTranslation("editor");
  const close = useBackup((s) => s.close);
  const restoring = request.mode === "restore";
  /** SQLite and DuckDB: their own commands, no client tools. */
  const embedded = request.family === "sqlite" || request.family === "duckdb";
  const [duckdbFormat, setDuckdbFormat] = useState<DuckdbFormatItem>("parquet");
  const [prefs, setPrefs] = useState<BackupPreferencesItem | null>(null);
  const [toolsFolder, setToolsFolder] = useState("");
  const [tools, setTools] = useState<ToolItem[] | null>(null);
  /** The database the connection opens, as the core resolved it. */
  const [database, setDatabase] = useState("");
  const databaseSeen = useRef(false);
  const [folder, setFolder] = useState("");
  const [pattern, setPattern] = useState("");
  const [extraArgs, setExtraArgs] = useState("");
  const [schemas, setSchemas] = useState<string[]>([]);
  const [chosenSchemas, setChosenSchemas] = useState<string[]>([]);
  const [tables, setTables] = useState<string[]>([]);
  const [chosenTables, setChosenTables] = useState<string[]>([]);
  const [pg, setPg] = useState<PgState>({
    format: "custom",
    compression: "",
    encoding: "",
    content: "schemaAndData",
    inserts: false,
    noPrivileges: false,
    noOwner: false,
    clean: false,
    create: false,
  });
  const [my, setMy] = useState<MysqlState>({
    consistency: "singleTransaction",
    addDrop: true,
    noCreate: false,
    noData: false,
    extendedInsert: true,
    routines: true,
    events: false,
    triggers: true,
    includeViews: true,
    stripDefiner: false,
    hexBlob: false,
    columnStatistics: "auto",
  });
  const [input, setInput] = useState("");
  const [restoreFormat, setRestoreFormat] = useState<RestoreFormatItem>("auto");
  const [pgRestore, setPgRestore] = useState({ clean: false, noOwner: false, create: false });
  const [myRestore, setMyRestore] = useState({ database: "", disableForeignKeys: false });
  const [preview, setPreview] = useState<BackupPreviewItem | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<RestorePreviewItem | null>(null);
  const [confirmError, setConfirmError] = useState<string | null>(null);
  const [log, setLog] = useState<string[]>([]);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [runId, setRunId] = useState<number | null>(null);
  const running = phase.kind === "running";
  const logRef = useRef<HTMLPreElement>(null);
  const confirmRef = useRef<HTMLButtonElement>(null);
  const bytes = (n: number) => formatBytes(n, i18n.language);
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }), [i18n.language]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || running) return;
      if (confirm || confirmError) {
        setConfirm(null);
        setConfirmError(null);
      } else close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, confirm, confirmError, close]);

  // The remembered tool folder, output folder and pattern.
  useEffect(() => {
    void call(commands.backupPreferences())
      .then((p) => {
        setPrefs(p);
        setToolsFolder(request.family === "postgres" ? p.postgresTools : request.family === "mysql" ? p.mysqlTools : "");
        setFolder(p.folder);
        setPattern(p.namePattern);
      })
      .catch(() => setPrefs(null));
  }, [request.family]);

  // The tools the core finds in the chosen folder, or by itself.
  useEffect(() => {
    if (embedded) {
      setTools([]);
      return;
    }
    let current = true;
    setTools(null);
    const timer = window.setTimeout(() => {
      void call(commands.backupTools(request.session, toolsFolder))
        .then((found) => {
          if (!current) return;
          setTools(found.tools);
          setDatabase(found.database);
          // MySQL starts with the connection's own database chosen.
          if (!databaseSeen.current && found.database) {
            databaseSeen.current = true;
            if (request.family === "mysql") setChosenSchemas((now) => (now.length === 0 ? [found.database] : now));
            setMyRestore((now) => (now.database ? now : { ...now, database: found.database }));
          }
        })
        .catch((e: unknown) => current && setTools([{ name: "", path: "", version: "", error: message(e) }]));
    }, 250);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [request.session, toolsFolder, embedded]);

  // Schemas (PostgreSQL) or databases (MySQL) to choose from.
  useEffect(() => {
    if (embedded || (restoring && request.family === "postgres")) return;
    let current = true;
    void call(commands.listSchemas(request.session, null, false))
      .then((list) => current && setSchemas(list.filter((s) => !s.system).map((s) => s.name)))
      .catch(() => current && setSchemas([]));
    return () => {
      current = false;
    };
  }, [request.session, request.family, restoring, embedded]);

  // Tables, when exactly one schema or database is chosen.
  const tableSchema = chosenSchemas.length === 1 ? chosenSchemas[0] : null;
  useEffect(() => {
    setChosenTables([]);
    if (!tableSchema || restoring) {
      setTables([]);
      return;
    }
    let current = true;
    void call(commands.listObjects(request.session, tableSchema, null, false))
      .then((objects) => current && setTables(objects.filter((o) => o.kind === "table" || o.kind === "partitioned_table").map((o) => o.name)))
      .catch(() => current && setTables([]));
    return () => {
      current = false;
    };
  }, [request.session, tableSchema, restoring]);

  useEffect(() => {
    logRef.current?.scrollTo({ top: logRef.current.scrollHeight });
  }, [log]);

  useEffect(() => {
    if (confirm) confirmRef.current?.focus();
  }, [confirm]);

  const backupOptions = (): BackupOptionsItem =>
    request.family === "postgres"
      ? {
          kind: "postgres",
          schemas: chosenSchemas,
          tables: tableSchema ? chosenTables.map((name) => ({ schema: tableSchema, name })) : [],
          format: pg.format,
          compression: pg.compression === "" ? null : Number(pg.compression),
          encoding: pg.encoding,
          content: pg.content,
          inserts: pg.inserts,
          noPrivileges: pg.noPrivileges,
          noOwner: pg.noOwner,
          clean: pg.clean,
          create: pg.create,
        }
      : { kind: "mysql", databases: chosenSchemas, tables: tableSchema ? chosenTables : [], ...my };

  const backupRequest = (): BackupRequestItem => ({
    toolsFolder,
    folder,
    namePattern: pattern,
    extraArgs,
    options: backupOptions(),
  });

  const restoreRequest = (): RestoreRequestItem => ({
    toolsFolder,
    input,
    format: request.family === "postgres" ? restoreFormat : "plain",
    extraArgs,
    options: request.family === "postgres" ? { kind: "postgres", ...pgRestore } : { kind: "mysql", ...myRestore },
  });

  const embeddedRequest = (): EmbeddedBackupItem => ({ folder, namePattern: pattern, duckdbFormat });

  // The file the backup will write, and the core's notes, as the choices change.
  const previewKey = JSON.stringify(restoring ? null : embedded ? embeddedRequest() : backupRequest());
  useEffect(() => {
    if (restoring || !folder.trim()) {
      setPreview(null);
      setPreviewError(null);
      return;
    }
    let current = true;
    const timer = window.setTimeout(() => {
      const asked = embedded
        ? call(commands.previewEmbeddedBackup(request.session, JSON.parse(previewKey) as EmbeddedBackupItem)).then((path): BackupPreviewItem => ({
            tool: "",
            version: "",
            path,
            notes: [],
          }))
        : call(commands.previewBackup(request.session, JSON.parse(previewKey) as BackupRequestItem));
      void asked
        .then((p) => {
          if (!current) return;
          setPreview(p);
          setPreviewError(null);
        })
        .catch((e: unknown) => {
          if (!current) return;
          setPreview(null);
          setPreviewError(message(e));
        });
    }, 300);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [previewKey, folder, restoring, embedded, request.session]);

  function remember() {
    if (!prefs) return;
    const next: BackupPreferencesItem = {
      ...prefs,
      postgresTools: request.family === "postgres" ? toolsFolder : prefs.postgresTools,
      mysqlTools: request.family === "mysql" ? toolsFolder : prefs.mysqlTools,
      folder: restoring ? prefs.folder : folder,
      namePattern: restoring ? prefs.namePattern : pattern,
    };
    setPrefs(next);
    void call(commands.setBackupPreferences(next)).catch(() => undefined);
  }

  function onEvent(event: BackupEventItem) {
    switch (event.kind) {
      case "started":
        setLog((now) => [...now, `$ ${event.commandLine}`].slice(-LOG_LINES));
        break;
      case "log":
        setLog((now) => [...now, event.line].slice(-LOG_LINES));
        break;
      case "progress":
        setPhase((now) => (now.kind === "running" ? { kind: "running", bytes: event.bytes ?? 0, total: event.total ?? null } : now));
        break;
      case "statements":
        break;
    }
  }

  async function startBackup() {
    remember();
    const id = ++nextRun;
    setRunId(id);
    setLog([]);
    setPhase({ kind: "running", bytes: 0, total: null });
    const channel = new Channel<BackupEventItem>();
    channel.onmessage = onEvent;
    try {
      const outcome = await call(
        embedded ? commands.runEmbeddedBackup(request.session, id, embeddedRequest(), channel) : commands.runBackup(request.session, id, backupRequest(), channel),
      );
      switch (outcome.kind) {
        case "done":
          setPhase({
            kind: "done",
            text: t("backup.done", { size: bytes(outcome.bytes ?? 0), seconds: numbers.format((outcome.millis ?? 0) / 1000), path: outcome.path }),
          });
          break;
        case "cancelled":
          setPhase({ kind: "failed", text: t("backup.cancelled") });
          break;
        case "passwordRequired":
          setPhase({ kind: "failed", text: t("backup.failed", { reason: t("backup.passwordRequired") }) });
          break;
        case "failed":
          setPhase({ kind: "failed", text: t("backup.failed", { reason: outcome.message }) });
          break;
      }
    } catch (e) {
      setPhase({ kind: "failed", text: t("backup.failed", { reason: message(e) }) });
    } finally {
      setRunId(null);
    }
  }

  async function askRestore() {
    setConfirmError(null);
    if (request.family === "duckdb") {
      // DuckDB's own IMPORT DATABASE: no tool to find; the core still refuses a read-only connection.
      setConfirm({ tool: "DuckDB", version: "DuckDB IMPORT DATABASE", format: "directory", notes: [], readOnly: false });
      return;
    }
    try {
      setConfirm(await call(commands.previewRestore(request.session, restoreRequest())));
    } catch (e) {
      setConfirmError(message(e));
    }
  }

  async function startRestore() {
    setConfirm(null);
    remember();
    const id = ++nextRun;
    setRunId(id);
    setLog([]);
    setPhase({ kind: "running", bytes: 0, total: null });
    const channel = new Channel<BackupEventItem>();
    channel.onmessage = onEvent;
    try {
      const outcome = await call(
        request.family === "duckdb"
          ? commands.runDuckdbImport(request.session, id, input, true, channel)
          : commands.runRestore(request.session, id, restoreRequest(), true, channel),
      );
      switch (outcome.kind) {
        case "done":
          setPhase({ kind: "done", text: t("restoreDb.done", { seconds: numbers.format((outcome.millis ?? 0) / 1000) }) });
          break;
        case "cancelled":
          setPhase({ kind: "failed", text: t("restoreDb.cancelled") });
          break;
        case "needsConfirmation":
          setPhase({ kind: "failed", text: t("restoreDb.failed", { reason: t("restoreDb.notConfirmed") }) });
          break;
        case "passwordRequired":
          setPhase({ kind: "failed", text: t("restoreDb.failed", { reason: t("backup.passwordRequired") }) });
          break;
        case "failed":
          setPhase({ kind: "failed", text: t("restoreDb.failed", { reason: outcome.message }) });
          break;
      }
    } catch (e) {
      setPhase({ kind: "failed", text: t("restoreDb.failed", { reason: message(e) }) });
    } finally {
      setRunId(null);
    }
  }

  async function pickToolsFolder() {
    const chosen = await chooseFolder(t("backup.chooseToolsFolder"), toolsFolder || null);
    if (chosen) setToolsFolder(chosen);
  }

  async function pickFolder() {
    const chosen = await chooseFolder(t("backup.chooseFolder"), folder || null);
    if (chosen) setFolder(chosen);
  }

  async function pickInput(directory: boolean) {
    const chosen = directory
      ? await chooseFolder(t("restoreDb.chooseArchiveFolder"), input || null)
      : await chooseOpenFile(
          t("restoreDb.chooseFile"),
          openFilters([
            { name: t("restoreDb.backupFiles"), extensions: request.family === "postgres" ? ["dump", "backup", "sql", "tar", "gz"] : ["sql"] },
            { name: t("restoreDb.allFiles"), extensions: ["*"] },
          ]) ?? [],
          input || null,
        );
    if (chosen) setInput(chosen);
  }

  const toggle = (list: string[], name: string, on: boolean) => (on ? [...list, name] : list.filter((n) => n !== name));
  const title = restoring ? t("restoreDb.title", { name: request.connectionName }) : t("backup.title", { name: request.connectionName });
  const missingTool = tools?.some(
    (tool) => tool.error && (restoring ? tool.name !== "pg_dump" && tool.name !== "mysqldump" : tool.name === "pg_dump" || tool.name === "mysqldump"),
  );
  const canBackup = !running && !!folder.trim() && !!preview && !previewError;
  const canRestore = !running && !!input.trim();

  return (
    <div className="modal-backdrop">
      <div className="modal backup-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        <p className="muted small">
          {restoring
            ? request.family === "duckdb"
              ? t("restoreDb.introDuckdb")
              : t("restoreDb.intro")
            : request.family === "sqlite"
              ? t("backup.introSqlite")
              : request.family === "duckdb"
                ? t("backup.introDuckdb")
                : t("backup.intro")}
        </p>

        {!embedded && (
          <fieldset disabled={running}>
            <legend>{t("backup.tools")}</legend>
            <div className="backup-row">
              <input type="text" aria-label={t("backup.toolsFolder")} placeholder={t("backup.toolsAuto")} value={toolsFolder} onChange={(e) => setToolsFolder(e.target.value)} />
              <button type="button" onClick={() => void pickToolsFolder()}>
                {t("backup.chooseFolderButton")}
              </button>
              {toolsFolder && (
                <button type="button" className="ghost" onClick={() => setToolsFolder("")}>
                  {t("backup.findAutomatically")}
                </button>
              )}
            </div>
            {tools === null ? (
              <p className="muted small" role="status">
                {t("backup.lookingForTools")}
              </p>
            ) : (
              <ul className="backup-tools" aria-label={t("backup.tools")}>
                {tools.map((tool, i) => (
                  <li key={i} className={tool.error ? "missing" : undefined}>
                    <span className="backup-tool-name">{tool.name}</span>
                    {tool.error ? (
                      <span className="error">{tool.error}</span>
                    ) : (
                      <span className="muted code" title={tool.path}>
                        {tool.version}
                      </span>
                    )}
                  </li>
                ))}
              </ul>
            )}
          </fieldset>
        )}

        {!restoring && (
          <>
            {request.family === "duckdb" && (
              <fieldset disabled={running}>
                <legend>{t("backup.options")}</legend>
                <label className="field">
                  {t("backup.duckdbFormat")}
                  <select value={duckdbFormat} onChange={(e) => setDuckdbFormat(e.target.value as DuckdbFormatItem)}>
                    <option value="parquet">{t("backup.duckdbFormats.parquet")}</option>
                    <option value="csv">{t("backup.duckdbFormats.csv")}</option>
                  </select>
                </label>
              </fieldset>
            )}
            {!embedded && (
              <fieldset disabled={running}>
                <legend>{request.family === "postgres" ? t("backup.schemas") : t("backup.databases")}</legend>
                <p className="muted small">{request.family === "postgres" ? t("backup.schemasHint") : t("backup.databasesHint")}</p>
                <div className="backup-choices">
                  {schemas.map((name) => (
                    <label key={name} className="check">
                      <input type="checkbox" checked={chosenSchemas.includes(name)} onChange={(e) => setChosenSchemas((now) => toggle(now, name, e.target.checked))} />
                      {name}
                    </label>
                  ))}
                </div>
                {tableSchema && tables.length > 0 && (
                  <details className="backup-tables">
                    <summary>{t("backup.tables", { schema: tableSchema, count: chosenTables.length })}</summary>
                    <div className="backup-choices">
                      {tables.map((name) => (
                        <label key={name} className="check">
                          <input type="checkbox" checked={chosenTables.includes(name)} onChange={(e) => setChosenTables((now) => toggle(now, name, e.target.checked))} />
                          {name}
                        </label>
                      ))}
                    </div>
                  </details>
                )}
              </fieldset>
            )}

            {!embedded && (
              <fieldset disabled={running}>
                <legend>{t("backup.options")}</legend>
                {request.family === "postgres" ? (
                  <>
                    <div className="backup-grid">
                      <label className="field">
                        {t("backup.format")}
                        <select value={pg.format} onChange={(e) => setPg({ ...pg, format: e.target.value as PgFormatItem })}>
                          {PG_FORMATS.map((f) => (
                            <option key={f} value={f}>
                              {t(`backup.formats.${f}`)}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="field">
                        {t("backup.compression")}
                        <select value={pg.compression} disabled={pg.format === "tar"} onChange={(e) => setPg({ ...pg, compression: e.target.value })}>
                          <option value="">{t("backup.compressionDefault")}</option>
                          {Array.from({ length: 10 }, (_, i) => (
                            <option key={i} value={String(i)}>
                              {i}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="field">
                        {t("backup.content")}
                        <select value={pg.content} onChange={(e) => setPg({ ...pg, content: e.target.value as DumpContentItem })}>
                          {CONTENTS.map((c) => (
                            <option key={c} value={c}>
                              {t(`backup.contents.${c}`)}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="field">
                        {t("backup.encoding")}
                        <input type="text" value={pg.encoding} placeholder={t("backup.encodingDefault")} onChange={(e) => setPg({ ...pg, encoding: e.target.value })} />
                      </label>
                    </div>
                    <div className="backup-checks">
                      {(
                        [
                          ["inserts", "pgInserts"],
                          ["noPrivileges", "pgNoPrivileges"],
                          ["noOwner", "pgNoOwner"],
                          ["clean", "pgClean"],
                          ["create", "pgCreate"],
                        ] as const
                      ).map(([key, label]) => (
                        <label key={key} className="check">
                          <input type="checkbox" checked={pg[key]} onChange={(e) => setPg({ ...pg, [key]: e.target.checked })} />
                          {t(`backup.${label}`)}
                        </label>
                      ))}
                    </div>
                  </>
                ) : (
                  <>
                    <div className="backup-grid">
                      <label className="field">
                        {t("backup.consistency")}
                        <select value={my.consistency} onChange={(e) => setMy({ ...my, consistency: e.target.value as MysqlConsistencyItem })}>
                          {CONSISTENCY.map((c) => (
                            <option key={c} value={c}>
                              {t(`backup.consistencies.${c}`)}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label className="field">
                        {t("backup.columnStatistics")}
                        <select value={my.columnStatistics} onChange={(e) => setMy({ ...my, columnStatistics: e.target.value as ColumnStatisticsItem })}>
                          {STATISTICS.map((c) => (
                            <option key={c} value={c}>
                              {t(`backup.statistics.${c}`)}
                            </option>
                          ))}
                        </select>
                      </label>
                    </div>
                    <div className="backup-checks">
                      {(
                        [
                          ["addDrop", "myAddDrop"],
                          ["noCreate", "myNoCreate"],
                          ["noData", "myNoData"],
                          ["extendedInsert", "myExtendedInsert"],
                          ["routines", "myRoutines"],
                          ["events", "myEvents"],
                          ["triggers", "myTriggers"],
                          ["includeViews", "myIncludeViews"],
                          ["stripDefiner", "myStripDefiner"],
                          ["hexBlob", "myHexBlob"],
                        ] as const
                      ).map(([key, label]) => (
                        <label key={key} className="check">
                          <input type="checkbox" checked={my[key]} onChange={(e) => setMy({ ...my, [key]: e.target.checked })} />
                          {t(`backup.${label}`)}
                        </label>
                      ))}
                    </div>
                  </>
                )}
              </fieldset>
            )}

            <fieldset disabled={running}>
              <legend>{t("backup.output")}</legend>
              <div className="backup-row">
                <input type="text" aria-label={t("backup.folder")} placeholder={t("backup.folder")} value={folder} onChange={(e) => setFolder(e.target.value)} />
                <button type="button" onClick={() => void pickFolder()}>
                  {t("backup.chooseFolderButton")}
                </button>
              </div>
              <label className="field">
                {t("backup.namePattern")}
                <input type="text" value={pattern} placeholder={prefs?.defaultNamePattern ?? ""} onChange={(e) => setPattern(e.target.value)} />
              </label>
              <p className="muted small">{t("backup.namePatternHint")}</p>
              {preview && (
                <p className="small backup-path" role="status">
                  {t("backup.willWrite")} <span className="code">{preview.path}</span>
                </p>
              )}
            </fieldset>
          </>
        )}

        {restoring && (
          <>
            <fieldset disabled={running}>
              <legend>{t("restoreDb.input")}</legend>
              <div className="backup-row">
                <input type="text" aria-label={t("restoreDb.file")} placeholder={t("restoreDb.file")} value={input} onChange={(e) => setInput(e.target.value)} />
                {request.family !== "duckdb" && (
                  <button type="button" onClick={() => void pickInput(false)}>
                    {t("restoreDb.chooseFileButton")}
                  </button>
                )}
                {(request.family === "postgres" || request.family === "duckdb") && (
                  <button type="button" onClick={() => void pickInput(true)}>
                    {t("restoreDb.chooseArchiveFolderButton")}
                  </button>
                )}
              </div>
              {request.family === "postgres" && (
                <label className="field">
                  {t("restoreDb.format")}
                  <select value={restoreFormat} onChange={(e) => setRestoreFormat(e.target.value as RestoreFormatItem)}>
                    {RESTORE_FORMATS.map((f) => (
                      <option key={f} value={f}>
                        {t(`restoreDb.formats.${f}`)}
                      </option>
                    ))}
                  </select>
                </label>
              )}
            </fieldset>
            {request.family !== "duckdb" && (
              <fieldset disabled={running}>
                <legend>{t("backup.options")}</legend>
                {request.family === "postgres" ? (
                  <div className="backup-checks">
                    {(
                      [
                        ["clean", "pgRestoreClean"],
                        ["noOwner", "pgRestoreNoOwner"],
                        ["create", "pgRestoreCreate"],
                      ] as const
                    ).map(([key, label]) => (
                      <label key={key} className="check">
                        <input type="checkbox" checked={pgRestore[key]} onChange={(e) => setPgRestore({ ...pgRestore, [key]: e.target.checked })} />
                        {t(`restoreDb.${label}`)}
                      </label>
                    ))}
                    <p className="muted small">{t("restoreDb.plainHint")}</p>
                  </div>
                ) : (
                  <>
                    <label className="field">
                      {t("restoreDb.database")}
                      <select value={myRestore.database} onChange={(e) => setMyRestore({ ...myRestore, database: e.target.value })}>
                        {[...new Set([database, ...schemas])].filter(Boolean).map((name) => (
                          <option key={name} value={name}>
                            {name}
                          </option>
                        ))}
                      </select>
                    </label>
                    <label className="check">
                      <input type="checkbox" checked={myRestore.disableForeignKeys} onChange={(e) => setMyRestore({ ...myRestore, disableForeignKeys: e.target.checked })} />
                      {t("restoreDb.disableForeignKeys")}
                    </label>
                  </>
                )}
              </fieldset>
            )}
          </>
        )}

        {!embedded && (
          <details className="backup-advanced">
            <summary>{t("backup.advanced")}</summary>
            <label className="field">
              {t("backup.extraArgs")}
              <input type="text" className="code" value={extraArgs} disabled={running} onChange={(e) => setExtraArgs(e.target.value)} />
            </label>
            <p className="muted small">{t("backup.extraArgsHint")}</p>
          </details>
        )}

        {preview?.notes.map((note, i) => (
          <p key={i} className="banner" role="status">
            {note}
          </p>
        ))}
        {previewError && folder.trim() && (
          <p className="error small" role="alert">
            {t("backup.cannotStart", { reason: previewError })}
          </p>
        )}
        {missingTool && !previewError && (
          <p className="error small" role="alert">
            {t("backup.missingTool")}
          </p>
        )}
        {confirmError && (
          <p className="error small" role="alert">
            {t("restoreDb.cannotStart", { reason: confirmError })}
          </p>
        )}

        {(log.length > 0 || running) && (
          <pre ref={logRef} className="backup-log" role="log" aria-live="polite" aria-label={t("backup.log")} tabIndex={0}>
            {log.join("\n")}
          </pre>
        )}
        {phase.kind === "running" && (
          <p role="status" className="backup-progress">
            {phase.total
              ? t("restoreDb.progress", { read: bytes(phase.bytes), total: bytes(phase.total) })
              : phase.bytes > 0
                ? t("backup.progress", { size: bytes(phase.bytes) })
                : restoring
                  ? t("restoreDb.running")
                  : t("backup.running")}
          </p>
        )}
        {phase.kind === "done" && (
          <p role="status" className="backup-result">
            {phase.text}
          </p>
        )}
        {phase.kind === "failed" && (
          <p role="alert" className="error backup-result">
            {phase.text}
          </p>
        )}

        <div className="modal-actions">
          {running ? (
            <button disabled={runId === null} onClick={() => runId !== null && void call(commands.cancelBackup(runId)).catch(() => undefined)}>
              {restoring ? t("restoreDb.cancel") : t("backup.cancel")}
            </button>
          ) : (
            <>
              <button onClick={close}>{t("backup.close")}</button>
              {restoring ? (
                <button className="primary" disabled={!canRestore} onClick={() => void askRestore()}>
                  {t("restoreDb.start")}
                </button>
              ) : (
                <button className="primary" disabled={!canBackup} onClick={() => void startBackup()}>
                  {phase.kind === "done" ? t("backup.again") : t("backup.start")}
                </button>
              )}
            </>
          )}
        </div>
      </div>

      {confirm && (
        <div className="modal-backdrop">
          <div className="modal backup-confirm" role="dialog" aria-modal="true" aria-label={t("restoreDb.confirmTitle", { database: database || request.connectionName })}>
            <h2>{t("restoreDb.confirmTitle", { database: database || request.connectionName })}</h2>
            <p>
              {t("restoreDb.confirmBody", {
                file: input,
                connection: request.connectionName,
                tool: confirm.version || confirm.tool,
                format: request.family === "duckdb" ? t("restoreDb.duckdbExport") : t(`restoreDb.formats.${confirm.format}`),
              })}
            </p>
            {request.family === "postgres" && pgRestore.clean && <p>{t("restoreDb.confirmClean")}</p>}
            {confirm.notes.map((note, i) => (
              <p key={i} className="banner">
                {note}
              </p>
            ))}
            {confirm.readOnly && (
              <p className="error small" role="alert">
                {t("restoreDb.readOnly")}
              </p>
            )}
            <div className="modal-actions">
              <button ref={confirmRef} autoFocus onClick={() => setConfirm(null)}>
                {t("restoreDb.keep")}
              </button>
              <button className="danger" disabled={confirm.readOnly} onClick={() => void startRestore()}>
                {t("restoreDb.confirm")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
