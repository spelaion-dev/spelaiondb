// An editor tab: its own session in the core (ADR-004), editor, execution and results.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { call, commands, type CellEditItem, type ColumnFilterItem, type RowChangeItem, type ScriptEvent } from "../api";
import { openSession } from "../connect";
import { DEFAULT_PAGE_SIZE, SAVE_QUERY_EVENT, useApp, useDialogs, type RunState, type SortState, type TableView, type Tab, type Translation } from "../store";
import { byteToIndex } from "../text";
import { ExportButton } from "./ExportButton";
import { ResultFilter } from "./ResultFilter";
import { ResultGrid, type GridEditing } from "./ResultGrid";
import { SqlEditor, type RunRequest, type SqlEditorHandle } from "./SqlEditor";
import { TableDataBar } from "./TableDataBar";

/** Engines whose sessions can change database with `USE` (a selector per tab). */
function databaseSwitching(engine: string | undefined): boolean {
  return engine === "mysql" || engine === "mariadb" || engine === "sqlserver";
}

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function EditorActionIcon({ name }: { name: "run" | "script" | "format" | "save" | "cancel" }) {
  return (
    <svg viewBox="0 0 20 20" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {name === "run" && <path d="M6.5 3.8v12.4l9-6.2z" fill="currentColor" stroke="none" />}
      {name === "script" && <><path d="M3.5 4h9M3.5 7.5h6M3.5 11h4M3.5 14.5h4" /><path d="M11 9.5v6l4.5-3z" fill="currentColor" stroke="none" /></>}
      {name === "format" && <><path d="M4 4.5h12M4 8h8M7 11.5h9M7 15h6" /><path d="M4 11v4" /></>}
      {name === "save" && <><path d="M4 3.5h10l2.5 2.5v10.5H4z" /><path d="M6.5 3.5v4h7v-4M6.5 16.5v-6h7v6" /></>}
      {name === "cancel" && <rect x="5" y="5" width="10" height="10" rx="1" fill="currentColor" stroke="none" />}
    </svg>
  );
}

function translated(text: Translation, t: TFunction<"editor">, numbers: Intl.NumberFormat): string {
  const values: Record<string, string | number> = {};
  for (const [key, value] of Object.entries(text.values ?? {})) {
    values[key] = typeof value === "number" && key !== "count" ? numbers.format(value) : value;
  }
  if (typeof values.count === "number") values.formattedCount = numbers.format(values.count);
  return t(text.key, values);
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

export function EditorTab({ tab }: { tab: Tab }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const connections = useApp((s) => s.connections);
  const updateTab = useApp((s) => s.updateTab);
  const updateRun = useApp((s) => s.updateRun);
  const historySaving = useApp((s) => tab.connectionId !== null && s.historySaving.includes(tab.connectionId));
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const connection = connections.find((c) => c.id === tab.connectionId) ?? null;
  const supportsDatabase = !tab.table && databaseSwitching(connection?.engine);
  const [databases, setDatabases] = useState<string[]>([]);
  const [database, setDatabase] = useState<string | null>(null);
  const [databaseBusy, setDatabaseBusy] = useState(false);
  const [databaseError, setDatabaseError] = useState(false);
  const [databaseRefresh, setDatabaseRefresh] = useState(0);
  const databaseBusyRef = useRef(false);
  const [tables, setTables] = useState<Record<string, string[]>>({});
  const [editorShare, setEditorShare] = useState(0.42);
  const body = useRef<HTMLDivElement>(null);
  const lastRequest = useRef<RunRequest | null>(null);
  const executedRequest = useRef<RunRequest | null>(null);
  const startingRun = useRef(false);
  /** Values given to `:name` placeholders in this tab, offered again next time. */
  const paramValues = useRef<Record<string, string>>({});
  const editor = useRef<SqlEditorHandle>(null);
  // A short message in the toolbar: a formatting error or "saved as …".
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), notice.error ? 6000 : 2500);
    return () => clearTimeout(timer);
  }, [notice]);

  // Pending changes of a table data tab (ADR-014): the user's input until Apply.
  const [pending, setPending] = useState<{ cells: Record<string, string | null>; deleted: number[]; inserts: CellEditItem[][] }>({
    cells: {},
    deleted: [],
    inserts: [],
  });
  const pendingCount = Object.keys(pending.cells).length + pending.deleted.length + pending.inserts.length;
  const clearPending = () => setPending({ cells: {}, deleted: [], inserts: [] });
  /** The table's primary key (`null` until known): editing needs one. */
  const [primaryKey, setPrimaryKey] = useState<string[] | null>(null);
  const [queryTable, setQueryTable] = useState<{ result: number; schema: string; name: string; primaryKey: string[] } | null>(null);
  const [queryChecked, setQueryChecked] = useState<number | null>(null);
  const [savingEdits, setSavingEdits] = useState(false);
  const [pageLoading, setPageLoading] = useState(false);
  const savingEditsRef = useRef(false);
  const queryPageBusy = useRef(false);
  const [queryPageSize, setQueryPageSize] = useState(DEFAULT_PAGE_SIZE);
  useEffect(() => {
    updateTab(tab.id, { pendingEdits: pendingCount > 0, savingEdits });
  }, [tab.id, pendingCount, savingEdits, updateTab]);

  // A query is editable only when the core can resolve its entire result to one table.
  useEffect(() => {
    if (tab.table || tab.status !== "connected" || tab.run?.running || tab.run?.results.length !== 1) return;
    const result = tab.run.results[0];
    const request = executedRequest.current;
    if (!result?.complete || !request) return;
    let active = true;
    call(commands.editableQueryTable(tab.id, result.id, request.text))
      .then((table) => { if (active) { setQueryTable(table ? { ...table, result: result.id } : null); setQueryChecked(result.id); } })
      .catch(() => { if (active) setQueryChecked(result.id); });
    return () => { active = false; };
  }, [tab.id, tab.table, tab.status, tab.run?.running, tab.run?.results]);

  const activeResult = tab.run?.results.find((result) => result.id === tab.run?.activeResult);
  const editTarget = tab.table
    ? (primaryKey?.length && !connection?.readOnly ? { ...tab.table, primaryKey } : null)
    : (queryTable && queryTable.result === activeResult?.id ? queryTable : null);

  const editing: GridEditing | undefined =
    editTarget && activeResult?.complete && tab.status === "connected"
      ? {
          cells: pending.cells,
          deleted: pending.deleted,
          inserts: pending.inserts,
          primaryKey: editTarget.primaryKey,
          busy: (tab.run?.running ?? false) || savingEdits || pageLoading || (activeResult?.paging?.loading ?? false),
          onInsert: (values) => setPending((p) => ({ ...p, inserts: [...p.inserts, values] })),
          onUpdateInsert: (index, values) => setPending((p) => ({ ...p, inserts: p.inserts.map((row, i) => i === index ? values : row) })),
          onRemoveInsert: (index) => setPending((p) => ({ ...p, inserts: p.inserts.filter((_, i) => i !== index) })),
          onApply: () => void applyEdits(),
          onApplyCell: (row, col, value, dirty) => {
            const cells = { ...pending.cells };
            if (dirty) {
              if (value === undefined) delete cells[`${row}:${col}`];
              else cells[`${row}:${col}`] = value;
            }
            const changes = { ...pending, cells };
            setPending(changes);
            void applyEdits(changes);
          },
          onDiscard: clearPending,
          onEdit: (row, col, value) =>
            setPending((p) => {
              const cells = { ...p.cells };
              if (value === undefined) delete cells[`${row}:${col}`];
              else cells[`${row}:${col}`] = value;
              return { ...p, cells };
            }),
          onDelete: (rows) => setPending((p) => ({ ...p, deleted: [...new Set([...p.deleted, ...rows])] })),
          onRevert: (rows) =>
            setPending((p) => ({
              ...p,
              deleted: p.deleted.filter((r) => !rows.includes(r)),
              cells: Object.fromEntries(Object.entries(p.cells).filter(([k]) => !rows.includes(Number(k.split(":")[0])))),
            })),
        }
      : undefined;

  /** Sends the pending changes; the core writes and applies them in one transaction. */
  async function applyEdits(changesToApply = pending) {
    const view = editTarget;
    const result = activeResult;
    if (!view || !result || (Object.keys(changesToApply.cells).length + changesToApply.deleted.length + changesToApply.inserts.length === 0) || savingEditsRef.current) return;
    savingEditsRef.current = true;
    setSavingEdits(true);
    const deleted = new Set(changesToApply.deleted);
    const updates = new Map<number, CellEditItem[]>();
    for (const [key, value] of Object.entries(changesToApply.cells)) {
      const [row = 0, column = 0] = key.split(":").map(Number);
      if (deleted.has(row)) continue;
      updates.set(row, [...(updates.get(row) ?? []), { column, value }]);
    }
    const changes: RowChangeItem[] = [
      ...[...updates].map(([row, values]): RowChangeItem => ({ kind: "update", row, values })),
      ...changesToApply.deleted.map((row): RowChangeItem => ({ kind: "delete", row })),
      ...changesToApply.inserts.map((values): RowChangeItem => ({ kind: "insert", values })),
    ];
    try {
      let confirmed = false;
      if (connection?.env?.toLowerCase().startsWith("prod")) {
        const statements = await call(commands.previewTableEdits(tab.id, result.id, view.schema, view.name, changes));
        const ok = await useDialogs.getState().askRisks(connection.name, statements.map((sql) => ({ sql, risk: "productionWrite" as const })));
        if (!ok) return;
        confirmed = true;
      }
      const applied = await call(commands.applyTableEdits(tab.id, result.id, view.schema, view.name, changes, confirmed));
      clearPending();
      setNotice({ text: t("edit.applied", { count: applied }), error: false });
      if (tab.table) void loadPage(tab.table);
      else if (executedRequest.current) void run(executedRequest.current, true);
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      savingEditsRef.current = false;
      setSavingEdits(false);
    }
  }

  /** The page asked for while another one was loading: it loads when that one ends. */
  const queuedPage = useRef<TableView | null>(null);
  /** A page load is between its start and the start of its run. */
  const loadingPage = useRef(false);

  /** Loads a page of a table data tab: the core writes and checks the SELECT. */
  async function loadPage(view: TableView) {
    const busy = () => loadingPage.current || useApp.getState().tabs.find((t) => t.id === tab.id)?.run?.running;
    if (busy()) {
      queuedPage.current = view;
      return;
    }
    // Set before any await: a second click must queue, not start a race with this load.
    loadingPage.current = true;
    setPageLoading(true);
    try {
      await loadPageNow(view);
    } finally {
      loadingPage.current = false;
      setPageLoading(false);
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
      call(commands.describeTable(tab.id, view.schema, view.name))
        .then((details) => setPrimaryKey(details.primaryKey))
        .catch(() => setPrimaryKey([]));
    }
    clearPending();
    let sql: string;
    try {
      sql = await call(
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
      updateTab(tab.id, { run: errorRun(message(e)) });
      return;
    }
    await run({ text: sql, from: 0 }, true);
  }

  /** Changes what the table data tab shows and loads it again. */
  function changeView(patch: Partial<TableView>) {
    if (!tab.table) return;
    if (pendingCount > 0 || savingEditsRef.current) {
      setNotice({ text: t("edit.applyFirst"), error: true });
      return;
    }
    const next = { ...tab.table, ...patch };
    updateTab(tab.id, { table: next });
    void loadPage(next);
  }

  /** A header click in a table data tab: ORDER BY that column, quoted by the core. */
  async function sortTable(sort: SortState | undefined) {
    const columns = tab.run?.results[0]?.columns ?? [];
    const column = sort ? columns[sort.column]?.name : undefined;
    if (sort && !column) return;
    try {
      const orderBy = sort && column ? `${await call(commands.quoteIdentifier(tab.id, column))}${sort.descending ? " DESC" : ""}` : "";
      changeView({ sort, orderBy, page: 0 });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }

  async function countTable() {
    const view = tab.table;
    if (!view || !(await ensureConnected())) return;
    try {
      const total = await call(commands.countTableRows(tab.id, view.schema, view.name, view.filter));
      updateTab(tab.id, { table: { ...view, total: total ?? 0 } });
    } catch (e) {
      setNotice({ text: message(e), error: true });
    }
  }

  /** Fetch the next block of the original SELECT without replacing the rows already shown. */
  async function loadMoreQueryRows(resultId: number): Promise<number> {
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    const result = current?.run?.results.find((item) => item.id === resultId);
    const paging = result?.paging;
    if (!result || !result.complete || !paging || !paging.hasMore || paging.loading || current?.run?.running ||
        queryPageBusy.current || savingEditsRef.current || pendingCount > 0 || result.sort || result.filter || result.columnFilters?.length ||
        (paging.total !== undefined && result.rows >= paging.total)) return 0;
    queryPageBusy.current = true;
    updateRun(tab.id, (run) => ({ ...run, results: run.results.map((item) => item.id === resultId && item.paging
      ? { ...item, paging: { ...item.paging, loading: true } } : item) }));
    try {
      const added = await call(commands.appendQueryPage(tab.id, resultId, paging.sql, paging.pageSize, result.rows));
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
    if (!result?.complete || !result.paging || tab.run?.running || queryPageBusy.current || savingEditsRef.current || pendingCount > 0 || result.sort || result.filter || result.columnFilters?.length) return;
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
    if (queryPageBusy.current || pendingCount > 0 || savingEditsRef.current || !executedRequest.current) return;
    setQueryPageSize(size);
    void run(executedRequest.current, false, size);
  }

  useEffect(() => {
    const queued = queuedPage.current;
    if (tab.run?.running || !queued) return;
    queuedPage.current = null;
    void loadPage(queued);
    // Runs when a load ends.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.run?.running]);

  // A table data tab shows its first page as soon as it opens.
  const opened = useRef(false);
  useEffect(() => {
    if (opened.current || !tab.table || tab.run || tab.status === "connecting") return;
    opened.current = true;
    void loadPage(tab.table);
    // Wait for an explorer-created tab's independent session before loading its first page.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab.status, tab.table, tab.run]);

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
      await call(commands.saveSnippet(name, tab.sql, tab.connectionId));
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

  // Autocompletion: tables and views of the user schemas.
  useEffect(() => {
    if (tab.status !== "connected") return;
    let cancelled = false;
    (async () => {
      const schemas = (await call(commands.listSchemas(tab.id))).filter((s) => !s.system).slice(0, 20);
      const out: Record<string, string[]> = {};
      for (const schema of schemas) {
        for (const o of await call(commands.listObjects(tab.id, schema.name))) {
          out[`${o.schema}.${o.name}`] = [];
          if (o.schema === "public" || o.schema === "main" || o.schema === database) out[o.name] = [];
        }
      }
      if (!cancelled) setTables(out);
    })().catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [tab.status, tab.id, database]);

  // Read the server state after connecting and after each script, including failed scripts:
  // a USE earlier in a script can succeed even when a later statement fails.
  useEffect(() => {
    if (!supportsDatabase || tab.status !== "connected") {
      setDatabase(null);
      setDatabases([]);
      setDatabaseError(false);
      databaseBusyRef.current = false;
      setDatabaseBusy(false);
      return;
    }
    if (tab.run?.running) return;
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
  }, [supportsDatabase, tab.id, tab.status, tab.connectionId, tab.run?.running, databaseRefresh]);

  async function changeDatabase(next: string) {
    if (pendingCount > 0 || savingEditsRef.current || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (!next || databaseBusyRef.current) return;
    databaseBusyRef.current = true;
    setDatabaseBusy(true);
    try {
      await call(commands.selectDatabase(tab.id, next));
      setDatabase(next);
      updateRun(tab.id, (r) => ({ ...r, results: r.results.map((result) => ({ ...result, paging: undefined })) }));
    } catch (e) {
      setNotice({ text: message(e), error: true });
    } finally {
      databaseBusyRef.current = false;
      setDatabaseBusy(false);
    }
  }

  async function ensureConnected(): Promise<boolean> {
    // The store, not this render's `tab`: a caller may have just connected (loading a
    // table page connects, then runs its SELECT through `run`).
    const status = useApp.getState().tabs.find((t) => t.id === tab.id)?.status;
    if (status === "connected") return true;
    if (status === "connecting") return false;
    if (!tab.connectionId) {
      updateTab(tab.id, { run: errorRun("", { key: "pickConnection" }) });
      return false;
    }
    updateTab(tab.id, { status: "connecting" });
    try {
      const version = await openSession(tab.id, tab.connectionId);
      if (version === null) {
        updateTab(tab.id, { status: "disconnected" });
        return false;
      }
      updateTab(tab.id, { status: "connected", serverVersion: version });
      return true;
    } catch (e) {
      const detail = message(e);
      updateTab(tab.id, { status: "disconnected", run: errorRun(detail, { key: "messages.connectionFailed", values: { detail } }) });
      return false;
    }
  }

  async function changeConnection(id: string) {
    if (pendingCount > 0 || savingEditsRef.current || queryPageBusy.current) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    await call(commands.disconnect(tab.id)).catch(() => undefined);
    setTables({});
    updateRun(tab.id, (r) => ({ ...r, results: r.results.map((result) => ({ ...result, paging: undefined })) }));
    updateTab(tab.id, { connectionId: id || null, status: "disconnected", serverVersion: null });
  }

  async function run(request: RunRequest, refreshingEdits = false, pageSize = queryPageSize) {
    if (!refreshingEdits && (pendingCount > 0 || savingEditsRef.current)) { setNotice({ text: t("edit.applyFirst"), error: true }); return; }
    if (queryPageBusy.current || startingRun.current) return;
    if (!request.text.trim() || tab.run?.running || databaseBusyRef.current || useApp.getState().workspaceChanging ||
        (tab.connectionId && useApp.getState().historySaving.includes(tab.connectionId))) return;
    startingRun.current = true;
    try {
    if (!(await ensureConnected())) return;
    // `:name` placeholders: the core lists them and writes the values in as literals.
    let text = request.text;
    const names = await call(commands.scriptParams(tab.id, text)).catch(() => [] as string[]);
    if (names.length > 0) {
      const values = await useDialogs.getState().askParams(names, paramValues.current);
      if (!values) return;
      paramValues.current = { ...paramValues.current, ...values };
      try {
        text = await call(commands.bindParams(tab.id, text, values));
      } catch (e) {
        const detail = message(e);
        updateTab(tab.id, { run: errorRun(detail, { key: "messages.runFailed", values: { detail } }) });
        return;
      }
    }
    // Positions in an error refer to the text that ran; with values written in, they no
    // longer match the editor, so nothing is underlined.
    const sent: RunRequest & { bound: boolean } = { text, from: request.from, bound: text !== request.text };
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
    const canPage = !tab.table && await call(commands.queryPageSupported(tab.id, sent.text)).catch(() => false);
    const queryPage = canPage ? { sql: sent.text, pageSize } : null;
    for (const r of tab.run?.results ?? []) void call(commands.releaseResult(r.id)).catch(() => undefined);
    lastRequest.current = request;
    executedRequest.current = { text: sent.text, from: 0 };
    setQueryTable(null);
    setQueryChecked(null);
    const startedAt = performance.now();
    updateTab(tab.id, {
      run: { running: true, results: [], activeResult: null, messages: [], error: null, startedAt, elapsedMs: null },
    });
    const channel = new Channel<ScriptEvent>();
    channel.onmessage = (event) => handle(event, sent, startedAt, queryPage);
    try {
      if (queryPage) await call(commands.runQueryPage(tab.id, sent.text, pageSize, channel));
      else await call(commands.runScript(tab.id, sent.text, confirmed, channel));
    } catch (e) {
      const detail = message(e);
      updateRun(tab.id, (r) => ({ ...r, running: false, error: { message: detail, offset: null, translation: { key: "messages.runFailed", values: { detail } } } }));
    }
    } finally {
      startingRun.current = false;
    }
  }

  function handle(event: ScriptEvent, request: RunRequest & { bound: boolean }, startedAt: number, page: { sql: string; pageSize: number } | null) {
    switch (event.kind) {
      case "statementStarted":
        break;
      case "resultStarted":
        updateRun(tab.id, (r) => ({
          ...r,
          results: [...r.results, { id: event.result, statement: event.index, columns: event.columns, rows: 0, complete: false, ...(page ? { paging: { ...page, hasMore: false, loading: false } } : {}) }],
          activeResult: r.activeResult ?? event.result,
        }));
        break;
      case "rows":
        updateRun(tab.id, (r) => ({
          ...r,
          results: r.results.map((x) => (x.id === event.result ? { ...x, rows: event.totalRows } : x)),
        }));
        break;
      case "notice":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: `${event.severity}: ${event.message}` }] }));
        break;
      case "historyWarning":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: event.message, translation: { key: "messages.historySaveFailed", values: { detail: event.message } } }] }));
        break;
      case "statementFinished": {
        const ms = Math.round(event.elapsedMs ?? 0);
        const translation: Translation = event.rowsAffected == null
          ? { key: "messages.statementOk", values: { index: event.index + 1, duration: ms } }
          : { key: "messages.statementRows", values: { index: event.index + 1, count: event.rowsAffected, duration: ms } };
        updateRun(tab.id, (r) => ({
          ...r,
          results: r.results.map((x) => (x.statement === event.index ? { ...x, complete: true, ...(x.paging ? { paging: { ...x.paging, hasMore: x.rows === x.paging.pageSize } } : {}) } : x)),
          messages: [...r.messages, { kind: "info", text: "", translation }],
        }));
        break;
      }
      case "statementFailed": {
        const offset =
          event.scriptOffset == null || request.bound ? null : request.from + byteToIndex(request.text, event.scriptOffset);
        const diagnostic = { message: event.message, detail: event.detail, hint: event.hint, code: event.code, cancelled: event.cancelled };
        updateRun(tab.id, (r) => ({
          ...r,
          error: { message: event.message, offset, diagnostic },
          messages: [...r.messages, { kind: "error", text: event.message, translation: { key: event.cancelled ? "messages.statementCancelled" : "messages.statementFailed", values: { index: event.index + 1, reason: event.message } } }],
        }));
        break;
      }
      case "scriptFinished":
        updateRun(tab.id, (r) => ({ ...r, running: false, elapsedMs: performance.now() - startedAt }));
        useApp.getState().refreshHistory();
        break;
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

  const running = tab.run?.running ?? false;
  return (
    <div className="editor-tab">
      <div className="toolbar">
        <select value={tab.connectionId ?? ""} onChange={(e) => void changeConnection(e.target.value)} aria-label={t("connection")} disabled={running || databaseBusy || tab.status === "connecting" || historySaving || workspaceChanging || activeResult?.paging?.loading}>
          <option value="">{t("noConnection")}</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name} ({c.engine}
              {c.env ? `, ${c.env}` : ""})
            </option>
          ))}
        </select>
        {supportsDatabase && (tab.status === "connected" ? (
          <><select aria-label={t("database")} title={t("database")} value={database ?? ""} onChange={(e) => void changeDatabase(e.target.value)} disabled={running || databaseBusy || databaseError || historySaving || workspaceChanging || activeResult?.paging?.loading}>
            <option value="" disabled>{databaseBusy ? t("loadingDatabases") : databaseError ? t("databaseLoadFailed") : t("noDatabase")}</option>
            {database && !databases.includes(database) && <option value={database}>{database}</option>}
            {databases.map((name) => <option key={name} value={name}>{name}</option>)}
          </select>{databaseError && <button className="ghost" disabled={running || databaseBusy} onClick={() => setDatabaseRefresh((n) => n + 1)}>{t("retryDatabases")}</button>}</>
        ) : (
          <button className="ghost" disabled={tab.status === "connecting" || workspaceChanging} onClick={() => void ensureConnected()}>{t("connectToChooseDatabase")}</button>
        ))}
        <span className={`status status-${tab.status}`}>
          {t(`status.${tab.status}`)}{tab.status === "connected" && tab.serverVersion ? ` · ${tab.serverVersion}` : ""}
        </span>
        {connection?.env?.toLowerCase().startsWith("prod") && <span className="badge badge-prod">{t("production")}</span>}
        {notice && (
          <span className={`small format-error ${notice.error ? "error" : "muted"}`} role={notice.error ? "alert" : "status"}>
            {notice.text}
          </span>
        )}
        <div className="spacer" />
        {tab.table && (running ? (
          <button className="danger" onClick={() => void call(commands.cancel(tab.id)).catch(() => undefined)}>
            {t("cancel")}
          </button>
        ) : (
          <button
            className="primary"
            disabled={historySaving || databaseBusy || tab.status === "connecting" || workspaceChanging || savingEdits}
            title={t("table.refreshHint")}
            onClick={() => {
              if (pendingCount > 0) setNotice({ text: t("edit.applyFirst"), error: true });
              else if (tab.table) void loadPage(tab.table);
            }}
          >
            {t("table.refresh")}
          </button>
        ))}
      </div>
      <div className="editor-body" ref={body}>
        {tab.table && (
          <TableDataBar
            view={tab.table}
            shown={tab.run?.results[0] ? (tab.run.results[0].total ?? tab.run.results[0].rows) : null}
            busy={running || savingEdits || pageLoading}
            onChange={changeView}
            onCount={() => void countTable()}
            editable={primaryKey === null ? null : primaryKey.length > 0 && !connection?.readOnly}
            readOnlyConnection={connection?.readOnly ?? false}
            pending={pendingCount}
          />
        )}
        {!tab.table && (
        <>
        <div style={{ flex: `${editorShare} 1 0` }} className="editor-pane">
          <div className="editor-actions" role="toolbar" aria-label={t("editorActions")}>
            {running ? (
              <button className="editor-action editor-action-cancel" aria-label={t("cancel")} title={t("cancel")} onClick={() => void call(commands.cancel(tab.id)).catch(() => undefined)}>
                <EditorActionIcon name="cancel" />
              </button>
            ) : (
              <button className="editor-action editor-action-run" aria-label={t("run")} title={t("runHint")} disabled={historySaving || databaseBusy || tab.status === "connecting" || workspaceChanging} onClick={() => editor.current?.runCurrent()}>
                <EditorActionIcon name="run" />
              </button>
            )}
            <button className="editor-action editor-action-run" aria-label={t("runScript")} title={t("runScriptHint")} disabled={running || historySaving || databaseBusy || tab.status === "connecting" || workspaceChanging} onClick={() => editor.current?.runScript()}>
              <EditorActionIcon name="script" />
            </button>
            <span className="editor-actions-divider" aria-hidden="true" />
            <button className="editor-action" aria-label={t("format")} title={t("formatHint")} onClick={() => editor.current?.format()}>
              <EditorActionIcon name="format" />
            </button>
            <button className="editor-action" aria-label={t("snippets.save")} title={t("snippets.saveHint")} onClick={() => void saveQuery()}>
              <EditorActionIcon name="save" />
            </button>
          </div>
          <SqlEditor
            handle={editor}
            onFormatError={(reason) => setNotice({ text: t("formatFailed", { reason }), error: true })}
            value={tab.sql}
            onChange={(sql) => {
              if (notice?.error) setNotice(null);
              updateTab(tab.id, { sql });
            }}
            engine={connection?.engine ?? null}
            tables={tables}
            onRun={(r) => void run(r)}
            error={editorError}
          />
        </div>
        <div className="divider" onPointerDown={startDrag} role="separator" aria-orientation="horizontal" aria-label={t("resizeEditor")} />
        </>
        )}
        <div style={{ flex: tab.table ? "1 1 0" : `${1 - editorShare} 1 0` }} className="results-pane">
          <Results
            run={tab.run}
            session={tab.id}
            serverSort={tab.table ? { sort: tab.table.sort, onSort: (sort) => void sortTable(sort) } : undefined}
            editing={editing}
            readOnlyHint={!tab.table && queryChecked === activeResult?.id && !queryTable}
            pendingEdits={pendingCount > 0}
            onLoadMore={loadMoreQueryRows}
            onCountQuery={(id) => void countQueryResult(id)}
            onQueryPageSize={changeQueryPageSize}
            onSelect={(id) => updateRun(tab.id, (r) => ({ ...r, activeResult: id }))}
            onSorted={(result, sort) =>
              updateRun(tab.id, (r) => ({ ...r, results: r.results.map((x) => (x.id === result ? { ...x, sort } : x)) }))
            }
            onColumnFiltered={(result, columnFilters, rows, total) =>
              updateRun(tab.id, (r) => ({
                ...r,
                results: r.results.map((x) => (x.id === result ? { ...x, columnFilters, rows, total } : x)),
              }))
            }
            onFiltered={(result, filter, rows, total) =>
              updateRun(tab.id, (r) => ({
                ...r,
                results: r.results.map((x) => (x.id === result ? { ...x, filter, rows, total } : x)),
              }))
            }
          />
        </div>
      </div>
    </div>
  );
}

function errorRun(text: string, translation?: Translation): RunState {
  return {
    running: false,
    results: [],
    activeResult: null,
    messages: [{ kind: "error", text, translation }],
    error: { message: text, offset: null, translation },
    startedAt: 0,
    elapsedMs: null,
  };
}

function Results({
  run,
  session,
  onSelect,
  onSorted,
  onFiltered,
  onColumnFiltered,
  serverSort,
  editing,
  readOnlyHint,
  pendingEdits,
  onLoadMore,
  onCountQuery,
  onQueryPageSize,
}: {
  run: RunState | null;
  session: string;
  onSelect(id: number | null): void;
  onSorted(result: number, sort: SortState | undefined): void;
  onFiltered(result: number, filter: string | undefined, rows: number, total: number): void;
  onColumnFiltered(result: number, columnFilters: ColumnFilterItem[], rows: number, total: number): void;
  serverSort?: { sort: SortState | undefined; onSort(sort: SortState | undefined): void };
  editing?: GridEditing;
  readOnlyHint?: boolean;
  pendingEdits: boolean;
  onLoadMore(result: number): Promise<number>;
  onCountQuery(result: number): void;
  onQueryPageSize(size: number): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filteringResult, setFilteringResult] = useState<number | null>(null);
  const [viewerOpen, setViewerOpen] = useState(false);
  const [viewerWidth, setViewerWidth] = useState(420);
  if (!run) return <div className="empty">{t("results.empty")}</div>;
  const active = run.results.find((r) => r.id === run.activeResult) ?? null;
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
        {run.results.map((r, i) => (
          <button key={r.id} className={r.id === run.activeResult ? "active" : ""} onClick={() => onSelect(r.id)}>
            {t("results.result", { number: numbers.format(i + 1) })}
          </button>
        ))}
        <button className={showMessages ? "active" : ""} onClick={() => onSelect(null)}>
          {t("results.messages")}{run.messages.some((m) => m.kind === "error") ? " ⚠" : ""}
        </button>
        {active && (
          <ResultFilter
            key={`filter-${active.id}`}
            result={active}
            onFiltered={(filter, rows, total) => onFiltered(active.id, filter, rows, total)}
            onPending={(pending) => setFilteringResult(pending ? active.id : null)}
            locked={!!active.paging?.loading || (editing !== undefined && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0))}
          />
        )}
        {active && <ExportButton key={`export-${active.id}`} result={active} session={session} />}
      </div>
      <div className="result-body">
        {active ? (
          <ResultGrid
            result={active}
            session={session}
            onSorted={(sort) => onSorted(active.id, sort)}
            onColumnFiltered={(columnFilters, rows, total) => onColumnFiltered(active.id, columnFilters, rows, total)}
            serverSort={serverSort}
            editing={filteringResult === active.id ? undefined : editing}
            viewerOpen={viewerOpen}
            onViewerOpen={setViewerOpen}
            viewerWidth={viewerWidth}
            onViewerWidth={setViewerWidth}
            pagingLocked={pendingEdits || run.running || filteringResult === active.id || !!active.paging?.loading || !!editing?.busy}
            onLoadMore={() => onLoadMore(active.id)}
            onCountQuery={() => onCountQuery(active.id)}
            onQueryPageSize={onQueryPageSize}
          />
        ) : (
          <div className="messages">
            {error && <pre className="error-box">{error}</pre>}
            {run.messages.map((m, i) => (
              <div key={i} className={`message message-${m.kind}`}>
                {m.translation ? translated(m.translation, t, numbers) : m.text}
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="statusline">
        {run.running ? (
          <span>{t("results.running")}</span>
        ) : (
          <span>
            {active
              ? `${
                  active.filter || active.columnFilters?.length
                    ? t("results.rowsFiltered", { count: active.rows, formattedCount: numbers.format(active.rows), total: numbers.format(active.total ?? active.rows) })
                    : t("results.rows", { count: active.rows, formattedCount: numbers.format(active.rows) })
                }${active.complete ? "" : t("results.loading")} · `
              : ""}
            {run.elapsedMs != null ? t("results.duration", { duration: numbers.format(Math.round(run.elapsedMs)) }) : ""}
          </span>
        )}
        {error && !showMessages && <span className="status-error">{t("results.error", { message: error.split("\n")[0] })}</span>}
        {active && readOnlyHint && <span className="muted" title={t("edit.queryReadOnlyHint")}>{t("edit.readOnly")}</span>}
      </div>
    </div>
  );
}
