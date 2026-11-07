// The MCP page's Activity tab (ADR-033): what `spelaiondb mcp` recorded, as the core reads,
// masks, groups and exports it. Only the rows shown are in React state.

import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type AuditEventItem, type AuditSessionItem, type AuditStatusItem } from "../api";
import { chooseSavePath } from "../dialogs";

const PAGE = 500;
type Sub = "events" | "sessions";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function ActivityTab() {
  const { t } = useTranslation("mcp");
  const [sub, setSub] = useState<Sub>("events");
  const [session, setSession] = useState<AuditSessionItem | null>(null);
  return (
    <div className="mcp-activity">
      <div className="mcp-subtabs" role="tablist" aria-label={t("activity.views")}>
        {(["events", "sessions"] as Sub[]).map((name) => (
          <button
            key={name}
            role="tab"
            aria-selected={sub === name}
            onClick={() => {
              setSub(name);
              if (name === "sessions") setSession(null);
            }}
          >
            {t(`activity.${name}`)}
          </button>
        ))}
      </div>
      {sub === "events" && <Events session={session} onLeaveSession={() => setSession(null)} />}
      {sub === "sessions" && (
        <Sessions
          onOpen={(s) => {
            setSession(s);
            setSub("events");
          }}
        />
      )}
    </div>
  );
}

function Events({ session, onLeaveSession }: { session: AuditSessionItem | null; onLeaveSession(): void }) {
  const { t, i18n } = useTranslation("mcp");
  const dates = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "short", timeStyle: "medium" }), [i18n.language]);
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState("");
  const [tool, setTool] = useState("");
  const [status, setStatus] = useState<AuditStatusItem | "">("");
  const [events, setEvents] = useState<AuditEventItem[]>([]);
  const [total, setTotal] = useState(0);
  const [tools, setTools] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [refresh, setRefresh] = useState(0);
  const [confirmClear, setConfirmClear] = useState(false);

  useEffect(() => {
    const timer = window.setTimeout(() => setQuery(search.trim()), 200);
    return () => window.clearTimeout(timer);
  }, [search]);

  const filter = useMemo(
    () => ({ search: query, tool: tool || null, status: status || null, session: session?.session ?? null }),
    [query, tool, status, session],
  );

  useEffect(() => {
    let current = true;
    setLoading(true);
    setError(null);
    Promise.all([call(commands.auditEvents(filter, 0, PAGE)), call(commands.auditTools())])
      .then(([page, toolNames]) => {
        if (!current) return;
        setEvents(page.events);
        setTotal(page.total);
        setTools(toolNames);
      })
      .catch((e: unknown) => current && setError(message(e)))
      .finally(() => current && setLoading(false));
    return () => {
      current = false;
    };
  }, [filter, refresh]);

  const loadMore = useCallback(async () => {
    try {
      const page = await call(commands.auditEvents(filter, events.length, PAGE));
      setEvents((shown) => [...shown, ...page.events]);
      setTotal(page.total);
    } catch (e) {
      setError(message(e));
    }
  }, [filter, events.length]);

  async function exportAs(format: "csv" | "json") {
    setNotice(null);
    const path = await chooseSavePath({
      defaultPath: `spelaiondb-mcp-activity.${format}`,
      filters: [{ name: format.toUpperCase(), extensions: [format] }],
    });
    if (!path) return;
    try {
      const written = await call(commands.exportAudit(filter, format, path));
      setNotice(t("activity.exported", { count: written, path }));
    } catch (e) {
      setError(message(e));
    }
  }

  async function clear() {
    setConfirmClear(false);
    try {
      const removed = await call(commands.clearAudit());
      setNotice(t("activity.cleared", { count: removed }));
      onLeaveSession();
      setRefresh((n) => n + 1);
    } catch (e) {
      setError(message(e));
    }
  }

  return (
    <div className="mcp-events">
      <div className="mcp-activity-bar">
        <input
          type="search"
          aria-label={t("activity.search")}
          placeholder={t("activity.search")}
          value={search}
          onChange={(e) => setSearch(e.currentTarget.value)}
        />
        <select aria-label={t("activity.tool")} value={tool} onChange={(e) => setTool(e.currentTarget.value)}>
          <option value="">{t("activity.allTools")}</option>
          {tools.map((name) => (
            <option key={name} value={name}>{name}</option>
          ))}
        </select>
        <select aria-label={t("activity.status")} value={status} onChange={(e) => setStatus(e.currentTarget.value as AuditStatusItem | "")}>
          <option value="">{t("activity.allStatuses")}</option>
          {(["ok", "error", "refused", "cleared"] as AuditStatusItem[]).map((s) => (
            <option key={s} value={s}>{t(`activity.statusName.${s}`)}</option>
          ))}
        </select>
        <button onClick={() => setRefresh((n) => n + 1)}>{t("activity.refresh")}</button>
        <span className="muted mcp-count" role="status" aria-live="polite">
          {loading ? t("activity.loading") : t("activity.count", { count: total })}
        </span>
        <span className="mcp-activity-actions">
          <button onClick={() => void exportAs("csv")} disabled={total === 0}>{t("activity.exportCsv")}</button>
          <button onClick={() => void exportAs("json")} disabled={total === 0}>{t("activity.exportJson")}</button>
          <button className="danger" onClick={() => setConfirmClear(true)}>{t("activity.clear")}</button>
        </span>
      </div>
      {session && (
        <p className="mcp-session-filter">
          {t("activity.inSession", { client: session.client, start: dates.format(new Date(session.startedMs)) })}{" "}
          <button className="link" onClick={onLeaveSession}>{t("activity.allSessions")}</button>
        </p>
      )}
      {error && <p className="error" role="alert">{error}</p>}
      {notice && <p className="mcp-notice">{notice}</p>}
      {!loading && events.length === 0 ? (
        <div className="mcp-empty">
          <p>{query || tool || status || session ? t("activity.noMatch") : t("activity.empty")}</p>
        </div>
      ) : (
        <table className="mcp-events-table" aria-label={t("activity.events")}>
          <thead>
            <tr>
              <th>{t("activity.time")}</th>
              <th>{t("activity.connection")}</th>
              <th>{t("activity.tool")}</th>
              <th>{t("activity.statement")}</th>
              <th>{t("activity.status")}</th>
              <th>{t("activity.duration")}</th>
            </tr>
          </thead>
          <tbody>
            {events.map((e) => (
              <tr key={e.id}>
                <td className="nowrap">{dates.format(new Date(e.atMs))}</td>
                <td>{e.connection}</td>
                <td>{e.tool ?? "—"}</td>
                <td>
                  <code className="mcp-statement">{e.statement}</code>
                  {e.detail && <div className="muted mcp-detail">{e.detail}</div>}
                </td>
                <td>
                  <span className={`badge mcp-status mcp-status-${e.status}`}>{t(`activity.statusName.${e.status}`)}</span>
                </td>
                <td className="nowrap">{e.durationMs === null ? "—" : t("activity.ms", { ms: e.durationMs })}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {events.length < total && (
        <button onClick={() => void loadMore()}>{t("activity.loadMore")}</button>
      )}
      {confirmClear && (
        <div className="modal-backdrop">
          <div className="modal" role="dialog" aria-modal="true" aria-labelledby="mcp-clear-title">
            <h2 id="mcp-clear-title">{t("activity.clearTitle")}</h2>
            <p>{t("activity.clearBody")}</p>
            <div className="modal-actions">
              <button onClick={() => setConfirmClear(false)}>{t("install.cancel")}</button>
              <button className="danger" onClick={() => void clear()}>{t("activity.clearConfirm")}</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

function Sessions({ onOpen }: { onOpen(session: AuditSessionItem): void }) {
  const { t, i18n } = useTranslation("mcp");
  const dates = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "short", timeStyle: "medium" }), [i18n.language]);
  const [sessions, setSessions] = useState<AuditSessionItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    call(commands.auditSessions()).then(setSessions, (e: unknown) => setError(message(e)));
  }, []);
  if (error) return <p className="error" role="alert">{error}</p>;
  if (!sessions) return <p className="muted">{t("activity.loading")}</p>;
  if (sessions.length === 0) return <div className="mcp-empty"><p>{t("activity.noSessions")}</p></div>;
  return (
    <table className="mcp-events-table" aria-label={t("activity.sessions")}>
      <thead>
        <tr>
          <th>{t("activity.client")}</th>
          <th>{t("activity.start")}</th>
          <th>{t("activity.end")}</th>
          <th>{t("activity.calls")}</th>
          <th />
        </tr>
      </thead>
      <tbody>
        {sessions.map((s) => (
          <tr key={s.session} data-session={s.session}>
            <td>{s.client}</td>
            <td className="nowrap">{dates.format(new Date(s.startedMs))}</td>
            <td className="nowrap">{dates.format(new Date(s.endedMs))}</td>
            <td>
              {t("activity.callCount", { count: s.calls })}
              {s.refused > 0 && <span className="muted"> · {t("activity.refusedCount", { count: s.refused })}</span>}
              {s.failed > 0 && <span className="muted"> · {t("activity.failedCount", { count: s.failed })}</span>}
            </td>
            <td>
              <button onClick={() => onOpen(s)}>{t("activity.openSession")}</button>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}
