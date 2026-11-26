// A SQL notebook tab (ADR-087): Markdown and SQL cells over one session, each SQL cell with its
// own results in the core (requested in windows by its grid, as points by its chart), Run all
// with a stop at the first cell that does not succeed, and an HTML export.
//
// Opening a notebook runs nothing and connects nothing: a cell runs only on the user's click.
// A cell runs as an editor script: the core expands its `{{name}}` references into CTEs, asks
// which statements need a confirmation, and runs it through the same `check_run` (the read-only
// barrier and the guard). The page holds the cells and what each run said, never a row.

import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { Channel } from "@tauri-apps/api/core";
import { EditorState } from "@codemirror/state";
import { EditorView, keymap, drawSelection, highlightActiveLine, highlightSpecialChars } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { syntaxHighlighting, defaultHighlightStyle, bracketMatching } from "@codemirror/language";
import { sql as sqlLanguage } from "@codemirror/lang-sql";
import { call, commands, type ScriptEvent } from "../api";
import type { Block, CellOutputItem, Inline } from "../bindings";
import { connectionOfTag, connectionTag, keyOf, listedConnection, tabConnection, tabConnectionOf } from "../connectionKey";
import { connectionTooltip } from "../connectionInfo";
import { confirmOpenTransactions, openTabSession } from "../connect";
import { chooseSavePath } from "../dialogs";
import { useConnectionName } from "../folders";
import { cellId, newNotebookCellName, notebookCellsReferenced, notebookOf, readNotebookCells, setCells, syncNotebookText } from "../notebook";
import { endResult } from "../runResults";
import { useApp, useDialogs, type NotebookCell, type ResultMeta, type Tab } from "../store";
import { CONNECTING, DISCONNECTED, connectFailed, connectedWith, isRunning, lostSession, serverVersion, statusName } from "../tabState";
import { byteToIndex } from "../text";
import { ResultChart, standaloneSvg } from "./ResultChart";
import { defaultChart, type ChartSettings } from "../chartDefaults";
import { ResultGrid } from "./ResultGrid";
import { runOptionsOf } from "./RunOptions";
import { sqlEditorColors, sqlEditorTheme } from "./SqlEditor";
import { TransactionControls } from "./TransactionControls";
import { TypeBadge } from "./TypeBadge";
import { Icon } from "./Icons";

/** The SQL cell's own key: runs the cell (the region's own key, as the guidelines' map says). */
const RUN_CELL_KEY = "Mod-Enter";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** What a SQL cell's last run said. The rows stay in the core. */
type CellRun =
  | { kind: "running"; results: ResultMeta[]; messages: string[] }
  | { kind: "done"; results: ResultMeta[]; messages: string[]; elapsedMs: number }
  | { kind: "failed"; results: ResultMeta[]; messages: string[]; error: string };

/** How a cell's run ended, for Run all. */
type Outcome = { kind: "ok" } | { kind: "failed"; error: string } | { kind: "declined" } | { kind: "cancelled" } | { kind: "notConnected" };

/** Run all's state on the page: idle, going (which cell), or ended with its summary. */
type RunAll =
  | { kind: "idle" }
  | { kind: "running"; cell: string }
  | { kind: "ended"; ran: number; total: number; stopped: { cell: string; reason: Outcome } | null; elapsedMs: number };

export function NotebookTab({ tab }: { tab: Tab }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const connections = useApp((s) => s.connections);
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const updateTab = useApp((s) => s.updateTab);
  const setConnection = useApp((s) => s.setConnection);
  const shownName = useConnectionName();
  const ownConnection = tabConnection(tab);
  const connection = (ownConnection ? listedConnection(connections, ownConnection) : undefined) ?? null;
  const status = statusName(tab);
  const lost = lostSession(tab);
  const version = serverVersion(tab);
  const sessionRunning = isRunning(tab);
  const notebook = tab.notebook!;
  const cells = notebook.cells;
  const [runs, setRuns] = useState<Record<string, CellRun>>({});
  const [runAll, setRunAll] = useState<RunAll>({ kind: "idle" });
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  const [shown, setShown] = useState<Record<string, number>>({});
  const [charts, setCharts] = useState<Record<number, ChartSettings | null>>({});
  // Grid or chart is the cell's choice, so running the cell again keeps it (its new result gets a
  // default chart until the user changes it); a chart's settings are its result's.
  const [chartShown, setChartShown] = useState<Record<string, boolean>>({});
  const [viewerOpen, setViewerOpen] = useState(false);
  const [viewerWidth, setViewerWidth] = useState(320);
  const [expanded, setExpanded] = useState<Record<string, string | null>>({});
  const [menu, setMenu] = useState<{ cell: string; x: number; y: number } | null>(null);
  const busy = useRef(false);
  const stopAll = useRef(false);
  const running = sessionRunning || runAll.kind === "running" || Object.values(runs).some((run) => run.kind === "running");

  // An opened or restored notebook's cells are read from its text; nothing runs.
  useEffect(() => {
    if (cells === null && !notebook.problem) void readNotebookCells(tab.id);
  }, [tab.id, cells, notebook.problem]);

  // The core writes the text again after the cells change: what a save writes and the kept tab
  // keeps. A notebook read and not changed keeps its file's text as it was.
  useEffect(() => {
    if (notebook.revision === 0) return;
    const timer = setTimeout(() => void syncNotebookText(tab.id), 150);
    return () => clearTimeout(timer);
  }, [tab.id, notebook.revision]);

  // A cell that is gone (deleted, or the file read again) frees its results in the core.
  const cellIds = (cells ?? []).map((cell) => cell.id).join(" ");
  useEffect(() => {
    const present = new Set(cellIds.split(" "));
    setRuns((current) => {
      const gone = Object.keys(current).filter((id) => !present.has(id));
      if (gone.length === 0) return current;
      for (const id of gone) void call(commands.releaseNotebookCell(tab.id, id)).catch(() => undefined);
      return Object.fromEntries(Object.entries(current).filter(([id]) => present.has(id)));
    });
  }, [tab.id, cellIds]);

  function tabNow(): Tab | undefined {
    return useApp.getState().tabs.find((item) => item.id === tab.id);
  }

  function cellsNow(): NotebookCell[] {
    return tabNow()?.notebook?.cells ?? [];
  }

  function change(next: NotebookCell[]) {
    setCells(tab.id, next);
  }

  function updateCell(id: string, patch: Partial<NotebookCell>) {
    change(cellsNow().map((cell) => (cell.id === id ? { ...cell, ...patch } : cell)));
  }

  async function addCell(kind: NotebookCell["kind"], after?: string) {
    const current = cellsNow();
    const name = kind === "sql" ? await newNotebookCellName(tabNow() ?? tab, current) : "";
    const cell: NotebookCell = { id: cellId(), kind, name, text: "" };
    const at = after ? current.findIndex((c) => c.id === after) + 1 : current.length;
    change([...current.slice(0, at), cell, ...current.slice(at)]);
  }

  function moveCell(id: string, by: -1 | 1) {
    const current = [...cellsNow()];
    const at = current.findIndex((cell) => cell.id === id);
    const to = at + by;
    if (at < 0 || to < 0 || to >= current.length) return;
    [current[at], current[to]] = [current[to]!, current[at]!];
    change(current);
  }

  async function deleteCell(id: string) {
    const cell = cellsNow().find((c) => c.id === id);
    if (!cell) return;
    if (cell.text.trim()) {
      const answer = await useDialogs.getState().askChoice({
        title: t("notebook.deleteTitle"),
        message: t(cell.kind === "sql" ? "notebook.deleteSqlMessage" : "notebook.deleteMarkdownMessage", { name: cell.name }),
        choices: [{ id: "cancel", label: t("notebook.cancel") }, { id: "delete", label: t("notebook.deleteCell"), danger: true }],
      });
      if (answer !== "delete") return;
    }
    change(cellsNow().filter((c) => c.id !== id));
  }

  async function ensureConnected(): Promise<boolean> {
    const current = tabNow();
    const gone = current ? lostSession(current) : null;
    // A lost session is reconnected only from its button, and nothing is run then (ADR-028).
    if (gone) { setNotice({ text: t("notebook.lost"), error: true }); return false; }
    const now = current ? statusName(current) : "disconnected";
    if (now === "connected") return true;
    if (now === "connecting") return false;
    if (!tab.connectionId) { setNotice({ text: t("pickConnection"), error: true }); return false; }
    setConnection(tab.id, CONNECTING);
    try {
      const session = await openTabSession(tab.id, tabConnectionOf(tab.connectionId, tab.connectionFolder));
      if (session === null) { setConnection(tab.id, DISCONNECTED); return false; }
      setConnection(tab.id, connectedWith(session));
      return true;
    } catch (e) {
      const detail = message(e);
      updateTab(tab.id, { connectionState: connectFailed(detail) });
      setNotice({ text: t("notebook.connectFailed", { detail }), error: true });
      return false;
    }
  }

  async function changeConnection(value: string) {
    if (!(await confirmOpenTransactions([tab.id], "disconnect"))) return;
    await call(commands.disconnect(tab.id)).catch(() => undefined);
    const picked = connectionOfTag(value);
    updateTab(tab.id, { connectionId: picked?.id ?? null, connectionFolder: picked?.folder ?? null, database: null, connectionState: DISCONNECTED });
    // The file names the connection it was last run on: the text changes with it.
    change(cellsNow());
  }

  async function reconnect() {
    const current = tabNow();
    if (current && lostSession(current)) setConnection(tab.id, DISCONNECTED);
    if (await ensureConnected()) setNotice({ text: t("reconnected"), error: false });
  }

  function setRun(cell: string, update: (run: CellRun | undefined) => CellRun) {
    setRuns((current) => ({ ...current, [cell]: update(current[cell]) }));
  }

  /** Runs one SQL cell as the editor runs a script, after the core expanded its references. */
  async function runCell(id: string): Promise<Outcome> {
    if (busy.current) return { kind: "cancelled" };
    busy.current = true;
    try {
      if (!(await ensureConnected())) return { kind: "notConnected" };
      const current = cellsNow();
      const index = current.findIndex((cell) => cell.id === id);
      const cell = current[index];
      if (!cell || cell.kind !== "sql") return { kind: "ok" };
      if (!cell.text.trim()) return { kind: "ok" };
      let sql: string;
      try {
        sql = (await call(commands.expandNotebookCell(tab.id, notebookOf(tabNow() ?? tab, current), index))).sql;
      } catch (e) {
        const error = message(e);
        setRun(id, (run) => ({ kind: "failed", results: run?.results ?? [], messages: [], error }));
        return { kind: "failed", error };
      }
      // The core says which statements need a confirmation; without it, it refuses the run.
      const risky = await call(commands.scriptRisks(tab.id, sql)).catch(() => []);
      let confirmed = false;
      if (risky.length > 0) {
        const statements = risky.map((r) => ({ sql: sql.slice(byteToIndex(sql, r.start), byteToIndex(sql, r.end)), risk: r.risk }));
        if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return { kind: "declined" };
        confirmed = true;
      }
      const startedAt = performance.now();
      let failure: string | null = null;
      let cancelled = false;
      setRun(id, (run) => ({ kind: "running", results: run?.results ?? [], messages: [] }));
      const channel = new Channel<ScriptEvent>();
      let finish: (() => void) | undefined;
      const finished = new Promise<void>((resolve) => { finish = resolve; });
      const failures = new Map<number, string>();
      channel.onmessage = (event) => {
        switch (event.kind) {
          case "runStarted":
            setRun(id, (run) => ({ kind: "running", results: (run?.results ?? []).filter((r) => !event.closed.includes(r.id)), messages: [] }));
            break;
          case "resultStarted": {
            const started: ResultMeta = { id: event.result, statement: event.index, columns: event.columns, rows: 0, end: { kind: "loading" }, ...(event.name ? { name: event.name } : {}) };
            setRun(id, (run) => ({ kind: "running", messages: run?.messages ?? [], results: [...(run?.results ?? []).filter((r) => r.id !== event.replaces && !event.closed.includes(r.id)), started] }));
            setShown((s) => ({ ...s, [id]: event.result }));
            break;
          }
          case "rows":
            setRun(id, (run) => (run ? { ...run, results: run.results.map((r) => (r.id === event.result ? { ...r, rows: event.totalRows } : r)) } : run!));
            break;
          case "resultEnded":
            setRun(id, (run) => (run ? { ...run, results: endResult(run.results, event.result, event.end) } : run!));
            break;
          case "notice":
            setRun(id, (run) => (run ? { ...run, messages: [...run.messages, `${event.severity}: ${event.message}`] } : run!));
            break;
          case "statementFinished": {
            const text = event.rowsAffected == null
              ? t("notebook.statementOk", { index: event.index + 1 })
              : t("notebook.statementRows", { index: event.index + 1, count: event.rowsAffected, rows: numbers.format(event.rowsAffected) });
            setRun(id, (run) => (run ? { ...run, messages: [...run.messages, text] } : run!));
            break;
          }
          case "statementFailed":
            failures.set(event.index, event.message);
            failure ??= event.message;
            if (event.end === "cancelled") cancelled = true;
            break;
          case "errorPaused": {
            // The core waits for the answer to this ticket; the dialog only asks (ADR-035).
            const ticket = event.ticket;
            void useDialogs.getState()
              .askScriptError({ tab: tab.id, statement: event.index + 1, message: failures.get(event.index) ?? "", remaining: event.remaining })
              .then((answer) => call(commands.answerScriptError(tab.id, ticket, answer)))
              .catch(() => undefined);
            break;
          }
          case "scriptFinished": {
            const elapsedMs = performance.now() - startedAt;
            if (event.summary.stop?.kind === "cancelled") cancelled = true;
            const error = failure ?? (event.summary.stop ? t(`messages.stop.${event.summary.stop.kind}`, { index: event.summary.stop.index + 1 }) : null);
            if (failure === null && error) failure = error;
            setRun(id, (run) => error
              ? { kind: "failed", results: run?.results ?? [], messages: run?.messages ?? [], error }
              : { kind: "done", results: run?.results ?? [], messages: run?.messages ?? [], elapsedMs });
            useApp.getState().refreshHistory();
            finish?.();
            break;
          }
          case "statementStarted":
          case "historyWarning":
            break;
        }
      };
      try {
        await call(commands.runNotebookCell(tab.id, id, sql, confirmed, runOptionsOf(tabNow() ?? tab), channel));
        // The command returns once the run started; Run all goes on only when it has ended.
        await finished;
      } catch (e) {
        // Refused before it started (the read-only barrier, the guard): nothing ran.
        const error = message(e);
        setRun(id, (run) => ({ kind: "failed", results: run?.results ?? [], messages: [], error }));
        return { kind: "failed", error };
      }
      if (cancelled) return { kind: "cancelled" };
      return failure ? { kind: "failed", error: failure } : { kind: "ok" };
    } finally {
      busy.current = false;
    }
  }

  /** Every SQL cell from the top, one after another, stopping at the first that does not
   *  succeed; then says what happened (ADR-087 point 14). */
  async function runAllCells() {
    if (running) return;
    stopAll.current = false;
    const sqlCells = cellsNow().filter((cell) => cell.kind === "sql" && cell.text.trim());
    const startedAt = performance.now();
    let ran = 0;
    let stopped: { cell: string; reason: Outcome } | null = null;
    for (const cell of sqlCells) {
      if (stopAll.current) { stopped = { cell: cell.name, reason: { kind: "cancelled" } }; break; }
      setRunAll({ kind: "running", cell: cell.name });
      const outcome = await runCell(cell.id);
      if (outcome.kind !== "notConnected") ran += 1;
      if (outcome.kind !== "ok") { stopped = { cell: cell.name, reason: outcome }; break; }
    }
    setRunAll({ kind: "ended", ran, total: sqlCells.length, stopped, elapsedMs: performance.now() - startedAt });
  }

  async function cancel() {
    stopAll.current = true;
    await call(commands.cancel(tab.id)).catch(() => undefined);
  }

  async function showExpanded(id: string) {
    if (expanded[id] !== undefined) { setExpanded(({ [id]: _closed, ...rest }) => rest); return; }
    const current = cellsNow();
    const index = current.findIndex((cell) => cell.id === id);
    if (!(await ensureConnected())) return;
    try {
      const sql = (await call(commands.expandNotebookCell(tab.id, notebookOf(tabNow() ?? tab, current), index))).sql;
      setExpanded((s) => ({ ...s, [id]: sql }));
    } catch (e) {
      setRun(id, (run) => ({ kind: "failed", results: run?.results ?? [], messages: [], error: message(e) }));
    }
  }

  async function exportHtml() {
    const current = cellsNow();
    const name = (tab.title ?? tab.file?.name.replace(/\.md$/i, "") ?? t("notebook.defaultName")).replace(/[\\/:*?"<>|]/g, "_");
    const path = await chooseSavePath({ title: t("notebook.exportTitle"), defaultPath: `${name}.html`, filters: [{ name: "HTML", extensions: ["html"] }] });
    if (!path) return;
    const outputs: CellOutputItem[] = current.map((cell) => {
      const run = runs[cell.id];
      if (cell.kind === "markdown" || !run || run.kind === "running") return { kind: "notRun" };
      if (run.kind === "failed") return { kind: "failed", error: run.error };
      return {
        kind: "ran",
        messages: run.messages,
        results: run.results.map((result) => {
          const svg = chartShown[cell.id] ? document.querySelector<SVGSVGElement>(`#${CSS.escape(`notebook-chart-${tab.id}-${result.id}`)} svg.chart-svg`) : null;
          return { result: result.id, chartSvg: svg ? standaloneSvg(svg) : null };
        }),
      };
    });
    try {
      await call(commands.exportNotebookHtml(path, tab.title ?? tab.file?.name ?? t("notebook.defaultName"), connection ? shownName(connection) : null, notebookOf(tab, current), outputs));
      setNotice({ text: t("notebook.exported", { path }), error: false });
    } catch (e) {
      setNotice({ text: t("notebook.exportFailed", { detail: message(e) }), error: true });
    }
  }

  const kind = connection?.kind ?? null;
  const totalSql = (cells ?? []).filter((cell) => cell.kind === "sql").length;

  return (
    <div className="notebook-tab" aria-label={t("notebook.label")}>
      <div className="toolbar">
        <select
          value={ownConnection ? connectionTag(ownConnection) : ""}
          onChange={(e) => void changeConnection(e.target.value)}
          aria-label={t("connection")}
          title={connection ? connectionTooltip(connection) : t("connection")}
          disabled={running || status === "connecting" || workspaceChanging}
        >
          <option value="">{t("noConnection")}</option>
          {connections.map((c) => (
            <option key={connectionTag(keyOf(c))} value={connectionTag(keyOf(c))} title={connectionTooltip(c)}>
              {shownName(c)} ({c.engine}{c.kind ? `, ${c.kind.name}` : c.env ? `, ${c.env}` : ""})
            </option>
          ))}
        </select>
        {lost ? (
          <span className="status status-lost" role="alert" title={lost.reason}>
            {t("status.lost")}
            <button className="ghost small" disabled={workspaceChanging} onClick={() => void reconnect()}>{t("reconnect")}</button>
          </span>
        ) : (
          <span className={`status status-${status}`}>{t(`status.${status}`)}{version ? ` · ${version}` : ""}</span>
        )}
        {kind && <TypeBadge type={kind} />}
        {status === "connected" && <TransactionControls tab={tab} running={running} onNotice={setNotice} />}
        {notice && <span className={`small format-error ${notice.error ? "error" : "muted"}`} role={notice.error ? "alert" : "status"}>{notice.text}</span>}
        <div className="spacer" />
      </div>
      <div className="toolbar notebook-actions">
        {running ? (
          <button onClick={() => void cancel()}>{t("notebook.cancelRun")}</button>
        ) : (
          <button className="primary" disabled={!cells || totalSql === 0 || workspaceChanging} onClick={() => void runAllCells()}>{t("notebook.runAll")}</button>
        )}
        <button className="ghost" disabled={!cells} onClick={() => void addCell("sql")} aria-label={t("notebook.addSql")} title={t("notebook.addSql")}><Icon name="plus" /> {t("notebook.sqlCell")}</button>
        <button className="ghost" disabled={!cells} onClick={() => void addCell("markdown")} aria-label={t("notebook.addMarkdown")} title={t("notebook.addMarkdown")}><Icon name="plus" /> {t("notebook.markdownCell")}</button>
        <button className="ghost" disabled={!cells || running} onClick={() => void exportHtml()}>{t("notebook.export")}</button>
        <div className="spacer" />
        <span className="small muted notebook-run-status" role="status">
          {runAll.kind === "running" ? t("notebook.runningCell", { name: runAll.cell }) : ""}
        </span>
      </div>
      {tab.restoreNotes && tab.restoreNotes.length > 0 && (
        <div className="banner restore-notes" role="status">
          <ul>
            {tab.restoreNotes.map((note, i) => (
              <li key={i}>
                {note.kind === "connectionMissing"
                  ? t("notebook.connectionMissing", { connection: note.connection })
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
      {notebook.problem && (
        <div className="banner banner-error" role="alert">
          <span>{cells ? t("notebook.writeProblem", { detail: notebook.problem }) : t("notebook.readProblem", { detail: notebook.problem })}</span>
        </div>
      )}
      {runAll.kind === "ended" && (
        <div className={`banner notebook-summary${runAll.stopped ? " banner-error" : ""}`} role="status" aria-label={t("notebook.summaryLabel")}>
          <span>
            {runAll.stopped
              ? t("notebook.summaryStopped", { ran: runAll.ran, total: runAll.total, cell: runAll.stopped.cell, reason: outcomeText(runAll.stopped.reason, t), notRun: runAll.total - runAll.ran, seconds: (runAll.elapsedMs / 1000).toFixed(1) })
              : t("notebook.summaryDone", { ran: runAll.ran, total: runAll.total, seconds: (runAll.elapsedMs / 1000).toFixed(1) })}
          </span>
          <button className="ghost small" onClick={() => setRunAll({ kind: "idle" })}>{t("restore.dismiss")}</button>
        </div>
      )}
      <div className="notebook-cells">
        {cells === null && !notebook.problem && <div className="empty">{t("notebook.reading")}</div>}
        {cells === null && notebook.problem && <pre className="notebook-raw">{tab.sql}</pre>}
        {cells?.length === 0 && <div className="empty">{t("notebook.empty")}</div>}
        {cells?.map((cell, index) => (
          <section
            key={cell.id}
            className={`notebook-cell notebook-cell-${cell.kind}`}
            data-cell={cell.kind === "sql" ? cell.name : undefined}
            aria-label={cell.kind === "sql" ? t("notebook.sqlCellLabel", { name: cell.name }) : t("notebook.markdownCellLabel", { number: index + 1 })}
            onContextMenu={(e) => { e.preventDefault(); setMenu({ cell: cell.id, x: e.clientX, y: e.clientY }); }}
            onKeyDown={(e) => {
              if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
                e.preventDefault();
                const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
                setMenu({ cell: cell.id, x: rect.left + 24, y: rect.top + 24 });
              }
            }}
          >
            {cell.kind === "sql" ? (
              <SqlCell
                tab={tab}
                cell={cell}
                index={index}
                run={runs[cell.id]}
                running={running}
                expanded={expanded[cell.id]}
                shownResult={shown[cell.id]}
                onShowResult={(result) => setShown((s) => ({ ...s, [cell.id]: result }))}
                onChange={(text) => updateCell(cell.id, { text })}
                onRename={(name) => updateCell(cell.id, { name })}
                onRun={() => void runCell(cell.id)}
                onMenu={(x, y) => setMenu({ cell: cell.id, x, y })}
                charts={charts}
                chartShown={!!chartShown[cell.id]}
                onChart={(result, settings) => setCharts((s) => ({ ...s, [result]: settings }))}
                onChartShown={(on) => setChartShown((s) => ({ ...s, [cell.id]: on }))}
                onResults={(update) => setRun(cell.id, (run) => (run ? { ...run, results: update(run.results) } : run!))}
                viewerOpen={viewerOpen}
                onViewerOpen={setViewerOpen}
                viewerWidth={viewerWidth}
                onViewerWidth={setViewerWidth}
              />
            ) : (
              <MarkdownCell cell={cell} onChange={(text) => updateCell(cell.id, { text })} onMenu={(x, y) => setMenu({ cell: cell.id, x, y })} />
            )}
          </section>
        ))}
      </div>
      {menu && (
        <CellMenu
          x={menu.x}
          y={menu.y}
          cell={cells?.find((c) => c.id === menu.cell) ?? null}
          first={cells?.[0]?.id === menu.cell}
          last={cells?.[cells.length - 1]?.id === menu.cell}
          running={running}
          expanded={expanded[menu.cell] !== undefined}
          onClose={() => setMenu(null)}
          onAction={(action) => {
            const id = menu.cell;
            setMenu(null);
            switch (action) {
              case "run": void runCell(id); break;
              case "showSql": void showExpanded(id); break;
              case "addSql": void addCell("sql", id); break;
              case "addMarkdown": void addCell("markdown", id); break;
              case "up": moveCell(id, -1); break;
              case "down": moveCell(id, 1); break;
              case "delete": void deleteCell(id); break;
            }
          }}
        />
      )}
    </div>
  );
}

function outcomeText(outcome: Outcome, t: (key: string, values?: Record<string, unknown>) => string): string {
  switch (outcome.kind) {
    case "ok": return "";
    case "failed": return outcome.error;
    case "declined": return t("notebook.declined");
    case "cancelled": return t("notebook.cancelled");
    case "notConnected": return t("notebook.notConnected");
  }
}

type MenuAction = "run" | "showSql" | "addSql" | "addMarkdown" | "up" | "down" | "delete";

/** A cell's menu (right click, the Menu key or Shift+F10, and its ☰ button): every action on it. */
function CellMenu({ x, y, cell, first, last, running, expanded, onClose, onAction }: { x: number; y: number; cell: NotebookCell | null; first: boolean; last: boolean; running: boolean; expanded: boolean; onClose(): void; onAction(action: MenuAction): void }) {
  const { t } = useTranslation("editor");
  const root = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });
  const close = useRef(onClose);
  close.current = onClose;
  useEffect(() => {
    const rect = root.current!.getBoundingClientRect();
    setPosition({ left: Math.max(0, Math.min(x, window.innerWidth - rect.width)), top: Math.max(0, Math.min(y, window.innerHeight - rect.height)) });
    root.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
    const outside = (event: MouseEvent) => { if (!root.current?.contains(event.target as Node)) close.current(); };
    window.addEventListener("mousedown", outside);
    return () => window.removeEventListener("mousedown", outside);
  }, [x, y]);
  if (!cell) return null;
  const item = (action: MenuAction, label: string, disabled = false, danger = false) => (
    <button role="menuitem" className={danger ? "danger-item" : undefined} disabled={disabled} onClick={() => onAction(action)}>{label}</button>
  );
  return createPortal(
    <div
      ref={root}
      className="grid-menu tab-menu editor-menu notebook-cell-menu"
      role="menu"
      aria-label={t("notebook.cellMenu")}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onKeyDown={(e) => {
        if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onClose(); return; }
        if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
        e.preventDefault();
        const items = [...(root.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
        const at = items.indexOf(document.activeElement as HTMLButtonElement);
        items[(at + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length]?.focus();
      }}
    >
      {cell.kind === "sql" && <>{item("run", t("notebook.runCell"), running)}{item("showSql", t(expanded ? "notebook.hideSql" : "notebook.showSql"), running)}<hr /></>}
      {item("addSql", t("notebook.addSqlBelow"))}
      {item("addMarkdown", t("notebook.addMarkdownBelow"))}
      <hr />
      {item("up", t("notebook.moveUp"), first)}
      {item("down", t("notebook.moveDown"), last)}
      <hr />
      {item("delete", t("notebook.deleteCell"), running && cell.kind === "sql", true)}
    </div>,
    document.body,
  );
}

function SqlCell(props: {
  tab: Tab;
  cell: NotebookCell;
  index: number;
  run: CellRun | undefined;
  running: boolean;
  expanded: string | null | undefined;
  shownResult: number | undefined;
  onShowResult(result: number): void;
  onChange(text: string): void;
  onRename(name: string): void;
  onRun(): void;
  onMenu(x: number, y: number): void;
  charts: Record<number, ChartSettings | null>;
  chartShown: boolean;
  onChart(result: number, settings: ChartSettings | null): void;
  onChartShown(on: boolean): void;
  onResults(update: (results: ResultMeta[]) => ResultMeta[]): void;
  viewerOpen: boolean;
  onViewerOpen(open: boolean): void;
  viewerWidth: number;
  onViewerWidth(width: number): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const { tab, cell, index, run } = props;
  const [renaming, setRenaming] = useState<{ value: string; error: string | null } | null>(null);
  const menuButton = useRef<HTMLButtonElement>(null);
  const references = useMemo(() => notebookCellsReferenced(cell.text), [cell.text]);
  const results = run?.results ?? [];
  const active = results.find((r) => r.id === props.shownResult) ?? results[results.length - 1];

  async function rename(value: string) {
    const name = value.trim();
    if (name === cell.name) { setRenaming(null); return; }
    const current = useApp.getState().tabs.find((item) => item.id === tab.id);
    try {
      await call(commands.checkNotebookCellName(notebookOf(current ?? tab, current?.notebook?.cells ?? []), index, name));
      props.onRename(name);
      setRenaming(null);
    } catch (e) {
      setRenaming({ value, error: message(e) });
    }
  }

  return (
    <>
      <header className="notebook-cell-header">
        {renaming ? (
          <span className="notebook-rename">
            <input
              autoFocus
              aria-label={t("notebook.cellName")}
              value={renaming.value}
              aria-invalid={renaming.error !== null}
              onChange={(e) => setRenaming({ value: e.target.value, error: null })}
              onKeyDown={(e) => {
                if (e.key === "Enter") { e.preventDefault(); void rename(renaming.value); }
                if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); setRenaming(null); }
              }}
              onBlur={() => void rename(renaming.value)}
            />
            {renaming.error && <span className="small error" role="alert">{renaming.error}</span>}
          </span>
        ) : (
          <button className="ghost notebook-cell-name" title={t("notebook.renameCell")} onClick={() => setRenaming({ value: cell.name, error: null })}>{cell.name}</button>
        )}
        {references.length > 0 && (
          <span className="small muted notebook-references" title={t("notebook.referencesHint")}>
            {t("notebook.references", { names: references.join(", ") })}
          </span>
        )}
        <div className="spacer" />
        {active && (
          <button
            className="ghost"
            aria-pressed={props.chartShown}
            aria-label={t(props.chartShown ? "notebook.showGrid" : "notebook.showChart")}
            title={t(props.chartShown ? "notebook.showGrid" : "notebook.showChart")}
            onClick={() => {
              props.onChartShown(!props.chartShown);
            }}
          >
            <Icon name={props.chartShown ? "table" : "chart"} />
          </button>
        )}
        <button className="ghost" aria-label={t("notebook.runCellHint", { name: cell.name })} title={t("notebook.runCellHint", { name: cell.name })} disabled={props.running} onClick={props.onRun}>
          <Icon name="run" />
        </button>
        <button
          ref={menuButton}
          className="ghost"
          aria-label={t("notebook.cellMenu")}
          title={t("notebook.cellMenu")}
          aria-haspopup="menu"
          onClick={() => { const rect = menuButton.current!.getBoundingClientRect(); props.onMenu(rect.right - 200, rect.bottom); }}
        >
          <Icon name="menu" />
        </button>
      </header>
      <CellEditor value={cell.text} onChange={props.onChange} onRun={props.onRun} label={t("notebook.sqlCellLabel", { name: cell.name })} />
      {props.expanded !== undefined && props.expanded !== null && (
        <div className="notebook-expanded">
          <span className="small muted">{t("notebook.expandedHint")}</span>
          <pre>{props.expanded}</pre>
        </div>
      )}
      {run && (
        <div className="notebook-output" aria-live="polite">
          {run.kind === "running" && <div className="small muted" role="status">{t("notebook.running")}</div>}
          {run.kind === "failed" && <pre className="error-box">{run.error}</pre>}
          {run.messages.length > 0 && results.length === 0 && run.kind !== "running" && (
            <div className="messages">{run.messages.map((text, i) => <div key={i} className="message message-info">{text}</div>)}</div>
          )}
          {results.length > 1 && (
            <div className="notebook-result-tabs" role="tablist">
              {results.map((result, i) => (
                <button key={result.id} role="tab" aria-selected={result.id === active?.id} className={result.id === active?.id ? "active" : undefined} onClick={() => props.onShowResult(result.id)}>
                  {result.name ?? t("notebook.resultName", { number: i + 1 })}
                </button>
              ))}
            </div>
          )}
          {active && (
            <div className="notebook-result">
              {props.chartShown ? (
                <div id={`notebook-chart-${tab.id}-${active.id}`} className="notebook-chart">
                  <ResultChart key={`chart-${active.id}`} result={active} settings={props.charts[active.id] !== undefined ? props.charts[active.id]! : defaultChart(active.columns)} onSettings={(settings) => props.onChart(active.id, settings)} />
                </div>
              ) : (
                <ResultGrid
                  key={active.id}
                  result={active}
                  session={tab.id}
                  onSorted={(sort) => props.onResults((list) => list.map((r) => (r.id === active.id ? { ...r, sort } : r)))}
                  onColumnFiltered={(columnFilters, rows, total, filterMode) => props.onResults((list) => list.map((r) => (r.id === active.id ? { ...r, columnFilters, rows, total, filterMode } : r)))}
                  viewerOpen={props.viewerOpen}
                  onViewerOpen={props.onViewerOpen}
                  viewerWidth={props.viewerWidth}
                  onViewerWidth={props.onViewerWidth}
                  pagingLocked={props.running}
                  onLoadMore={async () => 0}
                  onCountQuery={() => undefined}
                  onQueryPageSize={() => undefined}
                  onChart={(columns) => { props.onChart(active.id, defaultChart(active.columns, columns)); props.onChartShown(true); }}
                />
              )}
              <div className="statusline small muted">
                {t("notebook.rows", { count: active.rows, rows: numbers.format(active.rows) })}
                {run.kind === "done" ? ` · ${t("notebook.elapsed", { seconds: (run.elapsedMs / 1000).toFixed(2) })}` : ""}
              </div>
            </div>
          )}
        </div>
      )}
    </>
  );
}

/** A small CodeMirror editor for a SQL cell: the SQL editor's colors, Ctrl+Enter runs the cell. */
function CellEditor({ value, onChange, onRun, label }: { value: string; onChange(text: string): void; onRun(): void; label: string }) {
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const latest = useRef({ onChange, onRun });
  latest.current = { onChange, onRun };
  useEffect(() => {
    const editor = new EditorView({
      parent: host.current!,
      state: EditorState.create({
        doc: value,
        extensions: [
          highlightSpecialChars(), history(), drawSelection(), bracketMatching(), highlightActiveLine(),
          syntaxHighlighting(defaultHighlightStyle, { fallback: true }), syntaxHighlighting(sqlEditorColors),
          sqlLanguage(), sqlEditorTheme, EditorView.lineWrapping,
          EditorView.contentAttributes.of({ "aria-label": label }),
          keymap.of([{ key: RUN_CELL_KEY, preventDefault: true, run: () => { latest.current.onRun(); return true; } }, ...defaultKeymap, ...historyKeymap]),
          EditorView.updateListener.of((update) => { if (update.docChanged) latest.current.onChange(update.state.doc.toString()); }),
        ],
      }),
    });
    view.current = editor;
    return () => editor.destroy();
    // The editor is made once; later values arrive below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  useEffect(() => {
    const editor = view.current;
    if (editor && editor.state.doc.toString() !== value) editor.dispatch({ changes: { from: 0, to: editor.state.doc.length, insert: value } });
  }, [value]);
  return <div ref={host} className="notebook-editor" />;
}

/** A Markdown cell: drawn from the core's blocks as text (never HTML), edited as plain text. */
function MarkdownCell({ cell, onChange, onMenu }: { cell: NotebookCell; onChange(text: string): void; onMenu(x: number, y: number): void }) {
  const { t } = useTranslation("editor");
  const [editing, setEditing] = useState(cell.text === "");
  const [blocks, setBlocks] = useState<Block[]>([]);
  const menuButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (editing) return;
    let current = true;
    void call(commands.notebookMarkdown(cell.text)).then((read) => { if (current) setBlocks(read); }).catch(() => undefined);
    return () => { current = false; };
  }, [cell.text, editing]);
  return (
    <>
      <header className="notebook-cell-header notebook-markdown-header">
        <div className="spacer" />
        <button className="ghost" aria-pressed={editing} onClick={() => setEditing(!editing)}>{t(editing ? "notebook.doneEditing" : "notebook.editMarkdown")}</button>
        <button
          ref={menuButton}
          className="ghost"
          aria-label={t("notebook.cellMenu")}
          title={t("notebook.cellMenu")}
          aria-haspopup="menu"
          onClick={() => { const rect = menuButton.current!.getBoundingClientRect(); onMenu(rect.right - 200, rect.bottom); }}
        >
          <Icon name="menu" />
        </button>
      </header>
      {editing ? (
        <textarea
          className="notebook-markdown-source"
          aria-label={t("notebook.markdownSource")}
          autoFocus={cell.text === ""}
          value={cell.text}
          rows={Math.max(3, cell.text.split("\n").length + 1)}
          onChange={(e) => onChange(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Escape" || (e.key === "Enter" && (e.ctrlKey || e.metaKey))) { e.preventDefault(); e.stopPropagation(); setEditing(false); }
          }}
        />
      ) : (
        <div className="notebook-markdown" onDoubleClick={() => setEditing(true)}>
          {blocks.length === 0 ? <p className="muted">{t("notebook.emptyMarkdown")}</p> : <MarkdownBlocks blocks={blocks} />}
        </div>
      )}
    </>
  );
}

function MarkdownBlocks({ blocks }: { blocks: Block[] }) {
  return <>{blocks.map((block, i) => <MarkdownBlock key={i} block={block} />)}</>;
}

function MarkdownBlock({ block }: { block: Block }) {
  switch (block.kind) {
    case "heading": {
      const content = <Inlines inlines={block.content} />;
      switch (Math.min(6, Math.max(1, block.level))) {
        case 1: return <h1>{content}</h1>;
        case 2: return <h2>{content}</h2>;
        case 3: return <h3>{content}</h3>;
        case 4: return <h4>{content}</h4>;
        case 5: return <h5>{content}</h5>;
        default: return <h6>{content}</h6>;
      }
    }
    case "paragraph": return <p><Inlines inlines={block.content} /></p>;
    case "quote": return <blockquote><MarkdownBlocks blocks={block.blocks} /></blockquote>;
    case "list": {
      const items = block.items.map((item, i) => <li key={i}><Inlines inlines={item} /></li>);
      return block.ordered ? <ol start={block.start}>{items}</ol> : <ul>{items}</ul>;
    }
    case "code": return <pre><code>{block.text}</code></pre>;
    case "rule": return <hr />;
    case "table": {
      const align = (c: number) => {
        const value = block.align[c] ?? "none";
        return value === "none" ? undefined : { textAlign: value };
      };
      return (
        <table>
          <thead><tr>{block.header.map((cell, c) => <th key={c} style={align(c)}><Inlines inlines={cell} /></th>)}</tr></thead>
          <tbody>{block.rows.map((row, r) => <tr key={r}>{row.map((cell, c) => <td key={c} style={align(c)}><Inlines inlines={cell} /></td>)}</tr>)}</tbody>
        </table>
      );
    }
  }
}

/** Inline spans as React text. A link shows its address beside its text and never navigates;
 *  an image shows its alternative text and address and is never fetched (ADR-087 point 18). */
function Inlines({ inlines }: { inlines: Inline[] }) {
  return <>{inlines.map((inline, i) => {
    switch (inline.kind) {
      case "text": return <span key={i}>{inline.text}</span>;
      case "strong": return <strong key={i}><Inlines inlines={inline.content} /></strong>;
      case "emphasis": return <em key={i}><Inlines inlines={inline.content} /></em>;
      case "code": return <code key={i}>{inline.text}</code>;
      case "link": return <span key={i}><Inlines inlines={inline.content} /> <span className="muted notebook-address">({inline.url})</span></span>;
      case "image": return <span key={i}>[{inline.alt}] <span className="muted notebook-address">({inline.url})</span></span>;
      case "lineBreak": return <br key={i} />;
    }
  })}</>;
}
