import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import { useTranslation } from "react-i18next";
import { call, commands, type ConnectionItem } from "../api";
import type { ProcessItem_Serialize as ProcessItem, ProcessSnapshotItem_Serialize as ProcessSnapshotItem } from "../bindings";
import { openSession } from "../connect";
import { useApp } from "../store";
import { EngineIcon } from "./EngineIcon";

function supported(engine: string) { return ["mysql", "mariadb", "postgres", "sqlserver"].includes(engine); }
function message(error: unknown) { return error instanceof Error ? error.message : String(error); }

export function ProcessSidebar({ selected, onSelect }: { selected: string | null; onSelect(id: string): void }) {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  return <aside id="processes-sidebar" className="sidebar process-sidebar">
    <div className="sidebar-header"><div className="workspace-name">{t("processMonitor.title")}</div></div>
    <div className="tree">
      <div className="group-title">{t("connections")}</div>
      {connections.length === 0 && <div className="empty small">{t("processMonitor.noConnections")}</div>}
      {connections.map((connection) => <button key={connection.id} type="button" className={`process-connection ${connection.id === selected ? "active" : ""}`}
        disabled={!supported(connection.engine)} title={supported(connection.engine) ? connection.name : t("processMonitor.unsupported")}
        aria-pressed={connection.id === selected} onClick={() => onSelect(connection.id)}>
        <EngineIcon engine={connection.engine} /><span className="process-connection-name">{connection.name}</span>
        {!supported(connection.engine) && <span className="muted small">{t("processMonitor.soon")}</span>}
      </button>)}
    </div>
  </aside>;
}

export function ProcessMonitor({ connection, onOpenEditor }: { connection: ConnectionItem; onOpenEditor(): void }) {
  const { t, i18n } = useTranslation("shell");
  const [snapshot, setSnapshot] = useState<ProcessSnapshotItem | null>(null);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(true);
  const [attempt, setAttempt] = useState(0);
  const [intervalSeconds, setIntervalSeconds] = useState(0);
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState("all");
  const [database, setDatabase] = useState("");
  const [updated, setUpdated] = useState<Date | null>(null);
  const session = useRef<string | null>(null);
  const inFlight = useRef(false);
  const sequence = useRef(0);
  const translate = useRef(t);
  translate.current = t;

  const refresh = useCallback(async () => {
    const id = session.current;
    if (!id || inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    try {
      const next = await call(commands.listProcesses(id));
      if (session.current !== id) return;
      setSnapshot(next);
      setError(null);
      setUpdated(new Date());
    } catch (e) {
      if (session.current === id) setError(message(e));
    } finally {
      inFlight.current = false;
      if (session.current === id) setBusy(false);
    }
  }, []);

  useEffect(() => {
    const token = ++sequence.current;
    const id = `process:${connection.id}:${crypto.randomUUID()}`;
    let opened = false;
    let disposed = false;
    async function connect() {
      try {
        // Reuse credentials from an open explorer or query without returning them to the UI.
        const sources = [`meta:${connection.id}`, ...useApp.getState().tabs.filter((tab) => tab.connectionId === connection.id && tab.status === "connected").map((tab) => tab.id)];
        let version: string | null = null;
        for (const source of sources) {
          try { version = await call(commands.connectFromSession(source, id, null, false)); break; } catch { /* Try another open session. */ }
        }
        if (version === null && !disposed) version = await openSession(id, connection.id);
        if (version === null) {
          if (!disposed) { setBusy(false); setError(translate.current("processMonitor.cancelled")); }
          return;
        }
        opened = true;
        if (disposed || sequence.current !== token) {
          await call(commands.disconnect(id)).catch(() => undefined);
          return;
        }
        session.current = id;
        await refresh();
      } catch (e) {
        if (!disposed) { setBusy(false); setError(message(e)); }
      }
    }
    void connect();
    return () => {
      disposed = true;
      if (session.current === id) session.current = null;
      if (opened) void call(commands.disconnect(id)).catch(() => undefined);
    };
  }, [attempt, connection.id, refresh]);

  useEffect(() => {
    if (!intervalSeconds) return;
    const timer = window.setInterval(() => { if (document.visibilityState === "visible") void refresh(); }, intervalSeconds * 1000);
    return () => window.clearInterval(timer);
  }, [intervalSeconds, refresh]);

  const rows = snapshot?.processes ?? [];
  const databases = useMemo(() => [...new Set(rows.flatMap((row) => row.database ? [row.database] : []))].sort(), [rows]);
  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return rows.filter((row) => {
      if (filter === "active" && row.activity !== "active" && row.activity !== "waiting" || filter === "idle" && row.activity !== "idle" || filter === "waiting" && row.activity !== "waiting") return false;
      if (database === "none" ? row.database !== null : database.startsWith("db:") && row.database !== database.slice(3)) return false;
      return !query || [row.id, row.user, row.host, row.database, row.command, row.state, row.sql].some((part) => part?.toLowerCase().includes(query));
    }).sort((a, b) => Number(b.seconds ?? 0) - Number(a.seconds ?? 0));
  }, [rows, search, filter, database]);
  const selected = filtered.find((row) => row.id === selectedId) ?? null;
  const own = selected?.id === snapshot?.ownId;
  const ownDatabase = rows.find((row) => row.id === snapshot?.ownId)?.database;
  const otherPostgresDatabase = connection.engine === "postgres" && !!selected?.database && selected.database !== ownDatabase;
  const cannotVerify = (connection.engine === "postgres" || connection.engine === "sqlserver") && !selected?.identity;

  async function act(row: ProcessItem, terminate: boolean) {
    const id = session.current;
    if (!id || inFlight.current) return;
    const ok = await confirm(t(terminate ? "processMonitor.confirmEnd" : "processMonitor.confirmCancel", { id: row.id, user: row.user, host: row.host, connection: connection.name }), { kind: "warning" });
    if (!ok || session.current !== id || inFlight.current) return;
    inFlight.current = true;
    setBusy(true);
    try {
      await call(commands.controlProcess(id, { id: row.id, user: row.user, host: row.host, command: row.command, sql: row.sql, identity: row.identity }, terminate, true));
      if (session.current === id) setError(null);
    } catch (e) {
      if (session.current === id) setError(message(e));
    } finally {
      inFlight.current = false;
      if (session.current === id) { setBusy(false); void refresh(); }
    }
  }

  async function openSql(row: ProcessItem) {
    if (!row.sql) return;
    const id = session.current;
    const tab = useApp.getState().addTab(connection.id, row.sql);
    if (id) {
      useApp.getState().updateTab(tab, { status: "connecting" });
      try {
        // PostgreSQL sessions cannot switch databases after connecting. SQL Server and
        // MySQL can select the process's database when opening the query.
        const version = await call(commands.connectFromSession(id, tab, connection.engine === "postgres" ? null : row.database, false));
        const current = useApp.getState().tabs.find((item) => item.id === tab);
        if (!current || current.connectionId !== connection.id || current.status !== "connecting") { await call(commands.disconnect(tab)); return; }
        useApp.getState().updateTab(tab, { status: "connected", serverVersion: version });
      } catch {
        const current = useApp.getState().tabs.find((item) => item.id === tab);
        if (current?.connectionId === connection.id && current.status === "connecting") useApp.getState().updateTab(tab, { status: "disconnected" });
      }
    }
    if (session.current === id) onOpenEditor();
  }

  return <section className="process-monitor" aria-label={t("processMonitor.title")}>
    <header className="process-heading">
      <div><div className="process-eyebrow">{t("processMonitor.title")}</div><h1><EngineIcon engine={connection.engine} />{connection.name}</h1></div>
      <div className="process-heading-actions">
        <label className="process-refresh-label">{t("processMonitor.autoRefresh")}
          <select aria-label={t("processMonitor.autoRefresh")} value={intervalSeconds} onChange={(e) => setIntervalSeconds(Number(e.target.value))}>
            <option value={0}>{t("processMonitor.off")}</option><option value={5}>5 s</option><option value={10}>10 s</option><option value={30}>30 s</option>
          </select>
        </label>
        <button onClick={() => void refresh()} disabled={busy || !session.current} title={t("processMonitor.refresh")}><span aria-hidden="true">↻</span> {t("processMonitor.refresh")}</button>
      </div>
    </header>
    <div className="process-toolbar">
      <input type="search" aria-label={t("processMonitor.search")} placeholder={t("processMonitor.search")} value={search} onChange={(e) => setSearch(e.target.value)} />
      <select aria-label={t("processMonitor.state")} value={filter} onChange={(e) => setFilter(e.target.value)}>
        <option value="all">{t("processMonitor.allStates")}</option><option value="active">{t("processMonitor.active")}</option><option value="idle">{t("processMonitor.idle")}</option><option value="waiting">{t("processMonitor.waiting")}</option>
      </select>
      <select aria-label={t("processMonitor.database")} value={database} onChange={(e) => setDatabase(e.target.value)}>
        <option value="">{t("processMonitor.allDatabases")}</option><option value="none">{t("processMonitor.noDatabase")}</option>
        {databases.map((name) => <option key={name} value={`db:${name}`}>{name}</option>)}
      </select>
      <span className="spacer" />
      <span className="muted small" role="status">{busy ? t("processMonitor.loading") : updated ? t("processMonitor.updated", { time: updated.toLocaleTimeString(i18n.language) }) : ""}</span>
    </div>
    {error && <div className="banner banner-warn" role="alert">{error}{!session.current && <button onClick={() => { setError(null); setBusy(true); setAttempt((n) => n + 1); }}>{t("processMonitor.retry")}</button>}</div>}
    {snapshot?.limited && <div className="banner banner-warn" role="status">{t("processMonitor.limitedVisibility")}</div>}
    <div className="process-content">
      <div className="process-table-wrap">
        <table className="process-table">
          <thead><tr>{["id", "user", "host", "database", "command", "seconds", "state", "sql"].map((name) => <th key={name}>{t(`processMonitor.${name}`)}</th>)}</tr></thead>
          <tbody>{filtered.map((row) => <tr key={row.id} tabIndex={0} aria-selected={selectedId === row.id} onClick={() => setSelectedId(row.id)} onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setSelectedId(row.id); } }}>
            <td className="process-id">{row.id}{row.id === snapshot?.ownId && <span className="process-own" title={t("processMonitor.ownHint")}> {t("processMonitor.ownLabel")}</span>}</td><td>{row.user}</td><td>{row.host}</td><td>{row.database ?? "—"}</td><td><span className={`process-state ${row.activity === "idle" ? "idle" : "active"}`}>{row.command}</span></td><td className="process-seconds">{row.seconds ?? "—"}</td><td>{row.state ?? "—"}</td><td className="process-sql">{row.sql ?? "—"}</td>
          </tr>)}</tbody>
        </table>
        {!busy && snapshot && filtered.length === 0 && <div className="empty">{t("processMonitor.noResults")}</div>}
      </div>
      <aside className="process-details" aria-label={t("processMonitor.details")}>
        <div className="process-details-header">{t("processMonitor.details")}{selected && <span className="muted">#{selected.id}</span>}</div>
        {selected ? <>
          <dl><dt>{t("processMonitor.user")}</dt><dd>{selected.user || "—"}</dd><dt>{t("processMonitor.host")}</dt><dd>{selected.host || "—"}</dd><dt>{t("processMonitor.database")}</dt><dd>{selected.database ?? "—"}</dd><dt>{t("processMonitor.state")}</dt><dd>{selected.state ?? selected.command}</dd></dl>
          <div className="process-sql-label">SQL</div><pre className="process-sql-detail">{selected.sql ?? t("processMonitor.noSql")}</pre>
          <div className="process-details-actions">
            <button disabled={!selected.sql} onClick={() => { if (selected.sql) void navigator.clipboard.writeText(selected.sql).catch((e) => setError(message(e))); }}>{t("processMonitor.copySql")}</button>
            <button disabled={!selected.sql || otherPostgresDatabase} title={otherPostgresDatabase ? t("processMonitor.otherDatabase") : undefined} onClick={() => void openSql(selected)}>{t("processMonitor.openSql")}</button>
            <button title={connection.engine === "sqlserver" ? t("processMonitor.cancelUnavailable") : undefined} disabled={busy || own || connection.readOnly || cannotVerify || connection.engine === "sqlserver" || selected.activity === "idle" || selected.activity === "unknown"} onClick={() => void act(selected, false)}>{t("processMonitor.cancelQuery")}</button>
            <button className="danger-text" disabled={busy || own || connection.readOnly || cannotVerify} onClick={() => void act(selected, true)}>{t("processMonitor.endConnection")}</button>
            {own && <span className="muted small">{t("processMonitor.ownHint")}</span>}
            {connection.readOnly && <span className="muted small">{t("processMonitor.readOnly")}</span>}
            {cannotVerify && <span className="muted small">{t("processMonitor.cannotVerify")}</span>}
            {otherPostgresDatabase && <span className="muted small">{t("processMonitor.otherDatabase")}</span>}
            {connection.engine === "postgres" && selected.sql && <span className="muted small">{t("processMonitor.postgresSqlLimit")}</span>}
          </div>
        </> : <div className="empty small">{t("processMonitor.selectRow")}</div>}
      </aside>
    </div>
    <footer className="process-footer"><span>{t("processMonitor.count", { shown: filtered.length, total: rows.length })}</span><span>{t("processMonitor.visibility")}</span></footer>
  </section>;
}
