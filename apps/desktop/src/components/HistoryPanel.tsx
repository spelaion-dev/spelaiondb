import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type HistoryItem } from "../api";
import type { HistoryFilterItem } from "../bindings";
import { chooseSavePath, sqlFileFilters } from "../dialogs";
import { useApp } from "../store";
import { Icon } from "./Icons";

const PAGE_SIZE = 50;
const errorText = (reason: unknown) => reason instanceof Error ? reason.message : String(reason);
let exportSequence = 1_000_000;

export function HistoryPanel() {
  const { t, i18n } = useTranslation("editor");
  const workspace = useApp((s) => s.workspace?.workspace ?? null);
  const revision = useApp((s) => s.historyRevision);
  const connections = useApp((s) => s.connections);
  const close = () => useApp.getState().setHistoryOpen(false);
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [status, setStatus] = useState<NonNullable<HistoryFilterItem["status"]> | "">("");
  const [database, setDatabase] = useState("all");
  const [databases, setDatabases] = useState<string[]>([]);
  const [from, setFrom] = useState("");
  const [until, setUntil] = useState("");
  const [pinned, setPinned] = useState(false);
  const [entries, setEntries] = useState<HistoryItem[]>([]);
  const [offset, setOffset] = useState(0);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [pinning, setPinning] = useState<number | null>(null);
  const [exportId, setExportId] = useState<number | null>(null);
  const generation = useRef(0);
  const throughId = useRef<number | null>(null);
  const activeExport = useRef<number | null>(null);
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const days = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "full" }), [i18n.language]);
  const times = useMemo(() => new Intl.DateTimeFormat(i18n.language, { timeStyle: "medium" }), [i18n.language]);
  const filter = useMemo<HistoryFilterItem>(() => ({ search: query, status: status || null, database: database === "all" ? null : database.slice(3), fromMs: from ? new Date(`${from}T00:00:00`).getTime() : null, untilMs: until ? new Date(`${until}T00:00:00`).getTime() : null, pinned, throughId: null }), [query, status, database, from, until, pinned]);
  const invalidRange = Boolean(from && until && from >= until);
  const busy = loading || search.trim() !== query;

  useEffect(() => { const timer = window.setTimeout(() => setQuery(search.trim()), 200); return () => window.clearTimeout(timer); }, [search]);
  useEffect(() => { setDatabase("all"); }, [workspace]);
  useEffect(() => () => {
    const id = activeExport.current;
    if (id !== null) void call(commands.cancelExport(id)).catch(() => undefined);
    activeExport.current = null;
  }, [workspace]);
  useEffect(() => { throughId.current = null; setOffset(0); setNotice(null); }, [filter, revision, workspace, refresh]);
  useEffect(() => {
    const current = ++generation.current;
    setLoading(true); setError(null); setEntries([]); setHasMore(false);
    if (invalidRange) { setLoading(false); return; }
    void Promise.all([
      call(commands.historyPage({ ...filter, throughId: throughId.current }, PAGE_SIZE, offset)),
      call(commands.historyDatabases()),
    ]).then(([page, choices]) => {
      if (current !== generation.current) return;
      throughId.current = page.throughId; setEntries(page.entries); setHasMore(page.hasMore); setDatabases(choices);
      if (!page.entries.length && offset > 0) setOffset(Math.max(0, offset - PAGE_SIZE));
    }).catch((reason: unknown) => { if (current === generation.current) setError(t("history.loadFailed", { reason: errorText(reason) })); })
      .finally(() => { if (current === generation.current) setLoading(false); });
    return () => { generation.current += 1; };
  }, [filter, offset, revision, workspace, refresh, invalidRange]);

  function open(entry: HistoryItem) {
    useApp.getState().addTab(connections.find((connection) => connection.id === entry.connection)?.id ?? null, entry.statement);
    close();
  }
  async function pin(entry: HistoryItem) {
    setPinning(entry.id); setError(null);
    try { await call(commands.pinHistory(entry.id, !entry.pinned)); setRefresh((value) => value + 1); }
    catch (reason) { setError(t("history.pinFailed", { reason: errorText(reason) })); } finally { setPinning(null); }
  }
  async function exportSql() {
    setError(null); setNotice(null);
    const startedGeneration = generation.current;
    try {
      const path = await chooseSavePath({ title: t("history.export"), defaultPath: "query-history.sql", filters: sqlFileFilters(t("files.sqlFiles"), t("files.allFiles")) });
      if (!path) return;
      const id = ++exportSequence; activeExport.current = id; setExportId(id);
      const count = await call(commands.exportHistory(id, { ...filter, throughId: throughId.current }, path));
      if (activeExport.current === id) setNotice(t("history.exported", { count, path }));
    } catch (reason) { if (generation.current === startedGeneration) setError(t("history.exportFailed", { reason: errorText(reason) })); } finally { activeExport.current = null; setExportId(null); }
  }
  const dayKey = (entry: HistoryItem) => new Date(entry.atMs).toDateString();
  return (
    <aside className="history-panel library-panel" id="query-history" aria-label={t("history.title")} onKeyDown={(event) => { if (event.key === "Escape" && exportId === null) close(); }}>
      <div className="history-header"><h2>{t("history.title")}</h2><button className="ghost" onClick={() => setRefresh((value) => value + 1)} disabled={busy}>{t("history.refresh")}</button><button className="ghost icon-button" onClick={close} aria-label={t("history.close")} title={t("history.close")}><Icon name="x" /></button></div>
      <div className="history-search library-filters">
        <input autoFocus type="text" value={search} onChange={(event) => setSearch(event.target.value)} placeholder={t("history.searchPlaceholder")} aria-label={t("history.searchLabel")} />
        <div className="library-filter-row"><label>{t("history.status")}<select aria-label={t("history.status")} value={status} onChange={(event) => setStatus(event.target.value as typeof status)}><option value="">{t("history.allStatuses")}</option><option value="success">{t("history.ok")}</option><option value="error">{t("history.error")}</option><option value="cancelled">{t("history.cancelled")}</option></select></label><label>{t("history.database")}<select aria-label={t("history.database")} value={database} onChange={(event) => setDatabase(event.target.value)}><option value="all">{t("history.allDatabases")}</option>{databases.map((name) => <option key={name} value={`db:${name}`}>{name || t("history.unknownDatabase")}</option>)}</select></label></div>
        <details className="library-advanced"><summary>{t("history.timeAndPins")}</summary><div className="library-filter-row"><label>{t("history.from")}<input type="date" aria-label={t("history.from")} value={from} onChange={(event) => setFrom(event.target.value)} /></label><label>{t("history.until")}<input type="date" aria-label={t("history.until")} value={until} onChange={(event) => setUntil(event.target.value)} /></label></div><label className="check"><input type="checkbox" checked={pinned} onChange={(event) => setPinned(event.target.checked)} />{t("history.pinnedOnly")}</label></details>
        {invalidRange && <p className="error" role="alert">{t("history.invalidRange")}</p>}<p className="muted small">{t("history.privacyHelp")}</p>
      </div>
      <div className="library-toolbar">{exportId === null ? <button onClick={() => void exportSql()} disabled={busy || invalidRange || entries.length === 0}>{t("history.export")}</button> : <><span role="status">{t("history.exporting")}</span><button onClick={() => void call(commands.cancelExport(exportId)).catch((reason: unknown) => setError(t("history.exportFailed", { reason: errorText(reason) })))}>{t("export.cancel")}</button></>}</div>
      {notice && <div className="banner" role="status">{notice}</div>}
      {error && <div className="history-fetch-error banner library-alert" role="alert"><p>{error}</p><button onClick={() => setRefresh((value) => value + 1)} disabled={busy}>{t("history.retry")}</button></div>}
      <div className="history-entries" aria-busy={busy}>
        {!busy && !error && !invalidRange && entries.length === 0 && <div className="empty">{t(query || status || database !== "all" || from || until || pinned ? "history.noMatches" : "history.empty")}</div>}
        {entries.map((entry, index) => <Fragment key={entry.id}>
          {(index === 0 || dayKey(entries[index - 1]!) !== dayKey(entry)) && <h3 className="library-date">{days.format(entry.atMs)}</h3>}
          <article className="history-entry"><div className="history-entry-heading"><span className="history-connection" title={entry.connection}>{connections.find((connection) => connection.id === entry.connection)?.name ?? entry.connection}</span><span className={`library-status ${entry.cancelled ? "cancelled" : entry.error !== null ? "failed" : "succeeded"}`}>{t(entry.cancelled ? "history.cancelled" : entry.error !== null ? "history.error" : "history.ok")}</span></div>
            <div className="history-meta muted small"><span title={entry.database ?? undefined}>{entry.database ?? t("history.unknownDatabase")}</span><time dateTime={new Date(entry.atMs).toISOString()}>{times.format(entry.atMs)}</time><span>{t("history.duration", { duration: numbers.format(entry.durationMs) })}</span><span>{t("history.rowsReturned", { count: entry.rows, formattedCount: numbers.format(entry.rows) })}</span></div>
            <pre className="history-sql">{entry.statement}</pre>{(entry.cancelled || entry.error !== null) && <details className="library-details"><summary>{t("history.details")}</summary><pre className="history-error">{entry.cancelled ? t("messages.cancelled") : entry.error}</pre></details>}
            {!connections.some((connection) => connection.id === entry.connection) && <p className="muted small">{t("history.connectionUnavailable")}</p>}
            <div className="library-row-actions"><button onClick={() => open(entry)}>{t("history.open")}</button><button className="ghost library-pin" aria-pressed={entry.pinned} disabled={pinning !== null || busy} onClick={() => void pin(entry)}><Icon name="pin" />{t(entry.pinned ? "history.unpin" : "history.pin")}</button></div>
          </article>
        </Fragment>)}
        {busy && <div className="empty small" role="status">{t("history.loading")}</div>}
      </div>
      <div className="library-paging"><button disabled={busy || offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}>{t("history.previous")}</button><span className="muted small">{t("history.page", { page: Math.floor(offset / PAGE_SIZE) + 1 })}</span><button disabled={busy || !hasMore} onClick={() => setOffset(offset + PAGE_SIZE)}>{t("history.next")}</button></div>
    </aside>
  );
}
