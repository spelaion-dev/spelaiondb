import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type HistoryItem } from "../api";
import { useApp } from "../store";

const PAGE_SIZE = 100;

export function HistoryPanel() {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const dates = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "short", timeStyle: "medium" }), [i18n.language]);
  const workspace = useApp((s) => s.workspace?.workspace ?? null);
  const revision = useApp((s) => s.historyRevision);
  const connections = useApp((s) => s.connections);
  const addTab = useApp((s) => s.addTab);
  const setHistoryOpen = useApp((s) => s.setHistoryOpen);
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [settledQuery, setSettledQuery] = useState<string | null>(null);
  const [entries, setEntries] = useState<HistoryItem[]>([]);
  const [hasMore, setHasMore] = useState(false);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const generation = useRef(0);
  const busy = loading || search.trim() !== query || query !== settledQuery;

  useEffect(() => {
    const timer = window.setTimeout(() => setQuery(search.trim()), 200);
    return () => window.clearTimeout(timer);
  }, [search]);

  useEffect(() => {
    const current = ++generation.current;
    setLoading(true);
    setError(null);
    setEntries([]);
    setHasMore(false);
    void call(commands.listHistory(query, 0, PAGE_SIZE + 1))
      .then((items) => {
        if (current !== generation.current) return;
        setEntries(items.slice(0, PAGE_SIZE));
        setHasMore(items.length > PAGE_SIZE);
      })
      .catch((e: unknown) => {
        if (current === generation.current) setError(e instanceof Error ? e.message : String(e));
      })
      .finally(() => {
        if (current === generation.current) {
          setSettledQuery(query);
          setLoading(false);
        }
      });
    return () => { generation.current += 1; };
  }, [query, revision, workspace, refresh]);

  async function loadMore() {
    if (busy) return;
    const current = generation.current;
    setLoading(true);
    setError(null);
    try {
      const items = await call(commands.listHistory(query, entries.length, PAGE_SIZE + 1));
      if (current !== generation.current) return;
      setEntries((previous) => [...previous, ...items.slice(0, PAGE_SIZE)]);
      setHasMore(items.length > PAGE_SIZE);
    } catch (e) {
      if (current === generation.current) setError(e instanceof Error ? e.message : String(e));
    } finally {
      if (current === generation.current) setLoading(false);
    }
  }

  function reopen(entry: HistoryItem) {
    const connection = connections.find((c) => c.id === entry.connection);
    // A new tab starts disconnected. Opening history never connects or executes SQL.
    addTab(connection?.id ?? null, entry.statement);
    setHistoryOpen(false);
  }

  return (
    <aside className="history-panel" id="query-history" aria-label={t("history.title")}>
      <div className="history-header">
        <h2>{t("history.title")}</h2>
        <button className="ghost" onClick={() => setRefresh((value) => value + 1)} disabled={busy}>{t("history.refresh")}</button>
        <button className="ghost" onClick={() => setHistoryOpen(false)} aria-label={t("history.close")}>×</button>
      </div>
      <div className="history-search">
        <input
          type="text"
          autoFocus
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("history.searchPlaceholder")}
          aria-label={t("history.searchLabel")}
          onKeyDown={(e) => { if (e.key === "Escape") setHistoryOpen(false); }}
        />
        <p className="muted small">{t("history.privacyHelp")}</p>
      </div>
      <div className="history-entries" aria-busy={busy}>
        {!busy && !error && entries.length === 0 && <div className="empty">{t(query ? "history.noMatches" : "history.empty")}</div>}
        {entries.map((entry) => {
          const connection = connections.find((c) => c.id === entry.connection);
          return (
            <article className="history-entry" key={entry.id}>
              <div className="history-entry-heading">
                <span className="history-connection" title={entry.connection}>{connection?.name ?? entry.connection}</span>
                <span className={entry.error ? "error small" : "history-success small"}>{t(entry.error ? "history.error" : "history.ok")}</span>
              </div>
              <div className="history-meta muted small">
                <time dateTime={new Date(entry.atMs).toISOString()}>{dates.format(entry.atMs)}</time>
                <span>{t("history.duration", { duration: numbers.format(entry.durationMs) })}</span>
                <span>{t("history.rowsReturned", { count: entry.rows, formattedCount: numbers.format(entry.rows) })}</span>
              </div>
              <pre className="history-sql">{entry.statement}</pre>
              {entry.error && <pre className="history-error error">{entry.cancelled ? t("messages.cancelled") : entry.error}</pre>}
              {!connection && <p className="muted small">{t("history.connectionUnavailable")}</p>}
              <button onClick={() => reopen(entry)}>{t("history.open")}</button>
            </article>
          );
        })}
        {error && <div className="history-fetch-error" role="alert"><p className="error">{error}</p><button onClick={() => setRefresh((value) => value + 1)}>{t("history.retry")}</button></div>}
        {busy && <div className="empty small" role="status">{t("history.loading")}</div>}
        {hasMore && !busy && <div className="history-more"><button onClick={() => void loadMore()}>{t("history.loadMore")}</button></div>}
      </div>
    </aside>
  );
}
