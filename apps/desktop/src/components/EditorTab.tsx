// An editor tab: its own session in the core (ADR-004), editor, execution and results.

import { useConnectionName } from "../folders";
import { useEffect, useMemo, useRef, useState } from "react";
import { type ConnectionKey, connectionOfTag, connectionTag, keyOf, listedConnection, stillOnConnection, tabConnection, tabConnectionOf, tabOnConnection } from "../connectionKey";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { call, commands, type CellEditItem, type CellInputItem, type ColumnFilterItem, type PendingEditItem, type PendingEdits, type ScriptEvent } from "../api";
import { startJob, type Job } from "../jobs";
import type { ResolvedScriptItem, ScriptEchoItem } from "../bindings";
import type { ExplorerDatabaseItem_Serialize as ExplorerDatabaseItem, KeyTableItem, LoadEstimateItem_Serialize as LoadEstimateItem, LoadSourceItem, PlanSummary_Serialize as PlanSummary, PlanCapabilities_Serialize as PlanCapabilities, ResultPlacementItem, StatementEndItem } from "../bindings";
import { byteSize } from "../plugins";
import { PlanViewer } from "./PlanViewer";
import { DEFAULT_PAGE_SIZE, EDITOR_COMMAND_EVENT, EDITS_COMMAND_EVENT, SAVE_QUERY_EVENT, useApp, type EditorCommand, useDialogs, type FilterMode, type ResultMeta, type ResultTabCommand, type RunState, type SortState, type TableView, type Tab, type Translation } from "../store";
import { confirmOpenTransactions, connectTabFromExplorer, explorerDatabase, openTabSession, schemasRefreshed, SET_ACTIVE_SCHEMA_EVENT, tabLabel } from "../connect";
import { detachResults, reattachResults, sendMirror, serveRequests, type ResultsMirror, type ResultsRequest } from "../resultsWindow";
import { connectionTooltip } from "../connectionInfo";
import { afterRun, endResult, markRefreshable, sameResultColumns, withoutResults, withStartedResult } from "../runResults";
import { ResultTabList } from "./ResultTabs";
import { VariablesDialog } from "./VariablesDialog";
import { byteToIndex } from "../text";
import { latestOnly } from "../latestRequest";
import { ExportButton } from "./ExportButton";
import { ResultFilter } from "./ResultFilter";
import { defaultChart, ResultChart, type ChartSettings } from "./ResultChart";
import { ResultGrid, type GridEditing, type GridEditingState } from "./ResultGrid";
import { TransposedView } from "./TransposedView";
import { RunOptionsButton, runOptionsOf } from "./RunOptions";
import { showsSummary, summaryMessages, summaryShort } from "../scriptRun";
import { runStatus } from "../runState";
import { NOT_WAITING, notRunKey, press, runBlock, settle, waitingKey, type RunBlock, type SessionNow, type WaitingRun } from "../runWait";
import { CONNECTING, DISCONNECTED, cancelling, connectFailed, connectFailure, connectedWith, isRunning, isConnecting, lostSession, runStateOf, serverVersion, sessionReadOnly, statusName } from "../tabState";
import { saveTab, saveTabAs } from "../sqlFiles";
import { notifyFinished } from "../notify";
import { EditorActionsButton } from "./EditorMenu";
import { SqlEditor, type NameAction, type RunLabel, type RunRequest, type SqlEditorHandle } from "./SqlEditor";
import { ScriptOutline } from "./ScriptOutline";
import type { ResolvedName } from "../sqlCompletion";
import { commandShortcut, commandTitle } from "../shortcuts";
import { TableDataBar } from "./TableDataBar";
import { TransactionControls } from "./TransactionControls";
import { TypeBadge } from "./TypeBadge";
import { toggleResultsMaximized } from "../tabActions";
import { Icon } from "./Icons";
import { MissingPlugin } from "./MissingPlugin";
import { useCompletionCatalog } from "../useCompletionCatalog";
import { mayChangeCompletionCatalog } from "../sqlCompletion";
import { useEngineInfo } from "../engines";
import { ended, partialText } from "../resultEnd";

/** Engines whose sessions can change database with `USE` (a selector per tab). */
/** No header sort: one list, so the grid sees the same value between renders. */
const NO_TABLE_SORT: SortState[] = [];

function databaseSwitching(engine: string | undefined): boolean {
  return engine === "mysql" || engine === "mariadb" || engine === "sqlserver";
}

/** No pending edits, as the core answers for a page it never edited. */
function emptyPending(): PendingEdits {
  return { cells: [], deleted: [], inserts: [], updatedRows: 0, canUndo: false, canRedo: false, changed: 0, clipped: 0 };
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function translated(text: Translation, t: TFunction<"editor">, numbers: Intl.NumberFormat): string {
  const values: Record<string, string | number> = {};
  for (const [key, value] of Object.entries(text.values ?? {})) {
    values[key] = typeof value === "number" && key !== "count" ? numbers.format(value) : value;
  }
  if (typeof values.count === "number") values.formattedCount = numbers.format(values.count);
  return t(text.key, values);
}

/** The Messages line of a statement that did not run to its end, by how it ended. */
function statementEndKey(end: StatementEndItem): string {
  switch (end) {
    case "failed":
      return "messages.statementFailed";
    case "cancelled":
      return "messages.statementCancelled";
    case "timedOut":
      return "messages.statementTimedOut";
    default:
      return unknownEnd(end);
  }
}

function unknownRequest(request: never): never {
  throw new Error(`unknown results request: ${JSON.stringify(request as ResultsRequest)}`);
}

function unknownEnd(end: never): never {
  throw new Error(`unknown statement end: ${JSON.stringify(end)}`);
}

function translatedError(error: NonNullable<RunState["error"]>, t: TFunction<"editor">, numbers: Intl.NumberFormat): string {
  const diagnostic = error.diagnostic;
  if (!diagnostic) return error.translation ? translated(error.translation, t, numbers) : error.message;
  const parts = [diagnostic.cancelled ? t("messages.cancelled") : diagnostic.message];
  if (diagnostic.detail) parts.push(t("messages.detail", { detail: diagnostic.detail }));
  if (diagnostic.hint) parts.push(t("messages.hint", { hint: diagnostic.hint }));
  if (diagnostic.code) parts.push(t("messages.code", { code: diagnostic.code }));
  return parts.join("\n");
}

/** A run the user pressed, as it was then (SDB-525): its text and where its results go. */
type PressedRun =
  | { kind: "run"; request: RunRequest; placement: ResultPlacementItem }
  | { kind: "explain"; request: RunRequest };

export function EditorTab({ tab }: { tab: Tab }) {
  // Its connection, as the status line, the buttons and the effects below read it (SDB-371).
  const tabStatus = statusName(tab);
  const running = isRunning(tab);
  const { t, i18n } = useTranslation("editor");
  const shownName = useConnectionName();
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const connections = useApp((s) => s.connections);
  const updateTab = useApp((s) => s.updateTab);
  const setConnection = useApp((s) => s.setConnection);
  const updateRun = useApp((s) => s.updateRun);
  const historySaving = useApp((s) => {
    const connection = tabConnection(tab);
    return connection !== null && s.historySaving.includes(connectionTag(connection));
  });
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const ownConnection = tabConnection(tab);
  const connection = (ownConnection ? listedConnection(connections, ownConnection) : undefined) ?? null;
  const supportsDatabase = !tab.table && databaseSwitching(connection?.engine);
  // A schema picker where the session has a schema of its own and no database picker does it
  // (PostgreSQL, DuckDB): it shows, and changes, where unqualified names are found (ADR-037).
  const engineInfo = useEngineInfo(connection?.engine);
  const supportsSchema = !tab.table && !supportsDatabase && Boolean(engineInfo?.sessionSchema);
  // Behind a pooler in transaction mode the session keeps no schema of its own (SDB-396): the
  // picker shows where names are found but cannot change it (the core refuses too).
  const schemaPooled = connection?.pooler === "transaction";
  const [schemas, setSchemas] = useState<string[]>([]);
  const [schema, setSchema] = useState<string | null>(null);
  const [schemaBusy, setSchemaBusy] = useState(false);
  const [schemaRefresh, setSchemaRefresh] = useState(0);
  const [databases, setDatabases] = useState<string[]>([]);
  const [database, setDatabase] = useState<string | null>(null);
  const [databaseBusy, setDatabaseBusy] = useState(false);
  const [databaseError, setDatabaseError] = useState(false);
  /** A PostgreSQL connection that opens several databases (ADR-074, SDB-505): the databases the
   *  tab can be on, as the core lists them (its connection's own first); null without a choice. */
  const [tabDatabases, setTabDatabases] = useState<ExplorerDatabaseItem[] | null>(null);
  const [tabDatabaseBusy, setTabDatabaseBusy] = useState(false);
  const [databaseRefresh, setDatabaseRefresh] = useState(0);
  const databaseBusyRef = useRef(false);
  const workspace = useApp((s) => s.workspace?.workspace ?? null);
  // SQL completion only where the language is SQL (ADR-056).
  const completion = useCompletionCatalog(tab.id, ownConnection ? connectionTag(ownConnection) : null, workspace, database, tabStatus === "connected" && !tab.table && connection?.sql !== false && (!supportsDatabase || database !== null), tab.sql, connection?.engine ?? null);
  const [editorShare, setEditorShare] = useState(0.42);
  const body = useRef<HTMLDivElement>(null);
  /** This tab's runs, counted: each result remembers which one produced it (SDB-378). */
  const runSeq = useRef(0);
  /** The text each run executed and how many results it returned, by run: a result is
   *  editable only when it is the whole answer of its run, whose text the core resolves. */
  const runTexts = useRef(new Map<number, { text: string; source: string; results: number; canRefresh: boolean }>());
  const startingRun = useRef(false);
  const restoringView = useRef(false);
  const [refreshRestoring, setRefreshRestoring] = useState(false);
  /** The text of each statement of the running script, by index: what an export runs again. */
  const statementTexts = useRef<Record<number, string>>({});
  const [variablesOpen, setVariablesOpen] = useState(false);
  /** This tab as the store has it now: its variables and remembered parameters (SDB-383). */
  const tabNow = () => useApp.getState().tabs.find((item) => item.id === tab.id);

  /** Asks the core to cancel the run; the tab shows "Cancelling…" once the core says it is
   *  (its run state, SDB-370), and until the run has ended. */
  async function cancelRun() {
    const current = tabNow();
    if (!current?.run || !isRunning(current) || cancelling(current)) return;
    const startedAt = current.run.startedAt;
    try {
      await call(commands.cancel(tab.id));
    } catch (error) {
      const latest = tabNow();
      if (isRunning(latest) && latest?.run?.startedAt === startedAt) {
        setNotice({ text: t("results.cancelFailed", { detail: message(error) }), error: true });
      }
    }
  }

  /** `text` with the tab's and its connection's `${name}` variables written in by the core
   *  (SDB-383), before the parameters, the guard and the run read it. A variable without a
   *  value is asked for and kept for this tab. `null` when the user cancels; throws what the
   *  core refuses (a value that would change how the statement is split or read). */
  async function withVariables(text: string): Promise<ResolvedScriptItem | null> {
    let values = tabNow()?.variables ?? {};
    const used = await call(commands.scriptVariables(tab.id, text, values));
    if (used.missing.length > 0) {
      const asked = await useDialogs.getState().askParams(used.missing, {}, true);
      if (!asked) return null;
      values = { ...values, ...asked };
      updateTab(tab.id, { variables: values });
    }
    // The core resolves the script's `@set`, `@unset` and `@echo` lines too (ADR-078): what it
    // returns is what runs and what the guard reads. A script's `@set` never changes the tab's
    // values.
    return call(commands.resolveScript(tab.id, text, values));
  }

  /** `text` with its `:name` parameters written in as literals by the core. Each parameter's
   *  last value in this tab is offered again. `null` when the user cancels. */
  async function withParams(text: string): Promise<string | null> {
    const names = await call(commands.scriptParams(tab.id, text));
    if (names.length === 0) return text;
    const remembered = tabNow()?.params ?? {};
    const values = await useDialogs.getState().askParams(names, remembered);
    if (!values) return null;
    updateTab(tab.id, { params: { ...remembered, ...values } });
    return call(commands.bindParams(tab.id, text, values));
  }
  const editor = useRef<SqlEditorHandle>(null);
  const [plan, setPlan] = useState<PlanSummary | null>(null);
  const [planOpen, setPlanOpen] = useState(false);
  const [analyzePlan, setAnalyzePlan] = useState(false);
  const [planBuffers, setPlanBuffers] = useState(true);
  const [capabilities, setCapabilities] = useState<PlanCapabilities | null>(null);
  const explainAction = useRef<() => Promise<void>>(async () => undefined);
  // A short message in the toolbar: a formatting error or "saved as …".
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  /** A Run the user pressed while the tab could not start one (SDB-525): it waits, said in the
   *  toolbar, and runs once. The ref is what a press in the same moment reads. */
  const [waitingRun, setWaitingRunState] = useState<WaitingRun<PressedRun>>(NOT_WAITING);
  const waitingRef = useRef<WaitingRun<PressedRun>>(NOT_WAITING);
  const setWaitingRun = (next: WaitingRun<PressedRun>) => { waitingRef.current = next; setWaitingRunState(next); };
  const [runLabel, setRunLabel] = useState<RunLabel | null>(null);
  /** The outline of the script, beside the editor (Ctrl+Shift+O). */
  const [outlineOpen, setOutlineOpen] = useState(false);

  /** From a name in the SQL to its table (SDB-382): the table's details in a tab of its own,
   *  opened like this tab's session, or its row in the explorer. The name was resolved from
   *  the catalog already read; a column leads to its table. */
  function showName(name: ResolvedName, action: NameAction) {
    const connection = tabConnection(tab);
    if (!connection) return;
    const { schema, name: table } = name.table;
    const app = useApp.getState();
    if (action === "reveal") {
      app.setSidebarSection("connections");
      app.revealInExplorer({ connection, schema, name: table, connect: true });
      return;
    }
    const existing = app.tabs.some((other) => tabOnConnection(other, connection) && other.tableDetails?.schema === schema
      && other.tableDetails.name === table && other.tableDetails.kind === "columns");
    const details = app.addTableDetailsTab(connection, schema, table, "columns");
    if (!existing) connectTabFromExplorer(details, tab.id, explorerDatabase(connection, schema) ?? database ?? undefined);
  }
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), notice.error ? 6000 : 2500);
    return () => clearTimeout(timer);
  }, [notice]);

  // Pending edits of a table data tab (ADR-044): the core holds them with the page's result;
  // this is only what it answered last, to draw them. Edits go to the core one at a time, in
  // order, and only the answer to the latest one is shown (a cell typed shows at once).
  const [pendingView, setPendingView] = useState<{ result: number; edits: PendingEdits } | null>(null);
  const editQueue = useRef<Promise<unknown>>(Promise.resolve());
  const editSeq = useRef(0);
  const shownPending = pendingView && pendingView.result === tab.run?.activeResult ? pendingView.edits : null;
  const pending = useMemo(() => {
    const cells: Record<string, string | null> = {};
    const defaults = new Set<string>();
    for (const cell of shownPending?.cells ?? []) {
      const key = `${cell.row}:${cell.column}`;
      cells[key] = cell.value;
      if (cell.isDefault) defaults.add(key);
    }
    return {
      cells,
      defaults,
      deleted: shownPending?.deleted ?? [],
      inserts: (shownPending?.inserts ?? []) as CellEditItem[][],
      updatedRows: shownPending?.updatedRows ?? 0,
      canUndo: shownPending?.canUndo ?? false,
      canRedo: shownPending?.canRedo ?? false,
    };
  }, [shownPending]);
  const pendingCount = Object.keys(pending.cells).length + pending.deleted.length + pending.inserts.length;
  const clearPending = () => {
    editSeq.current += 1;
    setPendingView(null);
  };
  /** The table's primary key (`null` until known): editing needs one. */
  const [primaryKey, setPrimaryKey] = useState<string[] | null>(null);
  const [columnComments, setColumnComments] = useState<Record<string, string>>({});
  const [queryTarget, setQueryTarget] = useState<{ result: number; table: KeyTableItem | null } | null>(null);
  /** Its edits are being applied: kept once, for the store, this render and a second click. */
  const saving = useFlag();
  const savingEdits = saving.on;
  /** A page load is between its start and the start of its run. */
  const pageLoad = useFlag();
  const pageLoading = pageLoad.on;
  const queryPageBusy = useRef(false);
  const [queryPageSize, setQueryPageSize] = useState(DEFAULT_PAGE_SIZE);
  useEffect(() => {
    updateTab(tab.id, { pendingEdits: pendingCount > 0, savingEdits });
  }, [tab.id, pendingCount, savingEdits, updateTab]);

  const activeResult = tab.run?.results.find((result) => result.id === tab.run?.activeResult);
  const resultId = activeResult?.id;
  const resultRun = activeResult?.run === undefined ? undefined : runTexts.current.get(activeResult.run);
  const queryText = ended(activeResult) && resultRun?.results === 1 ? resultRun.text : undefined;
  const readOnly = !!connection?.readOnly || sessionReadOnly(tab);

  // Resolve the table once for both editing and foreign keys. View updates (sorting,
  // filtering, counting and appending rows) do not change the result's source.
  useEffect(() => {
    if (tab.table || tabStatus !== "connected" || running || resultId === undefined || queryText === undefined) return;
    let active = true;
    call(commands.queryKeyTable(tab.id, resultId, queryText))
      .then((table) => { if (active) setQueryTarget({ result: resultId, table }); })
      .catch(() => { if (active) setQueryTarget({ result: resultId, table: null }); });
    return () => { active = false; };
  }, [tab.id, tab.table, tabStatus, running, database, readOnly, resultId, queryText]);

  useEffect(() => {
    if (tabStatus !== "connected") setQueryTarget(null);
  }, [tabStatus]);

  const queryTable = tabStatus === "connected" && queryTarget && queryTarget.result === resultId
    ? queryTarget.table
    : undefined;
  const editTarget = readOnly ? null : tab.table
    ? (primaryKey?.length ? { ...tab.table, primaryKey } : null)
    : (queryTable?.writable ? queryTable : null);
  /** The table whose foreign keys the active result follows. */
  const keyTable = tab.table
    ? { schema: tab.table.schema, name: tab.table.name, connection: tabConnection(tab) }
    : (queryTable ? { schema: queryTable.schema, name: queryTable.name, connection: tabConnection(tab) } : undefined);
  const checkingEditability = tab.table
    ? primaryKey === null
    : !ended(activeResult) || running || (queryText !== undefined && queryTable === undefined);
  const editingState: GridEditingState = readOnly || tabStatus !== "connected" ? "readOnly"
    : editTarget ? "available"
    : checkingEditability ? "checking" : "readOnly";

  // A page shown again (another result tab, a tab restored) shows the edits its core holds.
  const activeResultId = activeResult && ended(activeResult) ? activeResult.id : null;
  useEffect(() => {
    if (activeResultId === null) return;
    let live = true;
    const seq = editSeq.current;
    call(commands.pendingEdits(activeResultId))
      .then((edits) => { if (live && seq === editSeq.current) setPendingView({ result: activeResultId, edits }); })
      .catch(() => undefined);
    return () => { live = false; };
  }, [activeResultId]);

  /** Sends one edit to the core, after the ones before it; `local` draws it before the answer. */
  function sendEdit(edit: PendingEditItem, local?: (cells: PendingEdits) => PendingEdits): Promise<PendingEdits | null> {
    const result = activeResult;
    if (!result) return Promise.resolve(null);
    const seq = ++editSeq.current;
    if (local) setPendingView((p) => ({ result: result.id, edits: local(p && p.result === result.id ? p.edits : emptyPending()) }));
    const sent = editQueue.current.then(async () => {
      try {
        const edits = await call(commands.editPending(result.id, edit));
        if (seq === editSeq.current) setPendingView({ result: result.id, edits });
        reportEdit(edit, edits);
        return edits;
      } catch (e) {
        setNotice({ text: t("edit.editFailed", { detail: message(e) }), error: true });
        // Show what the core holds, not what was drawn before it refused.
        if (seq === editSeq.current) {
          const edits = await call(commands.pendingEdits(result.id)).catch(() => null);
          if (edits && seq === editSeq.current) setPendingView({ result: result.id, edits });
        }
        return null;
      }
    });
    editQueue.current = sent;
    return sent;
  }

  /** A short note for edits whose effect is not obvious in the grid. */
  function reportEdit(edit: PendingEditItem, edits: PendingEdits) {
    if (edits.clipped > 0) setNotice({ text: t("edit.pasteClipped", { count: edits.clipped }), error: false });
    else if (edit.kind === "undo" || edit.kind === "redo") {
      if (edits.changed === 0) setNotice({ text: t(edit.kind === "undo" ? "edit.nothingToUndo" : "edit.nothingToRedo"), error: false });
    } else if (edit.kind === "bulk" || edit.kind === "paste") {
      setNotice({ text: edits.changed > 0 ? t("edit.cellsChanged", { count: edits.changed }) : t("edit.noCellChanged"), error: false });
    }
  }

  const cellInput = (value: string | null | undefined): CellInputItem | null =>
    value === undefined ? null : value === null ? { kind: "null" } : { kind: "text", text: value };
  /** The cell drawn with its new value at once (the core's answer follows). */
  const localCell = (row: number, column: number, value: string | null | undefined) => (edits: PendingEdits): PendingEdits => ({
    ...edits,
    cells: [
      ...edits.cells.filter((c) => c.row !== row || c.column !== column),
      ...(value === undefined ? [] : [{ row, column, value, isDefault: false }]),
    ],
  });

  const editing: GridEditing | undefined =
    editTarget && ended(activeResult) && tabStatus === "connected"
      ? {
          cells: pending.cells,
          defaults: pending.defaults,
          deleted: pending.deleted,
          inserts: pending.inserts,
          updatedRows: pending.updatedRows,
          canUndo: pending.canUndo,
          canRedo: pending.canRedo,
          primaryKey: editTarget.primaryKey,
          table: { schema: editTarget.schema, name: editTarget.name },
          busy: running || savingEdits || pageLoading || (activeResult?.paging?.loading ?? false),
          onInsert: (values) => void sendEdit({ kind: "insert", values }),
          onUpdateInsert: (index, values) => void sendEdit({ kind: "updateInsert", index, values }),
          onRemoveInsert: (index) => void sendEdit({ kind: "removeInsert", index }),
          onApply: () => void applyEdits(),
          onApplyCell: (row, col, value, dirty) => {
            if (dirty) void sendEdit({ kind: "setCells", cells: [{ row, column: col, value: cellInput(value) }] }, localCell(row, col, value));
            void applyEdits();
          },
          onDiscard: () => void sendEdit({ kind: "discard" }),
          onLoadFile: (row, col, path) => loadValueFile(row, col, path),
          onEdit: (row, col, value) =>
            void sendEdit({ kind: "setCells", cells: [{ row, column: col, value: cellInput(value) }] }, localCell(row, col, value)),
          onType: (row, col, value) =>
            void sendEdit({ kind: "type", cell: { row, column: col, value: cellInput(value) } }, localCell(row, col, value)),
          onDelete: (rows) => void sendEdit({ kind: "delete", rows }),
          onRevert: (rows) => void sendEdit({ kind: "revert", rows }),
          onBulk: (block, action) => void sendEdit({ kind: "bulk", block, action }),
          onPaste: (selection, columns, text) => void sendEdit({ kind: "paste", selection, columns, text }),
          onUndo: () => void sendEdit({ kind: "undo" }),
          onRedo: () => void sendEdit({ kind: "redo" }),
        }
      : undefined;
  const editingRef = useRef(editing);
  editingRef.current = editing;
  // Undo and redo from the command palette reach this tab's pending edits.
  useEffect(() => {
    const onCommand = (e: Event) => {
      const detail = (e as CustomEvent<{ tab: string; command: "undo" | "redo" }>).detail;
      const target = editingRef.current;
      if (detail.tab !== tab.id || !target || target.busy) return;
      if (detail.command === "undo") target.onUndo();
      else target.onRedo();
    };
    window.addEventListener(EDITS_COMMAND_EVENT, onCommand);
    return () => window.removeEventListener(EDITS_COMMAND_EVENT, onCommand);
  }, [tab.id]);

  /** Applies the edits the core holds; it writes and applies them in one transaction. */
  async function applyEdits() {
    const view = editTarget;
    const result = activeResult;
    if (!view || !result || saving.now()) return;
    saving.set(true);
    try {
      // Every edit sent before Apply reaches the core first.
      await editQueue.current;
      const held = await call(commands.pendingEdits(result.id));
      if (held.cells.length + held.deleted.length + held.inserts.length === 0) return;
      let confirmed = false;
      // The core decided the type's safeguards; it refuses unconfirmed edits too.
      if (connection?.kind?.safeguards.confirmDataChanges) {
        const statements = await call(commands.previewTableEdits(tab.id, result.id, view.schema, view.name));
        const ok = await useDialogs.getState().askRisks(connection.name, statements.map((sql) => ({ sql, risk: "dataChange" as const })));
        if (!ok) return;
        confirmed = true;
      }
      const applied = await call(commands.applyTableEdits(tab.id, result.id, view.schema, view.name, confirmed));
      clearPending();
      setNotice({ text: t("edit.applied", { count: applied }), error: false });
      if (tab.table) void loadPage(tab.table);
      else rerun(result.id, true);
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      saving.set(false);
    }
  }
  /** Loads a file into one cell at once: the core reads it and writes one UPDATE by primary
   *  key, applied like any edit (production asks first, read-only refuses). */
  async function loadValueFile(row: number, column: number, path: string) {
    const view = editTarget;
    const result = activeResult;
    if (!view || !result || saving.now()) return;
    saving.set(true);
    try {
      let confirmed = false;
      if (connection?.kind?.safeguards.confirmDataChanges) {
        const statements = await call(commands.previewLoadValueFile(tab.id, result.id, view.schema, view.name, row, column, path));
        const ok = await useDialogs.getState().askRisks(connection.name, statements.map((sql) => ({ sql, risk: "dataChange" as const })));
        if (!ok) return;
        confirmed = true;
      }
      await call(commands.loadValueFile(tab.id, result.id, view.schema, view.name, row, column, path, confirmed));
      const file = path.split(/[\\/]/).pop() ?? path;
      setNotice({ text: t("edit.fileLoaded", { file, column: result.columns[column]?.name ?? "" }), error: false });
      if (tab.table) void loadPage(tab.table);
      else rerun(result.id, true);
    } catch (e) {
      setNotice({ text: t("edit.fileLoadFailed", { detail: message(e) }), error: true });
    } finally {
      saving.set(false);
    }
  }

  /** The page asked for while another one was loading: it loads when that one ends. */
  const queuedPage = useRef<TableView | null>(null);

  /** Loads a page of a table data tab: the core writes and checks the SELECT. */
  async function loadPage(view: TableView) {
    const busy = () => pageLoad.now() || isRunning(useApp.getState().tabs.find((t) => t.id === tab.id));
    if (busy()) {
      queuedPage.current = view;
      return;
    }
    // Set before any await: a second click must queue, not start a race with this load.
    pageLoad.set(true);
    try {
      await loadPageNow(view);
    } finally {
      pageLoad.set(false);
      const queued = queuedPage.current;
      // A load that never started a run (an error, no connection) does not end one either.
      if (queued && !busy()) {
        queuedPage.current = null;
        void loadPage(queued);
      }
    }
  }

  async function loadPageNow(view: TableView) {
    if (!(await ensureConnected())) return;
    if (primaryKey === null) {
      call(commands.describeTable(tab.id, view.schema, view.name, null))
        .then((details) => {
          setPrimaryKey(details.primaryKey);
          // The columns' comments, for the grid's header (SDB-385).
          setColumnComments(Object.fromEntries(details.columns.flatMap((column) => (column.comment ? [[column.name, column.comment]] : []))));
        })
        .catch(() => setPrimaryKey([]));
    }
    clearPending();
    let sql: string;
    try {
      // "Load every row" (SDB-386): the same SELECT without its paging, checked the same way.
      sql = view.all
        ? await call(commands.tableRowsSql(tab.id, view.schema, view.name, view.filter, view.orderBy))
        : await call(
          commands.tablePageSql(tab.id, {
            schema: view.schema,
            name: view.name,
            filter: view.filter,
            orderBy: view.orderBy,
            limit: view.pageSize,
            offset: view.page * view.pageSize,
          }),
        );
    } catch (e) {
      updateTab(tab.id, { run: failedRun(message(e)) });
      return;
    }
    await run({ text: sql, from: 0 }, true);
  }

  /** Changes what the table data tab shows and loads it again. */
  function changeView(patch: Partial<TableView>) {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id)?.table;
    if (!current) return;
    if (pendingCount > 0 || saving.now()) {
      setNotice({ text: t("edit.applyFirst"), error: true });
      return;
    }
    const next = { ...current, ...patch };
    updateTab(tab.id, { table: next });
    void loadPage(next);
  }

  /** Header clicks in a table data tab: the core writes the ORDER BY from the table's own
   *  columns (the tab only names them), and the page is read again from the server. */
  const latestSort = useRef(latestOnly());
  async function sortTable(sort: SortState[]) {
    const view = tab.table;
    if (!view) return;
    const columns = tab.run?.results[0]?.columns ?? [];
    const keys = sort.map((key) => ({ column: columns[key.column]?.name, descending: key.descending }));
    if (keys.some((key) => key.column === undefined)) return;
    try {
      // Clicks can come faster than the core answers, and the answers in either order: only
      // the last click's ORDER BY changes the view (SDB-409). An older answer that arrived
      // late used to put its sort back over the newer one.
      const answered = await latestSort.current(() => call(commands.tableOrderBy(tab.id, view.schema, view.name,
        keys.map((key) => ({ column: key.column ?? "", descending: key.descending })))));
      if (!answered.latest) return;
      changeView({ sort: sort.length > 0 ? sort : undefined, orderBy: answered.value, page: 0 });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }

  /** Header clicks on a paged query (ADR-054): reads it again from the server in that order,
   *  its first page replacing the rows shown. `false` when the core cannot page the query
   *  that way: the grid then sorts the rows it has loaded. */
  async function sortQuery(resultId: number, sort: SortState[]): Promise<boolean> {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const result = current?.run?.results.find((item) => item.id === resultId);
    const paging = result?.paging;
    if (!paging) return false;
    if (isRunning(current) || queryPageBusy.current || startingRun.current) throw new Error(t("results.sortBusy"));
    if (sort.length > 0 && !(await call(commands.queryPageSupported(tab.id, paging.sql, sort)).catch(() => false))) return false;
    // The result's own query, into its own tab (SDB-378): the other result tabs stay.
    await run({ text: paging.sql, from: 0 }, false, paging.pageSize, sort, { kind: "into", result: resultId }, result?.refreshSql);
    return true;
  }

  const countSequence = useRef(0);
  async function countTable() {
    const view = tab.table;
    const counted = tabConnection(tab);
    const sequence = ++countSequence.current;
    if (!view || !(await ensureConnected())) return;
    const matchingView = () => {
      const current = useApp.getState().tabs.find((item) => item.id === tab.id);
      if (sequence !== countSequence.current || !current || !stillOnConnection(current, counted) ||
          !current.table || current.table.schema !== view.schema || current.table.name !== view.name ||
          current.table.filter !== view.filter) return undefined;
      return current.table;
    };
    if (!matchingView()) return;
    try {
      const total = await call(commands.countTableRows(tab.id, view.schema, view.name, view.filter));
      const current = matchingView();
      if (current) updateTab(tab.id, { table: { ...current, total: total ?? 0 } });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }

  /** The last page of a table data tab (SDB-386): the core counts the rows the filter keeps,
   *  then the tab shows the page that holds the last of them. */
  async function lastTablePage() {
    const view = tab.table;
    if (!view || !(await ensureConnected())) return;
    try {
      const last = await call(commands.lastTablePage(tab.id, view.schema, view.name, view.filter, view.pageSize));
      const current = useApp.getState().tabs.find((item) => item.id === tab.id)?.table;
      if (!current || current.schema !== view.schema || current.name !== view.name || current.filter !== view.filter) return;
      changeView({ page: last.page, total: last.total ?? 0 });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }

  /** Asks before loading every row (SDB-386): how many rows the server counts and roughly how
   *  much memory they take, from what the rows loaded take now (both the core's). */
  async function confirmLoadAll(resultId: number, source: LoadSourceItem): Promise<boolean> {
    setNotice({ text: t("pages.counting"), error: false });
    let estimate: LoadEstimateItem;
    try {
      estimate = await call(commands.loadEstimate(tab.id, resultId, source));
    } catch (e) {
      setNotice({ text: t("pages.loadFailed", { detail: message(e) }), error: true });
      return false;
    }
    setNotice(null);
    // Counts cross as f64, which the bindings type as nullable.
    const rows = estimate.total ?? 0;
    if (estimate.tooMany) {
      setNotice({ text: t("pages.tooMany", { total: numbers.format(rows), max: numbers.format(estimate.maxRows ?? 0) }), error: true });
      return false;
    }
    const total = numbers.format(rows);
    const choice = await useDialogs.getState().askChoice({
      title: t("pages.loadAllTitle"),
      message: estimate.bytes === null
        ? t("pages.loadAllBodyUnknown", { total })
        : t("pages.loadAllBody", { total, size: byteSize(estimate.bytes, i18n.language) }),
      choices: [
        { id: "cancel", label: t("results.cancel") },
        { id: "load", label: t("pages.loadAllConfirm", { count: rows, formattedCount: total }), primary: true },
      ],
    });
    return choice === "load";
  }

  /** "Load every row" of a table data tab: every row the filter keeps, in its pages' order. */
  async function loadAllTable() {
    const view = tab.table;
    const result = tab.run?.results[0];
    if (!view || !result || !(await ensureConnected())) return;
    if (!(await confirmLoadAll(result.id, { kind: "table", schema: view.schema, name: view.name, filter: view.filter }))) return;
    changeView({ all: true, page: 0 });
  }

  /** "Go to row" in a table data tab (SDB-386): `row` counts from 1 in the tab's order, so it
   *  is on page `(row - 1) / pageSize`, read as any page is. */
  const pendingRow = useRef<number | null>(null);
  function goToTableRow(row: number) {
    const view = tab.table;
    if (!view || row < 1) return;
    if (view.all) {
      // Every row is here already: a row past them is past the table's end.
      pendingRow.current = row - 1;
      showPendingRow();
      return;
    }
    const page = Math.floor((row - 1) / view.pageSize);
    pendingRow.current = (row - 1) % view.pageSize;
    if (page === view.page) showPendingRow();
    else changeView({ page });
  }
  /** Selects the row a jump asked for once its page is on screen. */
  function showPendingRow() {
    const index = pendingRow.current;
    const result = useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results[0];
    if (index === null || !result || !ended(result)) return;
    pendingRow.current = null;
    const row = Math.min(index, Math.max(0, result.rows - 1));
    if (index >= result.rows) setNotice({ text: t("goToRow.past", { rows: numbers.format((tab.table?.page ?? 0) * (tab.table?.pageSize ?? 0) + result.rows), row: numbers.format((tab.table?.page ?? 0) * (tab.table?.pageSize ?? 0) + index + 1) }), error: false });
    if (result.rows === 0) return;
    updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === result.id ? { ...item, goToRow: { row, seq: Date.now() } } : item) }));
  }
  const firstResult = tab.run?.results[0];
  useEffect(() => {
    if (tab.table && !running && ended(firstResult)) showPendingRow();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [firstResult?.id, ended(firstResult), running]);

  /** A load of many keyset pages of a paged query ("Load every row", or a page or a row past
   *  the ones loaded; SDB-386), and its job for Cancel (SDB-369). */
  const rowLoad = useRef<{ job: Job; result: number } | null>(null);

  /** Loads a paged query's rows until the result holds `until` rows (every row when `null`,
   *  after asking), by keyset pages in order (ADR-054). Resolves to the rows held. */
  async function loadQueryRowsUntil(resultId: number, until: number | null): Promise<number | null> {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const result = current?.run?.results.find((item) => item.id === resultId);
    const paging = result?.paging;
    if (!result || !ended(result) || !paging || paging.loading || isRunning(current) || queryPageBusy.current ||
        saving.now() || pendingCount > 0 || result.sort?.length || result.filter || result.columnFilters?.length) return null;
    if (!paging.hasMore || (until !== null && result.rows >= until)) return result.rows;
    if (until === null && !(await confirmLoadAll(resultId, { kind: "query", sql: paging.sql }))) return null;
    if (queryPageBusy.current) return null;
    queryPageBusy.current = true;
    const job = startJob();
    rowLoad.current = { job, result: resultId };
    const setPaging = (patch: Partial<NonNullable<ResultMeta["paging"]>>, rows?: number) =>
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
        ? { ...item, ...(rows === undefined ? {} : { rows }), paging: { ...item.paging, ...patch } } : item) }));
    setPaging({ loading: true, progress: result.rows });
    const channel = new Channel<number>();
    channel.onmessage = (rows) => { if (rowLoad.current?.job === job) setPaging({ progress: rows }); };
    try {
      const loaded = await call(commands.loadQueryRows(tab.id, job.channel, resultId, paging.sql, result.rows, paging.sort ?? [], until, channel));
      const outcome = loaded.outcome;
      setPaging({ hasMore: outcome.kind !== "complete" }, loaded.rows);
      if (outcome.kind === "cancelled") setNotice({ text: t("pages.loadCancelled", { rows: numbers.format(loaded.rows) }), error: false });
      else if (outcome.kind === "failed") setNotice({ text: t("pages.loadFailed", { detail: outcome.message }), error: true });
      return loaded.rows;
    } catch (e) {
      setNotice({ text: t("pages.loadFailed", { detail: message(e) }), error: true });
      return null;
    } finally {
      rowLoad.current = null;
      queryPageBusy.current = false;
      setPaging({ loading: false, progress: undefined });
    }
  }

  /** Stops a load of query rows: the page being read and every page after it. */
  function cancelRowLoad() {
    rowLoad.current?.job.cancel();
  }

  /** "Go to row" past the rows of a paged query that are loaded: they are loaded up to it. */
  async function goToQueryRow(resultId: number, row: number) {
    const rows = await loadQueryRowsUntil(resultId, row);
    if (rows === null) return;
    if (row > rows) setNotice({ text: t("goToRow.past", { rows: numbers.format(rows), row: numbers.format(row) }), error: false });
    if (rows === 0) return;
    updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId ? { ...item, goToRow: { row: Math.min(row, rows) - 1, seq: Date.now() } } : item) }));
  }

  /** Fetch the next block of the original SELECT without replacing the rows already shown. */
  async function loadMoreQueryRows(resultId: number): Promise<number> {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const result = current?.run?.results.find((item) => item.id === resultId);
    const paging = result?.paging;
    if (!result || !ended(result) || !paging || !paging.hasMore || paging.loading || isRunning(current) ||
        queryPageBusy.current || saving.now() || pendingCount > 0 || result.sort?.length || result.filter || result.columnFilters?.length ||
        (paging.total !== undefined && result.rows >= paging.total)) return 0;
    queryPageBusy.current = true;
    updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
      ? { ...item, paging: { ...item.paging, loading: true } } : item) }));
    try {
      const added = await call(commands.appendQueryPage(tab.id, resultId, paging.sql, paging.pageSize, result.rows, paging.sort ?? []));
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
        ? { ...item, rows: item.rows + added, paging: { ...item.paging, hasMore: added === paging.pageSize } } : item) }));
      return added;
    } catch (e) {
      setNotice({ text: message(e), error: true });
      return 0;
    } finally {
      queryPageBusy.current = false;
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
        ? { ...item, paging: { ...item.paging, loading: false } } : item) }));
    }
  }

  async function countQueryResult(resultId: number) {
    const result = useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results.find((item) => item.id === resultId);
    if (!result || !ended(result) || !result.paging || running || queryPageBusy.current || saving.now() || pendingCount > 0 || result.sort?.length || result.filter || result.columnFilters?.length) return;
    queryPageBusy.current = true;
    updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
      ? { ...item, paging: { ...item.paging, loading: true } } : item) }));
    try {
      const total = await call(commands.countQueryRows(tab.id, result.paging.sql));
      if (total === null) throw new Error(t("queryPaging.countFailed"));
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
        ? { ...item, paging: { ...item.paging, total } } : item) }));
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      queryPageBusy.current = false;
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
        ? { ...item, paging: { ...item.paging, loading: false } } : item) }));
    }
  }

  function changeQueryPageSize(size: number) {
    if (queryPageBusy.current || pendingCount > 0 || saving.now() || !activeResult) return;
    setQueryPageSize(size);
    // The grid's sort of the pages stays: the new pages are read in the same order.
    rerun(activeResult.id, false, size);
  }

  useEffect(() => {
    const queued = queuedPage.current;
    if (running || !queued) return;
    queuedPage.current = null;
    void loadPage(queued);
    // Runs when a load ends.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running]);

  /** The first page of a table data tab: with the table's default filter (SDB-381) when the
   *  tab opens without one of its own. The core checked that default again as it read it; one
   *  it refused is not applied (the filter box says so), and the page checks it once more. */
  async function openTablePage(view: TableView) {
    let first = view;
    if (!view.filter && tab.connectionId) {
      const saved = await call(commands.tableFilters(tabConnectionOf(tab.connectionId, tab.connectionFolder), tab.database ?? null, view.schema, view.name)).catch(() => null);
      if (saved?.default) {
        first = { ...view, filter: saved.default };
        updateTab(tab.id, { table: first });
      }
    }
    void loadPage(first);
  }

  // A table data tab shows its first page as soon as it opens.
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current || !tab.table || tab.run || tabStatus === "connecting") return;
    opened.current = true;
    void openTablePage(tab.table);
    // Wait for an explorer-created tab's independent session before loading its first page.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabStatus, tab.table, tab.run]);

  /** Saves the SQL as a named query in the local store (Ctrl+S). */
  async function saveQuery() {
    if (tab.table) return;
    const name = await useDialogs.getState().askText({
      title: t("snippets.saveTitle"),
      label: t("snippets.name"),
      initial: tab.snippetName ?? "",
      confirm: t("snippets.save"),
    });
    if (!name) return;
    try {
      await call(commands.saveSnippet(name, tab.sql, tabConnection(tab)));
      updateTab(tab.id, { snippetName: name });
      useApp.getState().refreshSnippets();
      setNotice({ text: t("snippets.saved", { name }), error: false });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }
  const save = useRef(saveQuery);
  save.current = saveQuery;
  useEffect(() => {
    const onSave = (e: Event) => {
      if ((e as CustomEvent<string>).detail === tab.id) void save.current();
    };
    window.addEventListener(SAVE_QUERY_EVENT, onSave);
    return () => window.removeEventListener(SAVE_QUERY_EVENT, onSave);
  }, [tab.id]);

  // The command palette's editor commands (SDB-18): what the action column's buttons do, and
  // nothing while those buttons are disabled.
  const actionState = useRef({ running: false, runBlocked: false });
  const resultCommand = useRef<(command: ResultTabCommand) => void>(() => undefined);
  /** The tab's session commands, as the latest render wrote them. */
  const sessionCommand = useRef({ reconnect: reconnectNow, readOnly: () => setSessionReadOnly(!sessionReadOnly(tab)), cancel: cancelRun });
  sessionCommand.current = { reconnect: reconnectNow, readOnly: () => setSessionReadOnly(!sessionReadOnly(tab)), cancel: cancelRun };
  useEffect(() => {
    const onCommand = (e: Event) => {
      const { tab: target, command } = (e as CustomEvent<{ tab: string; command: EditorCommand }>).detail;
      if (target !== tab.id) return;
      const { running } = actionState.current;
      switch (command) {
        // A Run pressed while the tab cannot start one waits or says why (SDB-525): the
        // editor's run reaches `pressRun`, never dropped here.
        case "runCurrent":
          if (!running) editor.current?.runCurrent();
          break;
        case "runScript":
          if (!running) editor.current?.runScript();
          break;
        case "runFromCursor":
          if (!running) editor.current?.action(command);
          break;
        case "explain":
          if (!running) void explainAction.current();
          break;
        case "variables":
          setVariablesOpen(true);
          break;
        case "reconnect":
          if (!running) void sessionCommand.current.reconnect();
          break;
        case "sessionReadOnly":
          if (!running) void sessionCommand.current.readOnly();
          break;
        case "format":
          editor.current?.format();
          break;
        case "runNewTab":
          if (!running) editor.current?.action(command);
          break;
        case "resultRerun":
        case "resultPin":
        case "resultRename":
        case "resultResetName":
        case "resultClose":
          resultCommand.current(command);
          break;
        case "lineComment":
        case "blockComment":
        case "wordWrap":
        case "fold":
        case "unfold":
        case "matchingBracket":
        case "upperCase":
        case "lowerCase":
        case "quotedList":
        case "selectStatement":
        case "copyStatement":
        case "selectOccurrences":
        case "openObject":
        case "revealInExplorer":
        case "nextStatement":
        case "previousStatement":
        case "outline":
        case "insertSelect":
        case "insertInsert":
        case "insertUpdate":
        case "zoomIn":
        case "zoomOut":
        case "zoomReset":
          editor.current?.action(command);
          break;
        case "cut":
        case "copy":
        case "paste":
        case "selectAll":
          editor.current?.clipboard(command);
          break;
        case "cancel":
          void sessionCommand.current.cancel();
          break;
      }
    };
    window.addEventListener(EDITOR_COMMAND_EVENT, onCommand);
    return () => window.removeEventListener(EDITOR_COMMAND_EVENT, onCommand);
  }, [tab.id]);

  // Read the server state after connecting and after each script, including failed scripts:
  // a USE earlier in a script can succeed even when a later statement fails.
  useEffect(() => {
    if (!supportsDatabase || tabStatus !== "connected") {
      setDatabase(null);
      setDatabases([]);
      setDatabaseError(false);
      databaseBusyRef.current = false;
      setDatabaseBusy(false);
      return;
    }
    if (running) return;
    let cancelled = false;
    databaseBusyRef.current = true;
    setDatabaseBusy(true);
    Promise.all([call(commands.listDatabases(tab.id)), call(commands.currentDatabase(tab.id))])
      .then(([names, current]) => {
        if (cancelled) return;
        setDatabases(names);
        setDatabase(current);
        setDatabaseError(false);
      })
      .catch((e: unknown) => {
        if (!cancelled) {
          setDatabase(null);
          setDatabases([]);
          setDatabaseError(true);
          setNotice({ text: message(e), error: true });
        }
      })
      .finally(() => {
        if (!cancelled) {
          databaseBusyRef.current = false;
          setDatabaseBusy(false);
        }
      });
    return () => { cancelled = true; };
  }, [supportsDatabase, tab.id, tabStatus, tab.connectionId, tab.connectionFolder, running, databaseRefresh]);

  // The session's schema, read after connecting and after each script (a SET search_path in
  // it changes it), as the database is.
  useEffect(() => {
    if (!supportsSchema || tabStatus !== "connected") {
      setSchema(null);
      setSchemas([]);
      return;
    }
    if (running) return;
    let cancelled = false;
    Promise.all([call(commands.listSchemas(tab.id, null, false)), call(commands.activeSchema(tab.id))])
      .then(([list, current]) => {
        if (cancelled) return;
        setSchemas(list.filter((item) => !item.system || item.name === current).map((item) => item.name));
        setSchema(current);
      })
      .catch(() => {
        if (!cancelled) {
          setSchemas([]);
          setSchema(null);
        }
      });
    return () => { cancelled = true; };
  }, [supportsSchema, tab.id, tabStatus, tab.connectionId, tab.connectionFolder, running, schemaRefresh]);

  async function changeSchema(next: string) {
    if (!next || schemaBusy) return;
    setSchemaBusy(true);
    try {
      await call(commands.setActiveSchema(tab.id, next));
      setSchema(next);
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      setSchemaBusy(false);
      setSchemaRefresh((n) => n + 1);
    }
  }

  // Which databases a PostgreSQL tab can be on is the core's answer, read once it connects.
  useEffect(() => {
    if (tab.table || tabStatus !== "connected" || connection?.engine !== "postgres") {
      setTabDatabases(null);
      return;
    }
    let cancelled = false;
    void call(commands.connectionDatabases(tab.id))
      .then((listed) => { if (!cancelled) setTabDatabases(listed.kind === "several" ? listed.databases : null); })
      .catch(() => { if (!cancelled) setTabDatabases(null); });
    return () => { cancelled = true; };
  }, [tab.id, tab.table, tabStatus, connection?.engine, tab.database]);

  /** Another database for a PostgreSQL tab (ADR-074 point 4): a disconnect and a connect, never
   *  a switch. Refused while a script runs (the core refuses too), asked about like any disconnect
   *  while a transaction is open (ADR-031); nothing is run again, and the rows on screen, read by
   *  the old session, stop being editable (ADR-061). The old session stays if the new one fails. */
  async function changeTabDatabase(next: string) {
    const own = tabDatabases?.find((d) => d.own)?.name ?? null;
    if (!next || next === (tab.database ?? own) || running || tabDatabaseBusy) return;
    if (pendingCount > 0 || saving.now() || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (!(await confirmOpenTransactions([tab.id], "disconnect"))) return;
    // The old session stays if the new one fails: the tab is still connected to it.
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const before = current ? serverVersion(current) : null;
    setTabDatabaseBusy(true);
    setConnection(tab.id, CONNECTING);
    try {
      const version = await call(commands.changeTabDatabase(tab.id, next));
      updateTab(tab.id, { database: next === own ? null : next });
      setConnection(tab.id, connectedWith({ version }));
      updateRun(tab.id, (r) => ({ ...r, results: r.results.map((result) => ({ ...result, paging: undefined })) }));
      setQueryTarget(null);
    } catch (e) {
      setConnection(tab.id, before !== null ? connectedWith({ version: before }) : DISCONNECTED);
      setNotice({ text: t("databaseChangeFailed", { database: next, detail: message(e) }), error: true });
    } finally {
      setTabDatabaseBusy(false);
    }
  }

  async function changeDatabase(next: string) {
    if (pendingCount > 0 || saving.now() || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (!next || databaseBusyRef.current) return;
    databaseBusyRef.current = true;
    setDatabaseBusy(true);
    try {
      await call(commands.selectDatabase(tab.id, next));
      setDatabase(next);
      updateRun(tab.id, (r) => ({ ...r, results: r.results.map((result) => ({ ...result, paging: undefined })) }));
      // The rows on screen were read in the other database: read-only until run again
      // (SDB-137; the core refuses their edits too).
      setQueryTarget(null);
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      databaseBusyRef.current = false;
      setDatabaseBusy(false);
    }
  }

  // "Set as the editor's schema" in the explorer, for this tab: through the database list where
  // it has one, so the list shows it; otherwise the core sets the session's schema.
  const [schemaRequest, setSchemaRequest] = useState<{ schema: string; n: number } | null>(null);
  useEffect(() => {
    const onRequest = (e: Event) => {
      const { tabId, schema } = (e as CustomEvent<{ tabId: string; schema: string }>).detail;
      if (tabId === tab.id) setSchemaRequest((request) => ({ schema, n: (request?.n ?? 0) + 1 }));
    };
    window.addEventListener(SET_ACTIVE_SCHEMA_EVENT, onRequest);
    return () => window.removeEventListener(SET_ACTIVE_SCHEMA_EVENT, onRequest);
  }, [tab.id]);
  useEffect(() => {
    if (!schemaRequest) return;
    if (supportsDatabase) {
      void changeDatabase(schemaRequest.schema);
      return;
    }
    call(commands.setActiveSchema(tab.id, schemaRequest.schema))
      .then(() => setNotice({ text: t("activeSchemaSet", { schema: schemaRequest.schema }), error: false }))
      .catch((e: unknown) => setNotice({ text: message(e), error: true }))
      .finally(() => setSchemaRefresh((n) => n + 1));
    // Once per request.
  }, [schemaRequest]);

  /** What the tab shows when a run could not start: the error, and the results it had. */
  function failedRun(text: string, translation?: Translation): RunState {
    return errorRun(text, translation, useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results);
  }

  async function ensureConnected(): Promise<boolean> {
    // The store, not this render's `tab`: a caller may have just connected (loading a
    // table page connects, then runs its SELECT through `run`).
    const current = useApp.getState().tabs.find((t) => t.id === tab.id);
    // A lost session is reconnected only from its banner, and nothing is run then (ADR-028).
    const lost = current ? lostSession(current) : null;
    if (lost) {
      updateTab(tab.id, {
        run: lost.idleSeconds !== null
          ? failedRun(lost.reason, { key: "messages.idleClosed", values: { minutes: idleMinutes(lost.idleSeconds) } })
          : failedRun(lost.reason, { key: "messages.connectionLost", values: { detail: lost.reason } }),
      });
      return false;
    }
    const status = current ? statusName(current) : "disconnected";
    if (status === "connected") return true;
    if (status === "connecting") return false;
    if (!tab.connectionId) {
      updateTab(tab.id, { run: failedRun("", { key: "pickConnection" }) });
      return false;
    }
    setConnection(tab.id, CONNECTING);
    try {
      // On the database the tab is on (ADR-074, SDB-505), never quietly another.
      const session = await openTabSession(tab.id, tabConnectionOf(tab.connectionId, tab.connectionFolder));
      if (session === null) {
        setConnection(tab.id, DISCONNECTED);
        return false;
      }
      setConnection(tab.id, connectedWith(session));
      return true;
    } catch (e) {
      const detail = message(e);
      updateTab(tab.id, { connectionState: connectFailed(detail), run: failedRun(detail, { key: "messages.connectionFailed", values: { detail } }) });
      return false;
    }
  }

  /** "Reconnect" on request (SDB-395), for a session that still looks alive: the session closes
   *  (asking first if a transaction is open, ADR-031) and a new one opens, as the connection
   *  says. Nothing is run again (ADR-028), and "Read-only for this session" ends with it. */
  async function reconnectNow() {
    if (!tab.connectionId) return;
    if (pendingCount > 0 || saving.now() || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (!(await confirmOpenTransactions([tab.id], "disconnect"))) return;
    await call(commands.disconnect(tab.id)).catch(() => undefined);
    setConnection(tab.id, DISCONNECTED);
    if (await ensureConnected()) setNotice({ text: t("reconnected"), error: false });
  }

  /** "Read-only for this session" (SDB-395): the core puts the session behind a read-only
   *  connection's barrier (ADR-053), or takes it away; it refuses with a transaction open, and
   *  never loosens a read-only connection. */
  async function setSessionReadOnly(on: boolean) {
    if (on && !(await ensureConnected())) return;
    try {
      await call(commands.setSessionReadOnly(tab.id, on));
      useApp.getState().setSessionReadOnly(tab.id, on);
      setNotice({ text: t(on ? "session.readOnlyOn" : "session.readOnlyOff"), error: false });
    } catch (e) {
      setNotice({ text: t("session.readOnlyFailed", { detail: message(e) }), error: true });
    }
  }

  /** Opens a new session for a tab whose session was lost. Nothing is run: the user runs
   *  the statement again. */
  async function reconnect() {
    const current = tabNow();
    if (current && lostSession(current)) setConnection(tab.id, DISCONNECTED);
    if (await ensureConnected()) setNotice({ text: t("reconnected"), error: false });
  }

  /** `value`: the picked connection's tag, or "" for none. */
  async function changeConnection(value: string) {
    if (pendingCount > 0 || saving.now() || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (!(await confirmOpenTransactions([tab.id], "disconnect"))) return;
    await call(commands.disconnect(tab.id)).catch(() => undefined);
    setPlan(null); setPlanOpen(false); setCapabilities(null); setAnalyzePlan(false);
    updateRun(tab.id, (r) => ({ ...r, results: r.results.map((result) => ({ ...result, paging: undefined })) }));
    // The rows on screen came from the other connection: read-only until run again (SDB-137).
    setQueryTarget(null);
    // Another connection starts on its own database (ADR-074, SDB-505).
    const picked = connectionOfTag(value);
    updateTab(tab.id, { connectionId: picked?.id ?? null, connectionFolder: picked?.folder ?? null, database: null, connectionState: DISCONNECTED });
  }

  // "Run…" on a routine (ADR-047): the whole script once, as soon as the tab is connected and
  // idle (its database list read), like Run script: the parameters are asked, the guard
  // confirms. The request is cleared first, so nothing runs again after a reconnect.
  const runOnOpenReady = !!tab.runOnOpen && tabStatus === "connected" && !running
    && !databaseBusy && !historySaving && !workspaceChanging;
  useEffect(() => {
    // The database list may have started loading in this same commit: wait for it.
    if (!runOnOpenReady || !editor.current || databaseBusyRef.current) return;
    useApp.getState().updateTab(tab.id, { runOnOpen: undefined });
    editor.current.runScript();
  }, [tab.id, runOnOpenReady]);

  // A waiting run (SDB-525) runs once the tab can run, or is dropped with a word. Read live:
  // the database list may have started loading in this same commit.
  useEffect(() => {
    if (waitingRef.current.kind === "none") return;
    const outcome = settle(waitingRef.current, blockNow(), sessionNow());
    switch (outcome.kind) {
      case "keep":
        if (outcome.waiting !== waitingRef.current) setWaitingRun(outcome.waiting);
        return;
      case "start":
        setWaitingRun(NOT_WAITING);
        startPressed(outcome.run);
        return;
      case "drop":
        setWaitingRun(NOT_WAITING);
        setNotice({ text: t(notRunKey(outcome.because, true)), error: true });
        return;
      default: {
        const unknown: never = outcome;
        return unknown;
      }
    }
  }, [waitingRun, tabStatus, databaseBusy, historySaving, workspaceChanging, refreshRestoring]);

  /** What keeps the tab from starting a run right now, read live: a press and the effect above
   *  may come in the same commit as what changed. */
  function blockNow(): RunBlock {
    const app = useApp.getState();
    const own = tabConnection(tab);
    const current = app.tabs.find((item) => item.id === tab.id);
    return runBlock({
      folder: app.workspaceChanging,
      connecting: !!current && isConnecting(current),
      databases: databaseBusyRef.current,
      history: !!own && app.historySaving.includes(connectionTag(own)),
      restoring: restoringView.current,
    });
  }

  function sessionNow(): SessionNow {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const status = current ? statusName(current) : "disconnected";
    return status === "disconnected" ? "none" : status;
  }

  /** A Run the user pressed (the editor's Run, Run script, Run from cursor, Run in a new tab, and
   *  Explain): started now, waiting for what blocks the tab and ends on its own, or refused with
   *  the reason; never dropped without a word (SDB-525). */
  function pressRun(pressed: PressedRun) {
    // A press already starting a run (connecting for it, or asking its parameters) covers this
    // one: a second press never makes a second run.
    if (startingRun.current) return;
    const outcome = press(blockNow(), waitingRef.current, pressed, sessionNow());
    switch (outcome.kind) {
      case "start":
        startPressed(pressed);
        return;
      case "wait":
        setNotice(null);
        setWaitingRun(outcome.waiting);
        return;
      case "alreadyWaiting":
        // One run waits, and the toolbar says so.
        return;
      case "refused":
        setNotice({ text: t(notRunKey(outcome.because, false)), error: true });
        return;
      default: {
        const unknown: never = outcome;
        return unknown;
      }
    }
  }

  function startPressed(pressed: PressedRun) {
    switch (pressed.kind) {
      case "run":
        void run(pressed.request, false, queryPageSize, [], pressed.placement);
        return;
      case "explain":
        void explainCurrent(pressed.request);
        return;
      default: {
        const unknown: never = pressed;
        return unknown;
      }
    }
  }

  /** Explain pressed: started now, or waiting with the statement as it is now (SDB-525). */
  async function explainPressed() {
    if (blockNow().kind === "free") return explainCurrent();
    const request = await editor.current?.currentRequest();
    if (request?.text.trim()) pressRun({ kind: "explain", request });
  }

  /** Runs the query of one result again, into that result's tab (SDB-378): a new run the
   *  user asked for, through the parameters, the guard and the confirmations like any other
   *  (ADR-028). The core keeps the old rows until the new ones start. A query read in the
   *  grid's sort is read again in it. */
  function rerun(resultId: number, refreshingEdits = false, pageSize = queryPageSize, sort?: SortState[]) {
    const result = useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results.find((item) => item.id === resultId);
    // Its own statement, as it ran (a page's query as the user wrote it): never the rest of
    // the script it came from.
    const text = result?.paging?.sql ?? result?.sql;
    if (!result || !text) return;
    void run({ text, from: 0 }, refreshingEdits, pageSize, sort ?? result.paging?.sort ?? [], { kind: "into", result: resultId }, result.refreshSql);
  }

  /** Runs again the query of a result the core no longer keeps (SDB-367): its tab, which
   *  nothing backs any more, gives way to the new result. */
  function runFreedAgain(resultId: number) {
    const result = useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results.find((item) => item.id === resultId);
    const text = result?.paging?.sql ?? result?.sql;
    if (!result || !text) return;
    updateRun(tab.id, (r) => withoutResults(r, [resultId]));
    void run({ text, from: 0 }, false, queryPageSize, result.paging?.sort ?? [], { kind: "newTab" }, result.refreshSql);
  }

  /** Refresh a single-statement result through Run, then restore its local view. */
  async function refreshQuery(resultId: number, row: number) {
    const before = useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results.find((item) => item.id === resultId);
    const text = before?.refreshSql;
    if (!before?.refreshable || !text) return;
    const previousRun = runSeq.current;
    setRefreshRestoring(true);
    try {
    await run({ text, from: 0 }, false, before.paging?.pageSize ?? queryPageSize, before.paging?.sort ?? [], { kind: "into", result: resultId });
    if (runSeq.current !== previousRun + 1) return;
    const find = () => useApp.getState().tabs.find((item) => item.id === tab.id)?.run?.results.find((item) => item.run === previousRun + 1);
    let result = find();
    if (!result || !ended(result) || !result.refreshable) return;
    if (!sameResultColumns(before.columns, result.columns)) {
      setNotice({ text: t("results.refreshColumnsChanged"), error: true });
      return;
    }
    restoringView.current = true;
      // A keyset page is reached by reading its predecessors, never by guessing an offset.
      const loaded = before.total ?? before.rows;
      while (result.paging?.hasMore && result.rows < loaded) {
        const added = await loadMoreQueryRows(result.id);
        if (!added || runSeq.current !== previousRun + 1) break;
        result = find();
        if (!result) return;
      }
      if (before.sort?.length) await call(commands.sortResult(result.id, before.sort));
      let rows = result.rows;
      let total: number | undefined;
      if (before.filter) {
        const outcome = await call(commands.filterResult(result.id, before.filter));
        rows = outcome.rows; total = outcome.total;
      }
      if (before.columnFilters?.length) {
        // Read as they were: Match any and Match case too (SDB-381).
        const outcome = await call(commands.filterColumns(result.id, before.columnFilters, before.filterMode ?? null));
        rows = outcome.rows; total = outcome.total;
      }
      updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === result?.id
        ? { ...item, sort: before.sort, filter: before.filter, columnFilters: before.columnFilters, filterMode: before.columnFilters?.length ? before.filterMode : undefined, rows, total, refreshScrollRow: Math.min(row, Math.max(0, rows - 1)) }
        : item) }));
    } catch (e) { setNotice({ text: message(e), error: true }); }
    finally { restoringView.current = false; setRefreshRestoring(false); }
  }

  /** `sort`: the grid's sort a paged query is read with (its header clicks); none for the
   *  query's own order, as every run from the editor. `placement`: where the core puts the
   *  run's results (SDB-378). */
  async function run(request: RunRequest, refreshingEdits = false, pageSize = queryPageSize, sort: SortState[] = [], placement: ResultPlacementItem = { kind: "replace" }, refreshSource = request.text) {
    if (!refreshingEdits && (pendingCount > 0 || saving.now())) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (queryPageBusy.current || startingRun.current || restoringView.current) return;
    if (!request.text.trim() || running || databaseBusyRef.current || useApp.getState().workspaceChanging ||
        (tabConnection(tab) && useApp.getState().historySaving.includes(connectionTag(tabConnection(tab)!)))) return;
    startingRun.current = true;
    try {
    if (!(await ensureConnected())) return;
    setPlan(null); setPlanOpen(false);
    // `${name}` variables first, written in as they are, then `:name` placeholders, written
    // in as literals: the core does both, and everything after reads the text that runs.
    let text: string;
    let echoes: ScriptEchoItem[];
    try {
      const written = await withVariables(request.text);
      if (written === null) return;
      const bound = await withParams(written.text);
      if (bound === null) return;
      text = bound;
      echoes = written.echoes;
    } catch (e) {
      const detail = message(e);
      updateTab(tab.id, { run: failedRun(detail, { key: "messages.runFailed", values: { detail } }) });
      return;
    }
    // Positions in an error refer to the text that ran; with values written in, they no
    // longer match the editor, so nothing is underlined.
    // A re-run's text is a result's, not the editor's: nothing is underlined either.
    const sent: RunRequest & { bound: boolean } = { text, from: request.from, bound: text !== request.text || placement.kind === "into" };
    // The core says which statements need a confirmation; without it, it refuses the run.
    const risky = await call(commands.scriptRisks(tab.id, sent.text)).catch(() => []);
    let confirmed = false;
    if (risky.length > 0) {
      const statements = risky.map((r) => ({
        sql: sent.text.slice(byteToIndex(sent.text, r.start), byteToIndex(sent.text, r.end)),
        risk: r.risk,
      }));
      if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return;
      confirmed = true;
    }
    // The results stay until the core says which ones the run replaced (its first event): it
    // frees every one that is not pinned, none for a run into a new tab (SDB-378).
    const started = startRun(sent.text, refreshSource);
    const channel = new Channel<ScriptEvent>();
    let finishEvent: (() => void) | undefined;
    const finished = new Promise<void>((resolve) => { finishEvent = resolve; });
    // The last error, for the question a paused run asks (ADR-035).
    const failures = new Map<number, string>();
    let queryPage: { sql: string; pageSize: number; sort?: SortState[] } | null = null;
    channel.onmessage = (event) => {
      if (event.kind === "runStarted" && event.pageSort != null) {
        queryPage = { sql: sent.text, pageSize, ...(event.pageSort.length ? { sort: event.pageSort } : {}) };
      }
      handle(event, sent, started.startedAt, queryPage, failures, started.run);
      if (event.kind === "scriptFinished") finishEvent?.();
    };
    // The script's `@echo` lines, shown when the run reaches each (ADR-078).
    const options = { ...runOptionsOf(tab), echoes };
    try {
      if (tab.table) await call(commands.runScript(tab.id, sent.text, confirmed, options, placement, channel));
      else await call(commands.runQueryPage(tab.id, sent.text, pageSize, sort, confirmed, options, placement, channel));
      // The command may resolve before the channel's final event updates the result metadata.
      // Bound the wait if the channel closes without delivering it.
      await Promise.race([finished, new Promise<void>((resolve) => setTimeout(resolve, 5000))]);
    } catch (e) {
      // Refused before it started (the guard, unsaved edits in a result it would replace):
      // nothing was replaced.
      const detail = message(e);
      const translation = { key: "messages.runFailed", values: { detail } };
      updateRun(tab.id, (r) => ({ ...r, running: false, elapsedMs: performance.now() - started.startedAt, activeResult: null, messages: [{ kind: "error", text: detail, translation }], error: { message: detail, offset: null, translation } }));
    }
    } finally {
      startingRun.current = false;
    }
  }

  /** Marks the tab as running a new run, keeping the result tabs it has, and numbers it. */
  function startRun(text: string, source = text, canRefresh = true): { run: number; startedAt: number } {
    const run = ++runSeq.current;
    const kept = useApp.getState().tabs.find((item) => item.id === tab.id)?.run;
    // Runs whose results are all gone are forgotten.
    for (const known of [...runTexts.current.keys()]) {
      if (!kept?.results.some((result) => result.run === known)) runTexts.current.delete(known);
    }
    runTexts.current.set(run, { text, source, results: 0, canRefresh });
    const startedAt = performance.now();
    updateTab(tab.id, {
      run: { running: true, results: kept?.results ?? [], activeResult: kept?.activeResult ?? null, messages: [], error: null, startedAt, elapsedMs: null },
    });
    return { run, startedAt };
  }

  async function explainCurrent(given?: RunRequest) {
    if (startingRun.current || queryPageBusy.current || actionState.current.running || blockNow().kind !== "free") return;
    if (pendingCount > 0 || saving.now()) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    startingRun.current = true;
    let runStarted = false;
    try {
      if (!(await ensureConnected())) return;
      const support = await call(commands.planCapabilities(tab.id));
      setCapabilities(support); setPlanOpen(true);
      if (!support.analyze) setAnalyzePlan(false);
      if (!support.estimated) { setNotice({ text: t("plan.unsupported", { engine: connection?.engine }), error: true }); return; }
      // A waiting Explain explains the statement as it was when pressed (SDB-525).
      const request = given ?? await editor.current?.currentRequest();
      if (!request?.text.trim()) return;
      const written = await withVariables(request.text);
      if (written === null) return;
      const text = await withParams(written.text);
      if (text === null) return;
      const analyze = analyzePlan && support.analyze;
      let options = { analyze, buffers: analyze && planBuffers && support.buffers, analyzeConfirmed: false, confirmed: false, timeoutSeconds: runOptionsOf(tab).timeoutSeconds };
      const check = await call(commands.explainPreflight(tab.id, text, options));
      if (check.executesQuery) {
        const answer = await useDialogs.getState().askChoice({ title: t("plan.analyzeTitle"), message: t("plan.analyzeWarning"), choices: [{ id: "execute", label: t("plan.analyzeExecute"), danger: true }, { id: "cancel", label: t("cancel") }] });
        if (answer !== "execute") return;
        options = { ...options, analyzeConfirmed: true };
      }
      if (check.risks.length) {
        if (!(await useDialogs.getState().askRisks(connection?.name ?? "", check.risks.map(item => ({ sql: text, risk: item.risk }))))) return;
        options = { ...options, confirmed: true };
      }
      // Acceptance replaces the results just like an ordinary Run: the core frees the ones
      // that are not pinned and says which. Refusals and dismissed warnings above leave them.
      setPlan(null);
      runStarted = true;
      const started = startRun(text, text, false);
      const channel = new Channel<ScriptEvent>(), failures = new Map<number, string>();
      // PostgreSQL diagnostics refer to the generated EXPLAIN, so never underline source offsets.
      channel.onmessage = event => handle(event, { ...request, text, bound: true }, started.startedAt, null, failures, started.run);
      const result = await call(commands.explainQuery(tab.id, text, options, channel));
      if (useApp.getState().tabs.some(item => item.id === tab.id)) setPlan(result);
      else await commands.releasePlan(result.id);
    } catch (e) {
      const detail = message(e);
      if (runStarted) updateRun(tab.id, current => ({ ...current, running: false, elapsedMs: performance.now() - current.startedAt, error: { message: detail, offset: null, translation: { key: "messages.runFailed", values: { detail } } } }));
      setNotice({ text: t("plan.runFailed", { reason: message(e) }), error: true });
    } finally { startingRun.current = false; }
  }
  explainAction.current = explainPressed;

  function handle(event: ScriptEvent, request: RunRequest & { bound: boolean }, startedAt: number, page: { sql: string; pageSize: number; sort?: SortState[] } | null, failures: Map<number, string>, runId: number) {
    switch (event.kind) {
      case "runStarted":
        // What the core freed as the run started: every result that was not pinned, or none.
        setQueryTarget((target) => (target && event.closed.includes(target.result) ? null : target));
        updateRun(tab.id, (r) => withoutResults(r, event.closed));
        break;
      case "statementStarted":
        if (event.index === 0) statementTexts.current = {};
        statementTexts.current[event.index] = request.text.slice(byteToIndex(request.text, event.start), byteToIndex(request.text, event.end));
        break;
      case "resultStarted": {
        const sql = page ? page.sql : statementTexts.current[event.index];
        const produced = runTexts.current.get(runId);
        if (produced) produced.results += 1;
        // Where the core put it: last, or in the place of the result it ran again; and the
        // tabs it closed to stay within the limit.
        const started: ResultMeta = { id: event.result, statement: event.index, run: runId, columns: event.columns, rows: 0, end: { kind: "loading" }, pinned: event.pinned, renamed: event.renamed, ...(event.name ? { name: event.name } : {}), ...(sql ? { sql } : {}), ...(page ? { paging: { ...page, hasMore: false, loading: false } } : {}) };
        updateRun(tab.id, (r) => withStartedResult(r, started, event.replaces ?? null, event.closed));
        break;
      }
      case "rows":
        updateRun(tab.id, (r) => ({
          ...r,
          results: r.results.map((x) => (x.id === event.result ? { ...x, rows: event.totalRows } : x)),
        }));
        break;
      case "resultEnded":
        // Whole, or cut short and why, as the core says it (SDB-367). A result cut short keeps
        // the rows that arrived (SDB-275).
        updateRun(tab.id, (r) => ({ ...r, results: endResult(r.results, event.result, event.end) }));
        break;
      case "notice":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: `${event.severity}: ${event.message}` }] }));
        break;
      case "echo":
        // A script's `@echo` line: its own text, as written (ADR-078).
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "info", text: event.text }] }));
        break;
      case "historyWarning":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: event.message, translation: { key: "messages.historySaveFailed", values: { detail: event.message } } }] }));
        break;
      case "statementFinished": {
        const timing = { duration: Math.round(event.elapsedMs ?? 0), execution: Math.round(event.executionMs ?? 0), fetch: Math.round(event.fetchMs ?? 0) };
        const translation: Translation = event.rowsAffected == null
          ? { key: "messages.statementOk", values: { index: event.index + 1, ...timing } }
          : { key: "messages.statementRows", values: { index: event.index + 1, count: event.rowsAffected, ...timing } };
        updateRun(tab.id, (r) => ({
          ...r,
          messages: [...r.messages, { kind: "info", text: "", translation }],
        }));
        break;
      }
      case "statementFailed": {
        const offset =
          event.scriptOffset == null || request.bound ? null : request.from + byteToIndex(request.text, event.scriptOffset);
        const diagnostic = { message: event.message, detail: event.detail, hint: event.hint, code: event.code, cancelled: event.end === "cancelled" };
        failures.set(event.index, event.message);
        const key = statementEndKey(event.end);
        updateRun(tab.id, (r) => ({
          ...r,
          error: { message: event.message, offset, diagnostic },
          messages: [...r.messages, { kind: "error", text: event.message, translation: { key, values: { index: event.index + 1, reason: event.message } } }],
        }));
        break;
      }
      case "errorPaused": {
        // The core waits for the answer to this ticket; the dialog only asks (ADR-035).
        const ticket = event.ticket;
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: "", translation: { key: "messages.paused", values: { index: event.index + 1 } } }] }));
        void useDialogs.getState()
          .askScriptError({ tab: tab.id, statement: event.index + 1, message: failures.get(event.index) ?? "", remaining: event.remaining })
          .then((answer) => call(commands.answerScriptError(tab.id, ticket, answer)))
          .catch(() => undefined);
        break;
      }
      case "scriptFinished": {
        const elapsedMs = performance.now() - startedAt;
        const summary = event.summary;
        // A run with no result of its own shows its messages.
        updateRun(tab.id, (r) => afterRun({
          ...r,
          results: markRefreshable(r.results, runId, summary).map((result) => result.run === runId && result.refreshable
            ? { ...result, refreshable: !!runTexts.current.get(runId)?.canRefresh, refreshSql: runTexts.current.get(runId)?.source }
            : result),
          running: false,
          elapsedMs,
          summary,
          messages: showsSummary(summary) ? [...r.messages, ...summaryMessages(summary)] : r.messages,
        }, runId));
        useApp.getState().refreshHistory();
        if (mayChangeCompletionCatalog(request.text, connection?.engine ?? null)) {
          if (tab.connectionId) schemasRefreshed(tabConnectionOf(tab.connectionId, tab.connectionFolder));
        }
        notifyFinished(tab, summary, elapsedMs);
        break;
      }
    }
  }

  const editorError = useMemo(
    () => (tab.run?.error?.offset != null ? { index: tab.run.error.offset, message: translatedError(tab.run.error, t, numbers), source: tab.run.error } : null),
    [tab.run?.error, t, numbers],
  );

  function startDrag(e: React.PointerEvent) {
    const el = body.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const move = (ev: PointerEvent) =>
      setEditorShare(Math.min(0.85, Math.max(0.12, (ev.clientY - rect.top) / rect.height)));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    e.preventDefault();
  }

  // What the core says the tab's session runs (SDB-370): paused at a statement, cancelling.
  const cancelRequested = cancelling(tab);
  const runStatusText = runStatus(runStateOf(tab));
  const lost = lostSession(tab);
  const version = serverVersion(tab);
  const runStatusLine = running ? t(runStatusText.key, runStatusText.values) : "";
  // What Run will send, in the core's words; plain "Run" until its first answer.
  const runPicks = runLabel?.kind === "selection" && runLabel.statements > 1;
  const runName = !runLabel ? t("run")
    : runLabel.kind === "statement" ? t("runStatement")
    : runLabel.kind === "script" ? t("runScript")
    : runPicks ? t("runSelectionPick") : t("runSelection");
  // The names the table tab's filter box completes from: the columns of the page on screen.
  const tableColumns = tab.table ? (tab.run?.results[0]?.columns ?? []).map((column) => column.name) : [];
  // What disables the run buttons; a Run pressed meanwhile reads the same causes live (SDB-525).
  actionState.current = { running, runBlocked: runBlock({ folder: workspaceChanging, connecting: tabStatus === "connecting", databases: databaseBusy, history: historySaving, restoring: refreshRestoring }).kind !== "free" };
  // A result tab's menu (SDB-378), on the active result. The pin and the name are the core's:
  // it answers what the tab is now, and that is what is drawn.
  resultCommand.current = (command) => {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id)?.run;
    const result = current?.results.find((item) => item.id === current.activeResult);
    if (!result || tab.table) return;
    const drawn = (item: { result: number; name: string | null; renamed: boolean; pinned: boolean }) =>
      updateRun(tab.id, (r) => ({ ...r, results: r.results.map((x) => (x.id === item.result ? { ...x, name: item.name ?? undefined, renamed: item.renamed, pinned: item.pinned } : x)) }));
    const failed = (e: unknown) => setNotice({ text: message(e), error: true });
    switch (command) {
      case "resultRerun":
        if (!isRunning(tabNow()) && ended(result)) rerun(result.id);
        break;
      case "resultPin":
        void call(commands.pinResult(tab.id, result.id, !result.pinned)).then(drawn).catch(failed);
        break;
      case "resultRename":
        void useDialogs.getState()
          .askText({ title: t("resultTabs.renameTitle"), label: t("resultTabs.name"), initial: result.name ?? "", confirm: t("resultTabs.renameConfirm") })
          .then((name) => (name ? call(commands.renameResult(tab.id, result.id, name)).then(drawn) : undefined))
          .catch(failed);
        break;
      case "resultResetName":
        // An empty name: the core gives back the comment's name, or none (SDB-407).
        void call(commands.renameResult(tab.id, result.id, "")).then(drawn).catch(failed);
        break;
      case "resultClose":
        // Closing frees the result in the core, pinned or not; never with unsaved edits.
        if (!result || !ended(result)) break;
        if (pendingCount > 0 || saving.now()) { setNotice({ text: t("edit.applyFirst"), error: true }); break; }
        void call(commands.releaseResult(result.id))
          .then(() => updateRun(tab.id, (r) => withoutResults(r, [result.id])))
          .catch(failed);
        break;
    }
  };
  // Results shown in their own window leave the editor the whole tab, never maximized.
  const maximized = !tab.table && !tab.resultsWindow && (tab.resultsMaximized ?? false);
  const kind = connection?.kind ?? null;
  const refreshDisabled = running || historySaving || databaseBusy || tabStatus !== "connected" || workspaceChanging || savingEdits || pageLoading || refreshRestoring;
  // What the results panel calls back, the same whether it is here or in a window of its own
  // (SDB-30: the results window asks, and these carry it out).
  const resultsCallbacks = {
    onCancelRun: () => void cancelRun(),
    onQuerySort: tab.table ? undefined : sortQuery,
    onRefresh: (result: number, row: number) => {
      if (pendingCount > 0 || saving.now()) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
      if (tab.table) void loadPage(tab.table);
      else void refreshQuery(result, row);
    },
    onLoadMore: loadMoreQueryRows,
    onRunAgain: runFreedAgain,
    onCountQuery: (id: number) => void countQueryResult(id),
    onQueryPageSize: changeQueryPageSize,
    onGoToRow: (result: number, row: number) => {
      if (tab.table) goToTableRow(row);
      else void goToQueryRow(result, row);
    },
    onLoadRows: (result: number, until: number | null) => loadQueryRowsUntil(result, until),
    onCancelLoad: cancelRowLoad,
    onSelect: (id: number | null) => updateRun(tab.id, (r) => ({ ...r, activeResult: id })),
    onSorted: (result: number, sort: SortState[]) =>
      updateRun(tab.id, (r) => ({ ...r, results: r.results.map((x) => (x.id === result ? { ...x, sort } : x)) })),
    onColumnFiltered: (result: number, columnFilters: ColumnFilterItem[], rows: number, total: number, filterMode?: FilterMode) =>
      updateRun(tab.id, (r) => ({
        ...r,
        results: r.results.map((x) => (x.id === result ? { ...x, columnFilters, filterMode, rows, total } : x)),
      })),
    onFiltered: (result: number, filter: string | undefined, rows: number, total: number) =>
      updateRun(tab.id, (r) => ({
        ...r,
        results: r.results.map((x) => (x.id === result ? { ...x, filter, rows, total } : x)),
      })),
  };
  const resultsWindow = tab.table ? undefined : tab.resultsWindow;
  const mirror: ResultsMirror = {
    tab: tab.id,
    name: tabLabel(tab),
    connection: ownConnection,
    run: tab.run,
    running,
    connectFailed: connectFailure(tab) !== null,
    runStatus: runStatusLine,
    cancelRequested,
    refreshDisabled,
    restoringView: refreshRestoring,
  };
  const latestResults = useRef({ callbacks: resultsCallbacks, mirror });
  latestResults.current = { callbacks: resultsCallbacks, mirror };
  // Every change of what the panel would show goes to the results window.
  useEffect(() => {
    if (resultsWindow) void sendMirror(resultsWindow, mirror).catch(() => undefined);
  }, [resultsWindow, tab.run, tab.connectionId, tab.connectionFolder, mirror.name, running, mirror.connectFailed, runStatusLine, cancelRequested, refreshDisabled, refreshRestoring]);
  // Its requests, carried out by this tab as its own panel's would be.
  useEffect(() => {
    if (!resultsWindow) return;
    return serveRequests(tab.id, (request) => {
      const { callbacks, mirror: now } = latestResults.current;
      switch (request.kind) {
        case "hello": return sendMirror(resultsWindow, now);
        case "select": return callbacks.onSelect(request.id);
        case "sorted": return callbacks.onSorted(request.result, request.sort);
        case "filtered": return callbacks.onFiltered(request.result, request.filter, request.rows, request.total);
        case "columnFiltered": return callbacks.onColumnFiltered(request.result, request.columnFilters, request.rows, request.total, request.filterMode);
        case "refresh": return callbacks.onRefresh(request.result, request.row);
        case "loadMore": return callbacks.onLoadMore(request.result);
        case "runAgain": return callbacks.onRunAgain(request.result);
        case "countQuery": return callbacks.onCountQuery(request.result);
        case "queryPageSize": return callbacks.onQueryPageSize(request.size);
        case "goToRow": return callbacks.onGoToRow(request.result, request.row);
        case "loadRows": return callbacks.onLoadRows(request.result, request.until);
        case "cancelLoad": return callbacks.onCancelLoad();
        case "cancelRun": return callbacks.onCancelRun();
        case "querySort": return callbacks.onQuerySort ? callbacks.onQuerySort(request.result, request.sort) : false;
        default: return unknownRequest(request);
      }
    });
  }, [tab.id, resultsWindow]);
  // A tab that closes (or leaves with its folder) takes its results window with it.
  const shownElsewhere = useRef<string | undefined>(undefined);
  shownElsewhere.current = resultsWindow;
  useEffect(() => () => {
    if (shownElsewhere.current) void reattachResults(tab.id).catch(() => undefined);
  }, [tab.id]);
  const detach = async () => {
    try {
      const label = await detachResults(tab.id, tabLabel(tab));
      updateTab(tab.id, { resultsWindow: label });
    } catch (e) {
      setNotice({ text: t("detach.failed", { reason: message(e) }), error: true });
    }
  };
  const planControls = <div className="plan-run-controls"><label className="check"><input type="checkbox" checked={analyzePlan} disabled={running || capabilities?.analyze === false} onChange={e => setAnalyzePlan(e.target.checked)} />ANALYZE</label><label className="check"><input type="checkbox" checked={planBuffers && capabilities?.buffers !== false} disabled={running || !analyzePlan || capabilities?.buffers === false} onChange={e => setPlanBuffers(e.target.checked)} />BUFFERS</label><button disabled={running || actionState.current.runBlocked} onClick={() => void explainPressed()}>{t("plan.explain")}</button><span className="muted small">{t(analyzePlan ? "plan.executesHelp" : "plan.estimatedHelp")}</span>{capabilities?.analyzeUnavailable && capabilities.estimated && <span className="muted small">{t(capabilities.analyzeUnavailable === "engine" ? "plan.noAnalyzeHelp" : "plan.readOnlyHelp")}</span>}<button className="ghost" aria-label={t(maximized ? "restoreResults" : "maximizeResults", { shortcut: commandShortcut("results.maximize") })} title={t(maximized ? "restoreResults" : "maximizeResults", { shortcut: commandShortcut("results.maximize") })} onClick={() => toggleResultsMaximized(tab.id)}><Icon name={maximized ? "minimize" : "maximize"} /></button>{tab.run?.messages.length ? <details className="plan-run-messages"><summary>{t("results.messages")}</summary>{tab.run.messages.map((item, index) => <p key={index}>{item.translation ? translated(item.translation, t, numbers) : item.text}</p>)}</details> : null}</div>;
  return (
    <div className="editor-tab">
      <span className="sr-only" role="status">{runStatusLine}</span>
      {variablesOpen && !tab.table && (
        <VariablesDialog tabId={tab.id} connection={tabConnection(tab)} connectionName={connection?.name ?? null} onClose={() => setVariablesOpen(false)} />
      )}
      <div className="toolbar">
        <select
          value={ownConnection ? connectionTag(ownConnection) : ""}
          onChange={(e) => void changeConnection(e.target.value)}
          aria-label={t("connection")}
          // The chosen connection's description, which an open list may not show per option.
          title={(() => {
            return connection ? connectionTooltip(connection) : t("connection");
          })()}
          disabled={running || databaseBusy || tabStatus === "connecting" || historySaving || workspaceChanging || activeResult?.paging?.loading}>
          <option value="">{t("noConnection")}</option>
          {connections.map((c) => (
            <option key={connectionTag(keyOf(c))} value={connectionTag(keyOf(c))} title={connectionTooltip(c)}>
              {shownName(c)} ({c.engine}
              {c.kind ? `, ${c.kind.name}` : c.env ? `, ${c.env}` : ""})
            </option>
          ))}
        </select>
        {supportsDatabase && (tabStatus === "connected" ? (
          <><select aria-label={t("database")} title={t("database")} value={database ?? ""} onChange={(e) => void changeDatabase(e.target.value)} disabled={running || databaseBusy || databaseError || historySaving || workspaceChanging || activeResult?.paging?.loading}>
            <option value="" disabled>{databaseBusy ? t("loadingDatabases") : databaseError ? t("databaseLoadFailed") : t("noDatabase")}</option>
            {database && !databases.includes(database) && <option value={database}>{database}</option>}
            {databases.map((name) => <option key={name} value={name}>{name}</option>)}
          </select>{databaseError && <button className="ghost" disabled={running || databaseBusy} onClick={() => setDatabaseRefresh((n) => n + 1)}>{t("retryDatabases")}</button>}</>
        ) : (
          <button className="ghost" disabled={tabStatus === "connecting" || workspaceChanging} onClick={() => void ensureConnected()}>{t("connectToChooseDatabase")}</button>
        ))}
        {tabDatabases && tabStatus === "connected" && (
          <select
            aria-label={t("database")}
            title={t("databasePostgresHint")}
            name="tab-database"
            value={tab.database ?? tabDatabases.find((d) => d.own)?.name ?? ""}
            onChange={(e) => void changeTabDatabase(e.target.value)}
            disabled={running || tabDatabaseBusy || historySaving || workspaceChanging || activeResult?.paging?.loading}
          >
            {/* A database with a connection of its own opens only there (ADR-074 point 3). */}
            {tabDatabases.filter((d) => !d.ownedBy).map((d) => (
              <option key={d.name} value={d.name}>{d.own ? t("databaseOwn", { name: d.name }) : d.name}</option>
            ))}
          </select>
        )}
        {supportsSchema && tabStatus === "connected" && (
          <select
            aria-label={t("schema")}
            title={schemaPooled ? t("schemaPooled") : t("schemaHint")}
            name="schema"
            value={schema ?? ""}
            onChange={(e) => void changeSchema(e.target.value)}
            disabled={schemaPooled || running || schemaBusy || historySaving || workspaceChanging || activeResult?.paging?.loading}
          >
            <option value="" disabled>{t("noSchema")}</option>
            {schema && !schemas.includes(schema) && <option value={schema}>{schema}</option>}
            {schemas.map((name) => <option key={name} value={name}>{name}</option>)}
          </select>
        )}
        {lost ? (
          <span className="status status-lost" role="alert" title={lost.transactionOpen ? `${lost.reason}
${t("transaction.lostWithTransaction")}` : lost.reason}>
            {lost.idleSeconds !== null ? t("status.idleClosed", { minutes: idleMinutes(lost.idleSeconds) }) : t("status.lost")}
            {lost.transactionOpen && <span className="small">{t("transaction.lostWithTransaction")}</span>}
            <button className="ghost small" disabled={workspaceChanging} onClick={() => void reconnect()}>
              {t("reconnect")}
            </button>
          </span>
        ) : (
          <span className={`status status-${tabStatus}`}>
            {t(`status.${tabStatus}`)}{version ? ` · ${version}` : ""}
          </span>
        )}
        {kind && <TypeBadge type={kind} />}
        {!tab.table && tabStatus === "connected" && <TransactionControls tab={tab} running={running} onNotice={setNotice} />}
        {sessionReadOnly(tab) && (
          <button
            type="button"
            className="session-read-only"
            aria-pressed="true"
            title={t("session.readOnlyHint")}
            disabled={running}
            onClick={() => void setSessionReadOnly(false)}
          >
            {t("session.readOnlyChip")}
          </button>
        )}
        {waitingRun.kind === "waiting" && (
          <span className="small muted" role="status">
            {t(waitingKey(waitingRun.waitFor))}{" "}
            <button className="ghost small" onClick={() => setWaitingRun(NOT_WAITING)}>{t("runWait.dontRun")}</button>
          </span>
        )}
        {notice && (
          <span className={`small format-error ${notice.error ? "error" : "muted"}`} role={notice.error ? "alert" : "status"}>
            {notice.text}
          </span>
        )}
        <div className="spacer" />
      </div>
      {tab.setupSkipped && tab.setupSkipped.length > 0 && (
        <div className="banner restore-notes setup-skipped" role="status" aria-label={t("setupSkipped.title")}>
          <div>
            <strong>{t("setupSkipped.title")}</strong>
            <ul>
              {tab.setupSkipped.map((skipped) => (
                <li key={skipped.index}>
                  {t("setupSkipped.item", { index: skipped.index, statement: skipped.statement, message: skipped.message })}
                </li>
              ))}
            </ul>
          </div>
          <button className="ghost small" onClick={() => updateTab(tab.id, { setupSkipped: undefined })}>{t("restore.dismiss")}</button>
        </div>
      )}
      {tab.idleRolledBack && (
        <div className="banner restore-notes" role="status">
          <span>
            {t("transaction.idleRolledBack", {
              count: tab.idleRolledBack.statements,
              duration: t("transaction.idleMinutes", { count: Math.max(1, Math.round(tab.idleRolledBack.afterSeconds / 60)) }),
            })}
          </span>
          <button className="ghost small" onClick={() => updateTab(tab.id, { idleRolledBack: undefined })}>{t("restore.dismiss")}</button>
        </div>
      )}
      {tab.restoreNotes && tab.restoreNotes.length > 0 && (
        <div className="banner restore-notes" role="status">
          <ul>
            {tab.restoreNotes.map((note, i) => (
              <li key={i}>
                {note.kind === "connectionMissing"
                  ? t("restore.connectionMissing", { connection: note.connection })
                  : note.kind === "databaseMissing"
                    ? t("restore.databaseMissing", { database: note.database })
                  : note.kind === "fileUnreadable"
                    ? t("restore.fileUnreadable", { message: note.message })
                    : t(`restore.${note.kind}`)}
              </li>
            ))}
          </ul>
          <button className="ghost small" onClick={() => updateTab(tab.id, { restoreNotes: undefined })}>{t("restore.dismiss")}</button>
        </div>
      )}
      <div className="editor-body" ref={body}>
        {tab.table && (
          <TableDataBar
            view={tab.table}
            database={tab.database ?? null}
            shown={tab.run?.results[0] ? (tab.run.results[0].total ?? tab.run.results[0].rows) : null}
            busy={running || savingEdits || pageLoading}
            onChange={changeView}
            onCount={() => void countTable()}
            onLast={() => void lastTablePage()}
            onLoadAll={() => void loadAllTable()}
            keyless={primaryKey !== null && primaryKey.length === 0}
            editable={primaryKey === null ? null : primaryKey.length > 0 && !connection?.readOnly}
            readOnlyConnection={connection?.readOnly ?? false}
            pending={pendingCount}
            connection={ownConnection}
            session={tab.id}
            columns={tableColumns}
            loaded={!!tab.run && !running && !tab.run.error && tab.run.results.length > 0}
          />
        )}
        {!tab.table && (
        <>
        {/* Maximized results hide the editor without unmounting it: its text, undo and
            cursor stay as they were. */}
        <div style={{ flex: resultsWindow ? "1 1 0" : `${editorShare} 1 0` }} className="editor-pane" hidden={maximized}>
          <div className="editor-actions" role="toolbar" aria-label={t("editorActions")}>
            {running ? (
              <button className="editor-action editor-action-cancel" aria-label={t("cancel")} title={t("cancel")} disabled={cancelRequested} onClick={() => void cancelRun()}>
                <Icon name="stopped" filled />
              </button>
            ) : (
              <button className="editor-action editor-action-run" data-run-target={runLabel?.kind ?? ""} aria-label={runName} title={`${runPicks ? t("runSelectionPickHint", { count: runLabel?.statements }) : runName} (${commandShortcut("editor.run")})`} disabled={historySaving || databaseBusy || tabStatus === "connecting" || workspaceChanging} onClick={() => editor.current?.runCurrent()}>
                <Icon name="run" filled />
              </button>
            )}
            <button className="editor-action editor-action-run" aria-label={t("runScript")} title={t("runScriptHint", { shortcut: commandShortcut("editor.runScript") })} disabled={running || historySaving || databaseBusy || tabStatus === "connecting" || workspaceChanging} onClick={() => editor.current?.runScript()}>
              <Icon name="runScript" />
            </button>
            <RunOptionsButton tab={tab} disabled={running} />
            <button className="editor-action" aria-label={t("plan.explain")} title={t("plan.explain")} disabled={running || historySaving || databaseBusy || tabStatus === "connecting" || workspaceChanging} onClick={() => void explainPressed()}><Icon name="plan" /></button>
            <EditorActionsButton runName={runName} />
            <span className="editor-actions-divider" aria-hidden="true" />
            <button className="editor-action" aria-label={t("format")} title={t("formatHint", { shortcut: commandShortcut("editor.format") })} onClick={() => editor.current?.format()}>
              <Icon name="format" />
            </button>
            {tab.file ? (
              <button className="editor-action" aria-label={t("files.saveFile")} title={t("files.saveFileHint", { shortcut: commandShortcut("file.save") })} onClick={() => void saveTab(tab.id)}>
                <Icon name="save" />
              </button>
            ) : (
              <button className="editor-action" aria-label={t("snippets.save")} title={t("snippets.saveHint", { shortcut: commandShortcut("file.save") })} onClick={() => void saveQuery()}>
                <Icon name="save" />
              </button>
            )}
            <button className="editor-action" aria-label={t("files.saveAs")} title={t("files.saveAsHint", { shortcut: commandShortcut("file.saveAs") })} onClick={() => void saveTabAs(tab.id)}>
              <Icon name="saveAs" />
            </button>
          </div>
          <SqlEditor
            handle={editor}
            onActionError={(reason) => setNotice({ text: t("actions.failed", { reason }), error: true })}
            onFormatError={(reason) => setNotice({ text: t("formatFailed", { reason }), error: true })}
            value={tab.sql}
            revision={tab.textRevision}
            onChange={(sql) => {
              if (notice?.error) setNotice(null);
              updateTab(tab.id, { sql });
            }}
            engine={connection?.engine ?? null}
            catalog={completion.catalog}
            onRun={(r, newTab) => pressRun({ kind: "run", request: r, placement: newTab ? { kind: "newTab" } : { kind: "replace" } })}
            onRunLabel={(next) => setRunLabel((before) => (before?.kind === next.kind && before.statements === next.statements ? before : next))}
            onName={showName}
            onOutline={() => setOutlineOpen((open) => !open)}
            error={editorError}
            cursorAtEnd={tab.fromTemplate}
            runName={runName}
          />
          {outlineOpen && (
            <ScriptOutline
              sql={tab.sql}
              engine={connection?.engine ?? null}
              onShow={(index) => editor.current?.showAt(index)}
              onClose={() => { setOutlineOpen(false); editor.current?.focus(); }}
            />
          )}
        </div>
        {!tab.table && completion.error ? <div className="notice error" role="status">{t("completion.loadFailed", { reason: completion.error })} <button onClick={completion.retry}>{t("completion.retry")}</button></div>
          : !tab.table && completion.loading ? <div className="muted small" role="status">{t("completion.loading")}</div> : null}
        <div
          className="divider"
          hidden={maximized || resultsWindow !== undefined}
          onPointerDown={startDrag}
          onDoubleClick={() => setEditorShare(0.42)}
          role="separator"
          tabIndex={0}
          aria-orientation="horizontal"
          aria-label={t("resizeEditor")}
          aria-valuemin={12}
          aria-valuemax={85}
          aria-valuenow={Math.round(editorShare * 100)}
          onKeyDown={(e) => {
            const step = e.shiftKey ? 0.1 : 0.02;
            if (e.key === "ArrowUp") setEditorShare((share) => Math.max(0.12, share - step));
            else if (e.key === "ArrowDown") setEditorShare((share) => Math.min(0.85, share + step));
            else if (e.key === "Home") setEditorShare(0.12);
            else if (e.key === "End") setEditorShare(0.85);
            else return;
            e.preventDefault();
          }}
        />
        </>
        )}
        {resultsWindow ? (
          // The results are in their own window (SDB-30): the run's status stays here, with the
          // way to that window and back.
          <div className="results-detached" role="status">
            <Icon name="detach" />
            <span className="results-detached-text">{running ? runStatusLine : t("detach.shownElsewhere")}</span>
            <button onClick={() => void detach()}>{t("detach.show")}</button>
            <button onClick={() => void reattachResults(tab.id).catch((e: unknown) => setNotice({ text: t("detach.failed", { reason: message(e) }), error: true }))}>{t("detach.reattach")}</button>
          </div>
        ) : (
        <div style={{ flex: tab.table || maximized ? "1 1 0" : `${1 - editorShare} 1 0` }} className="results-pane">
          {planOpen && plan ? <PlanViewer key={plan.id} summary={plan} controls={planControls} onBack={() => { setPlan(null); setPlanOpen(false); }} /> : <>{planOpen && planControls}<Results
            maximize={tab.table ? undefined : { maximized, onToggle: () => toggleResultsMaximized(tab.id) }}
            onDetach={tab.table ? undefined : () => void detach()}
            run={tab.run}
            running={running}
            connectFailed={mirror.connectFailed}
            runStatus={runStatusLine}
            cancelRequested={cancelRequested}
            session={tab.id}
            connection={ownConnection}
            table={tab.table ? { schema: tab.table.schema, name: tab.table.name, filter: tab.table.filter, orderBy: tab.table.orderBy } : null}
            serverSort={tab.table ? { sort: tab.table.sort ?? NO_TABLE_SORT, onSort: (sort) => void sortTable(sort) } : undefined}
            tableData={tab.table ? { schema: tab.table.schema, name: tab.table.name, filter: tab.table.filter, onFilter: (filter) => changeView({ filter, page: 0, all: undefined, total: undefined }), connection: ownConnection } : undefined}
            keyTable={keyTable}
            editing={editing}
            columnComments={tab.table ? columnComments : undefined}
            editingState={editingState}
            pendingEdits={pendingCount > 0}
            refreshDisabled={refreshDisabled}
            restoringView={refreshRestoring}
            rowOffset={tab.table && !tab.table.all ? tab.table.page * tab.table.pageSize : 0}
            {...resultsCallbacks}
          /></>}
        </div>
        )}
      </div>
    </div>
  );
}

/** A flag kept in one place (SDB-371): `on` for rendering and `now()` for code already running
 *  (a second click), which must not wait for a render to see it. */
function useFlag(): { on: boolean; now(): boolean; set(on: boolean): void } {
  const [on, setOn] = useState(false);
  const current = useRef(false);
  return {
    on,
    now: () => current.current,
    set: (value) => {
      current.current = value;
      setOn(value);
    },
  };
}

/** A run that failed before it started: nothing was replaced, so the results stay. */
function errorRun(text: string, translation?: Translation, results: RunState["results"] = []): RunState {
  return {
    running: false,
    results,
    activeResult: null,
    messages: [{ kind: "error", text, translation }],
    error: { message: text, offset: null, translation },
    startedAt: 0,
    elapsedMs: null,
  };
}

export function Results({
  run,
  running,
  connectFailed,
  runStatus,
  cancelRequested,
  onCancelRun,
  session,
  connection,
  table,
  onSelect,
  onSorted,
  onFiltered,
  onColumnFiltered,
  serverSort,
  onQuerySort,
  tableData,
  keyTable,
  editing,
  columnComments,
  editingState,
  pendingEdits,
  refreshDisabled,
  restoringView,
  onRefresh,
  onLoadMore,
  onRunAgain,
  onCountQuery,
  onQueryPageSize,
  rowOffset,
  onGoToRow,
  onLoadRows,
  onCancelLoad,
  maximize,
  onDetach,
}: {
  /** The button that hides the SQL editor (SQL tabs only). */
  maximize?: { maximized: boolean; onToggle(): void };
  /** Shows the results in a window of their own (SDB-30; SQL tabs only, not in that window). */
  onDetach?: () => void;
  run: RunState | null;
  /** Its run is going (`tabState.ts`'s `isRunning`). */
  running: boolean;
  /** The last attempt to connect failed: a missing plugin may be why. */
  connectFailed: boolean;
  runStatus: string;
  cancelRequested: boolean;
  onCancelRun(): void;
  session: string;
  connection: ConnectionKey | null;
  table: { schema: string; name: string; filter: string; orderBy: string } | null;
  onSelect(id: number | null): void;
  onSorted(result: number, sort: SortState[]): void;
  onFiltered(result: number, filter: string | undefined, rows: number, total: number): void;
  onColumnFiltered(result: number, columnFilters: ColumnFilterItem[], rows: number, total: number, filterMode?: FilterMode): void;
  serverSort?: { sort: SortState[]; onSort(sort: SortState[]): void };
  /** Reads a paged query again from the server in a sort's order (see `ResultGrid`). */
  onQuerySort?(result: number, sort: SortState[]): Promise<boolean>;
  tableData?: { schema: string; name: string; filter: string; onFilter(filter: string): void; connection: ConnectionKey | null };
  /** The table whose foreign keys the result follows (SDB-93). */
  keyTable?: { schema: string; name: string; connection: ConnectionKey | null };
  editing?: GridEditing;
  columnComments?: Record<string, string>;
  editingState: GridEditingState;
  pendingEdits: boolean;
  refreshDisabled: boolean;
  restoringView: boolean;
  onRefresh(result: number, row: number): void;
  onLoadMore(result: number): Promise<number>;
  /** Runs again the query of a result the core no longer keeps (SDB-367). */
  onRunAgain(result: number): void;
  onCountQuery(result: number): void;
  onQueryPageSize(size: number): void;
  /** Rows before the first one shown (a table data tab's page): the grid numbers from there. */
  rowOffset: number;
  /** "Go to row" past what the grid holds (SDB-386): `row` counts from 1, as the grid numbers. */
  onGoToRow(result: number, row: number): void;
  /** Loads a paged query's rows until it holds `until` (every row when `null`). */
  onLoadRows(result: number, until: number | null): Promise<number | null>;
  onCancelLoad(): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filteringResult, setFilteringResult] = useState<number | null>(null);
  const [refreshSlot, setRefreshSlot] = useState<HTMLSpanElement | null>(null);
  const [filterError, setFilterError] = useState<{ result: number; message: string } | null>(null);
  const [viewerOpen, setViewerOpen] = useState(false);
  const [viewerWidth, setViewerWidth] = useState(420);
  /** Columns hidden in each result's grid (positions only), for the export dialog. */
  const [hiddenColumns, setHiddenColumns] = useState<Record<number, number[]>>({});
  /** Results shown as a chart instead of the grid, and what each chart draws (ADR-040). */
  const [chartShown, setChartShown] = useState<Record<number, boolean>>({});
  const [charts, setCharts] = useState<Record<number, ChartSettings | null>>({});
  /** Results shown transposed (SDB-386): the rows of the current view shown as columns. */
  const [transposed, setTransposed] = useState<Record<number, number[]>>({});
  /** The rows each grid has selected (positions, not values), for the Transpose button. */
  const selectedRows = useRef<Record<number, number[]>>({});
  // A connection that could not open may need a plugin that is not installed.
  const connectionEngine = useApp((s) => (connection ? listedConnection(s.connections, connection)?.engine ?? null : null));
  const maximizeButton = maximize && (
    <button
      className={`ghost results-maximize${maximize.maximized ? " active" : ""}`}
      aria-pressed={maximize.maximized}
      aria-label={maximize.maximized ? t("restoreResults", { shortcut: commandShortcut("results.maximize") }) : t("maximizeResults", { shortcut: commandShortcut("results.maximize") })}
      title={maximize.maximized ? t("restoreResults", { shortcut: commandShortcut("results.maximize") }) : t("maximizeResults", { shortcut: commandShortcut("results.maximize") })}
      onClick={maximize.onToggle}
    >
      <Icon name={maximize.maximized ? "minimize" : "maximize"} />
    </button>
  );
  // Nothing to show yet: the button only brings the editor back if it was hidden.
  if (!run) return <div className="empty">{t("results.empty", { shortcut: commandShortcut("editor.run") })}{maximize?.maximized && maximizeButton}</div>;
  const active = run.results.find((r) => r.id === run.activeResult) ?? null;
  const canRefresh = !!active && (!!table || !!active.refreshable);
  const showMessages = active === null;
  const error = run.error ? translatedError(run.error, t, numbers) : null;
  return (
    <div className="results" onKeyDown={(event) => {
      if ((event.ctrlKey || event.metaKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === "s") {
        event.preventDefault();
        event.stopPropagation();
        const target = event.target instanceof Element ? event.target : null;
        if (!target?.closest('[role="dialog"]') && active && filteringResult !== active.id && editing && !editing.busy) editing.onApply();
      }
    }}>
      <div className="result-tabs">
        <ResultTabList
          results={run.results}
          activeResult={run.activeResult}
          session={table ? null : session}
          messages={t("results.messages")}
          messagesError={run.messages.some((m) => m.kind === "error")}
          onSelect={onSelect}
        />
        {active && <div className="result-tools">
          {canRefresh && <span ref={setRefreshSlot} />}
          {!restoringView && <ResultFilter
            key={`filter-${active.id}-${active.refreshScrollRow === undefined ? 0 : 1}`}
            result={active}
            scope={serverSort ? "page" : active.paging?.hasMore ? "loaded" : undefined}
            errorId={`result-filter-error-${active.id}`}
            onError={(message) => setFilterError(message ? { result: active.id, message } : null)}
            onFiltered={(filter, rows, total) => onFiltered(active.id, filter, rows, total)}
            onPending={(pending) => setFilteringResult(pending ? active.id : null)}
            locked={restoringView || !!active.paging?.loading || (editing !== undefined && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0))}
          />}
          <ExportButton
            key={`export-${active.id}`}
            result={active}
            session={session}
            connection={connection}
            sql={table ? null : (active.sql ?? null)}
            table={table}
            hidden={hiddenColumns[active.id] ?? []}
          />
          <button
            type="button"
            className={`ghost result-chart-toggle${chartShown[active.id] ? " active" : ""}`}
            aria-pressed={!!chartShown[active.id]}
            aria-label={t(chartShown[active.id] ? "chart.hide" : "chart.show")}
            title={t(chartShown[active.id] ? "chart.hide" : "chart.show")}
            onClick={() => {
              if (!(active.id in charts)) setCharts((current) => ({ ...current, [active.id]: defaultChart(active.columns) }));
              setChartShown((current) => ({ ...current, [active.id]: !current[active.id] }));
            }}
          >
            <Icon name="chart" />
          </button>
          <button
            type="button"
            className={`ghost result-transpose-toggle${transposed[active.id] ? " active" : ""}`}
            aria-pressed={!!transposed[active.id]}
            aria-label={t(transposed[active.id] ? "transpose.hide" : "transpose.show")}
            title={t(transposed[active.id] ? "transpose.hide" : "transpose.show")}
            disabled={!transposed[active.id] && active.rows === 0}
            onClick={() => setTransposed((current) => {
              const { [active.id]: shown, ...rest } = current;
              if (shown) return rest;
              // The selected rows, or the current one: the first row without a selection.
              const rows = selectedRows.current[active.id] ?? [];
              return { ...current, [active.id]: rows.length > 0 ? rows : [0] };
            })}
          >
            <Icon name="transpose" />
          </button>
        </div>}
        {onDetach && (
          <button
            className="ghost results-maximize results-detach"
            aria-label={commandTitle(t("detach.action"), "results.detach")}
            title={commandTitle(t("detach.action"), "results.detach")}
            onClick={onDetach}
          >
            <Icon name="detach" />
          </button>
        )}
        {maximizeButton}
      </div>
      {running && <ResultRunProgress key={run.startedAt} startedAt={run.startedAt} status={runStatus} cancelRequested={cancelRequested} onCancel={onCancelRun} />}
      {active && filterError?.result === active.id && <div id={`result-filter-error-${active.id}`} className="result-filter-error" role="alert">{filterError.message}</div>}
      <div className="result-body">
        {active && chartShown[active.id] && (
          <ResultChart
            key={`chart-${active.id}`}
            result={active}
            settings={charts[active.id] ?? null}
            onSettings={(settings) => setCharts((current) => ({ ...current, [active.id]: settings }))}
            scope={serverSort ? "page" : active.paging?.hasMore ? "loaded" : undefined}
          />
        )}
        {active && !chartShown[active.id] && transposed[active.id] && (
          <TransposedView
            key={`transposed-${active.id}-${active.rows}-${active.sort?.map((k) => `${k.column}${k.descending ? "d" : "a"}`).join(",") ?? ""}-${active.filter ?? ""}-${JSON.stringify(active.columnFilters ?? [])}`}
            source={active}
            rows={transposed[active.id] ?? []}
            firstNumber={rowOffset + 1}
            session={session}
            onClose={() => setTransposed(({ [active.id]: _closed, ...rest }) => rest)}
          />
        )}
        {active ? (
          <div className="result-view" hidden={!!chartShown[active.id] || !!transposed[active.id]}>
          <ResultGrid
            result={active}
            onChart={(columns) => {
              setCharts((current) => ({ ...current, [active.id]: defaultChart(active.columns, columns) }));
              setChartShown((current) => ({ ...current, [active.id]: true }));
            }}
            session={session}
            onSorted={(sort) => onSorted(active.id, sort)}
            onColumnFiltered={(columnFilters, rows, total, filterMode) => onColumnFiltered(active.id, columnFilters, rows, total, filterMode)}
            serverSort={serverSort}
            onQuerySort={onQuerySort ? (sort) => onQuerySort(active.id, sort) : undefined}
            tableData={tableData}
            keyTable={keyTable}
            columnComments={columnComments}
            editing={filteringResult === active.id || restoringView ? undefined : editing}
            editingState={editingState}
            restoringView={restoringView}
            viewerOpen={viewerOpen}
            onViewerOpen={setViewerOpen}
            viewerWidth={viewerWidth}
            onViewerWidth={setViewerWidth}
            pagingLocked={restoringView || pendingEdits || running || filteringResult === active.id || !!active.paging?.loading || !!editing?.busy}
            refresh={canRefresh && refreshSlot ? {
              target: refreshSlot,
              disabled: refreshDisabled || filteringResult === active.id || !!active.paging?.loading || !ended(active),
              run: (row) => onRefresh(active.id, row),
            } : undefined}
            onLoadMore={() => onLoadMore(active.id)}
            onCountQuery={() => onCountQuery(active.id)}
            onQueryPageSize={onQueryPageSize}
            onHiddenColumns={(hidden) => setHiddenColumns((current) => ({ ...current, [active.id]: hidden }))}
            rowOffset={rowOffset}
            onGoToRow={(row) => onGoToRow(active.id, row)}
            onLoadRows={(until) => onLoadRows(active.id, until)}
            onCancelLoad={onCancelLoad}
            onSelectedRows={(rows) => { selectedRows.current[active.id] = rows; }}
            onTranspose={(rows) => setTransposed((current) => ({ ...current, [active.id]: rows }))}
            onRunAgain={!running && (active.paging?.sql ?? active.sql) ? () => onRunAgain(active.id) : undefined}
          />
          </div>
        ) : (
          <div className="messages">
            {error && <pre className="error-box">{error}</pre>}
            {connectFailed && connectionEngine && <MissingPlugin engine={connectionEngine} />}
            {run.messages.map((m, i) => (
              <div key={i} className={`message message-${m.kind}`}>
                {m.translation ? translated(m.translation, t, numbers) : m.text}
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="statusline">
        {!running && (
          <span>
            {(() => {
              if (!active) return "";
              const filtered = !!(active.filter || active.columnFilters?.length);
              // A result cut short says so, and why, in words (SDB-367).
              const partial = partialText(active.end, active.total ?? active.rows, t, numbers);
              const rows = filtered
                ? t("results.rowsFiltered", { count: active.rows, formattedCount: numbers.format(active.rows), total: numbers.format(active.total ?? active.rows) })
                : t("results.rows", { count: active.rows, formattedCount: numbers.format(active.rows) });
              return (
                <>
                  {partial && !filtered ? "" : `${rows}${ended(active) ? "" : t("results.loading")} · `}
                  {partial && (
                    <>
                      <span className="status-partial" title={t("results.partial.hint")}>{partial}</span>
                      {" · "}
                    </>
                  )}
                </>
              );
            })()}
            {(() => {
              const short = summaryShort(run.summary);
              return short ? `${translated(short, t, numbers)} · ` : "";
            })()}
            {run.elapsedMs != null ? t("results.duration", { duration: numbers.format(Math.round(run.elapsedMs)) }) : ""}
          </span>
        )}
        {error && !showMessages && <span className="status-error">{t("results.error", { message: error.split("\n")[0] })}</span>}
      </div>
    </div>
  );
}

function ResultRunProgress({ startedAt, status, cancelRequested, onCancel }: {
  startedAt: number;
  status: string;
  cancelRequested: boolean;
  onCancel(): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const seconds = useMemo(() => new Intl.NumberFormat(i18n.language, {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  }), [i18n.language]);
  const [now, setNow] = useState(() => performance.now());
  useEffect(() => {
    const tick = window.setInterval(() => setNow(performance.now()), 100);
    return () => window.clearInterval(tick);
  }, []);
  return (
    <div className="result-run-progress">
      <span className="result-run-spinner" aria-hidden="true" />
      <span aria-hidden="true">{status}</span>
      <span className="result-run-elapsed muted" role="timer">{t("results.elapsed", { seconds: seconds.format((now - startedAt) / 1000) })}</span>
      <button type="button" disabled={cancelRequested} onClick={onCancel}>{t("results.cancelQuery")}</button>
    </div>
  );
}

/** Whole minutes for an idle time in seconds, at least one. */
function idleMinutes(seconds: number): number {
  return Math.max(1, Math.round(seconds / 60));
}
