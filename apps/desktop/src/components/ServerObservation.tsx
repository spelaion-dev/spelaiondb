import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type {
  LockItem_Serialize as LockItem,
  LockSnapshotItem_Serialize as LockSnapshotItem,
  ProcessItem_Serialize as ProcessItem,
  ServerSettingItem_Serialize as ServerSettingItem,
} from "../bindings";

/** Bytes in the largest unit that keeps the number readable (B, KB, MB, GB, TB). */
export function formatBytes(n: number, language: string): string {
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = n;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${new Intl.NumberFormat(language, { maximumFractionDigits: unit === 0 ? 0 : 1 }).format(value)} ${units[unit]}`;
}

/** What the process actions need to know, decided by the monitor (same rules as its table). */
export type ProcessActions = {
  disabled(row: ProcessItem, terminate: boolean): boolean;
  hints(row: ProcessItem): string[];
  act(row: ProcessItem, terminate: boolean): void;
};

type LockFilter = "all" | "waiting" | "involved";

/** The lock view: who blocks whom, the locks, and one session's details with its actions. */
export function LockView({ snapshot, processes, busy, actions }: {
  snapshot: LockSnapshotItem | null; processes: ProcessItem[]; busy: boolean; actions: ProcessActions;
}) {
  const { t } = useTranslation("shell");
  const [search, setSearch] = useState("");
  const [filter, setFilter] = useState<LockFilter>("all");
  const [selectedSession, setSelectedSession] = useState<string | null>(null);
  const [selectedLock, setSelectedLock] = useState<number | null>(null);
  const locks = snapshot?.locks ?? [];
  const waits = snapshot?.waits ?? [];
  const tree = snapshot?.tree ?? [];
  const process = (id: string) => processes.find((p) => p.id === id);
  const involved = useMemo(() => new Set(waits.flatMap((w) => [w.waiting, w.blocking])), [waits]);
  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return locks.map((lock, index) => ({ lock, index })).filter(({ lock }) => {
      if (filter === "waiting" && lock.granted) return false;
      if (filter === "involved" && !involved.has(lock.session)) return false;
      return !query || [lock.session, lock.object, lock.lockType, lock.mode, lock.detail].some((part) => part?.toLowerCase().includes(query));
    });
  }, [locks, search, filter, involved]);
  const blockedBy = (id: string) => waits.filter((w) => w.waiting === id).map((w) => w.blocking);
  const blocks = (id: string) => waits.filter((w) => w.blocking === id).map((w) => w.waiting);
  const lock: LockItem | undefined = selectedLock === null ? undefined : locks[selectedLock];
  const sessionId = lock?.session ?? selectedSession;
  const row = sessionId ? process(sessionId) : undefined;
  const waitingSeconds = (id: string) => locks.filter((l) => l.session === id && !l.granted).reduce<number | null>((max, l) => l.seconds === null ? max : Math.max(max ?? 0, l.seconds), null);

  function selectSession(id: string) { setSelectedSession(id); setSelectedLock(null); }

  return <>
    <div className="process-toolbar">
      <input type="search" aria-label={t("locks.search")} placeholder={t("locks.search")} value={search} onChange={(e) => setSearch(e.target.value)} />
      <select aria-label={t("locks.show")} value={filter} onChange={(e) => setFilter(e.target.value as LockFilter)}>
        <option value="all">{t("locks.allLocks")}</option><option value="waiting">{t("locks.waitingOnly")}</option><option value="involved">{t("locks.involvedOnly")}</option>
      </select>
    </div>
    <div className="process-content">
      <div className="process-table-wrap">
        <section className="lock-chains" aria-label={t("locks.chains")}>
          <div className="lock-section-title">{t("locks.chains")}</div>
          {tree.length === 0
            ? <div className="lock-chains-empty muted">{snapshot ? t("locks.noBlocking") : ""}</div>
            : <ul className="lock-tree" aria-label={t("locks.chains")}>
              {tree.map((node, index) => {
                const info = process(node.session);
                const waited = waitingSeconds(node.session);
                return <li key={`${index}:${node.session}`} style={{ ["--lock-depth" as string]: node.depth }}>
                  <button type="button" className={`lock-node ${sessionId === node.session && !lock ? "active" : ""}`} aria-pressed={sessionId === node.session && !lock} onClick={() => selectSession(node.session)}>
                    <span className="lock-node-id">#{node.session}</span>
                    <span className={`lock-status ${node.depth === 0 ? "blocking" : "waiting"}`}>
                      {node.cycle ? t("locks.deadlock") : node.depth === 0 ? t("locks.blocksCount", { count: blocks(node.session).length }) : waited === null ? t("locks.waitingLabel") : t("locks.waitingFor", { seconds: waited })}
                    </span>
                    <span className="lock-node-who">{info ? `${info.user || "—"} @ ${info.host || "—"}` : ""}</span>
                    <span className="lock-node-sql">{info?.sql ?? info?.state ?? ""}</span>
                  </button>
                </li>;
              })}
            </ul>}
        </section>
        <div className="lock-section-title">{t("locks.locks")}</div>
        <table className="process-table">
          <thead><tr>{["session", "object", "type", "mode", "status", "seconds", "detail"].map((name) => <th key={name}>{t(`locks.${name}`)}</th>)}</tr></thead>
          <tbody>{filtered.map(({ lock: item, index }) => <tr key={index} tabIndex={0} aria-selected={selectedLock === index}
            onClick={() => { setSelectedLock(index); setSelectedSession(item.session); }}
            onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setSelectedLock(index); setSelectedSession(item.session); } }}>
            <td className="process-id">{item.session}</td><td>{item.object || "—"}</td><td>{item.lockType}</td><td className="lock-mode">{item.mode}</td>
            <td><span className={`lock-status ${item.granted ? "granted" : "waiting"}`}>{t(item.granted ? "locks.granted" : "locks.waitingLabel")}</span></td>
            <td className="process-seconds">{item.seconds ?? "—"}</td><td className="lock-detail">{item.detail ?? "—"}</td>
          </tr>)}</tbody>
        </table>
        {!busy && snapshot && filtered.length === 0 && <div className="empty">{locks.length === 0 ? t("locks.noLocks") : t("locks.noResults")}</div>}
      </div>
      <aside className="process-details" aria-label={t("locks.details")}>
        <div className="process-details-header">{t("locks.details")}{sessionId && <span className="muted">#{sessionId}</span>}</div>
        {sessionId ? <>
          {lock && <dl aria-label={t("locks.lock")}>
            <dt>{t("locks.object")}</dt><dd>{lock.object || "—"}</dd>
            <dt>{t("locks.type")}</dt><dd>{lock.lockType}</dd>
            <dt>{t("locks.mode")}</dt><dd className="lock-mode">{lock.mode}</dd>
            <dt>{t("locks.status")}</dt><dd>{t(lock.granted ? "locks.granted" : "locks.waitingLabel")}</dd>
            <dt>{t(lock.granted ? "locks.heldFor" : "locks.waitedFor")}</dt><dd>{lock.seconds === null ? "—" : t("locks.secondsValue", { seconds: lock.seconds })}</dd>
            {lock.detail && <><dt>{t("locks.detail")}</dt><dd>{lock.detail}</dd></>}
          </dl>}
          <dl aria-label={t("locks.session")}>
            <dt>{t("processMonitor.user")}</dt><dd>{row?.user || "—"}</dd>
            <dt>{t("processMonitor.host")}</dt><dd>{row?.host || "—"}</dd>
            <dt>{t("processMonitor.database")}</dt><dd>{row?.database ?? "—"}</dd>
            <dt>{t("processMonitor.state")}</dt><dd>{row ? row.state ?? row.command : "—"}</dd>
            <dt>{t("locks.blockedBy")}</dt><dd>{blockedBy(sessionId).map((id) => `#${id}`).join(", ") || "—"}</dd>
            <dt>{t("locks.blocking")}</dt><dd>{blocks(sessionId).map((id) => `#${id}`).join(", ") || "—"}</dd>
          </dl>
          <div className="process-sql-label">SQL</div><pre className="process-sql-detail">{row?.sql ?? t("processMonitor.noSql")}</pre>
          <div className="process-details-actions">
            {row ? <>
              <button disabled={actions.disabled(row, false)} onClick={() => actions.act(row, false)}>{t("processMonitor.cancelQuery")}</button>
              <button className="danger-text" disabled={actions.disabled(row, true)} onClick={() => actions.act(row, true)}>{t("processMonitor.endConnection")}</button>
              {actions.hints(row).map((hint) => <span key={hint} className="muted small">{hint}</span>)}
            </> : <span className="muted small">{t("locks.sessionGone")}</span>}
          </div>
        </> : <div className="empty small">{t("locks.selectRow")}</div>}
      </aside>
    </div>
    <footer className="process-footer">
      <span>{t("locks.count", { shown: filtered.length, total: locks.length })} · {t("locks.waitCount", { count: waits.length })}</span>
      <span>{t("locks.visibility")}</span>
    </footer>
  </>;
}

const KINDS = ["setting", "variable", "status", "property"] as const;

/** The server's settings, variables, status counters and properties, with search. */
export function SettingsView({ settings, busy }: { settings: ServerSettingItem[] | null; busy: boolean }) {
  const { t } = useTranslation("shell");
  const [search, setSearch] = useState("");
  const [kind, setKind] = useState("");
  const [selected, setSelected] = useState<string | null>(null);
  const rows = settings ?? [];
  const kinds = KINDS.filter((k) => rows.some((row) => row.kind === k));
  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return rows.filter((row) => (!kind || row.kind === kind)
      && (!query || [row.name, row.value, row.category, row.description, row.source].some((part) => part?.toLowerCase().includes(query))));
  }, [rows, search, kind]);
  const key = (row: ServerSettingItem) => `${row.kind}:${row.name}`;
  const current = filtered.find((row) => key(row) === selected) ?? null;
  const [copyError, setCopyError] = useState<string | null>(null);

  return <>
    <div className="process-toolbar">
      <input type="search" aria-label={t("serverSettings.search")} placeholder={t("serverSettings.search")} value={search} onChange={(e) => setSearch(e.target.value)} />
      {kinds.length > 1 && <select aria-label={t("serverSettings.kind")} value={kind} onChange={(e) => setKind(e.target.value)}>
        <option value="">{t("serverSettings.allKinds")}</option>
        {kinds.map((k) => <option key={k} value={k}>{t(`serverSettings.kinds.${k}`)}</option>)}
      </select>}
    </div>
    <div className="process-content">
      <div className="process-table-wrap">
        <table className="process-table settings-table">
          <thead><tr>{["name", "value", "kind", "category", "source"].map((name) => <th key={name}>{t(`serverSettings.${name}`)}</th>)}</tr></thead>
          <tbody>{filtered.map((row) => <tr key={key(row)} tabIndex={0} aria-selected={selected === key(row)} title={row.description ?? undefined}
            onClick={() => setSelected(key(row))} onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); setSelected(key(row)); } }}>
            <td className="settings-name">{row.name}</td><td className="settings-value">{row.value || "—"}</td><td>{t(`serverSettings.kinds.${row.kind}`)}</td><td>{row.category ?? "—"}</td><td>{row.source ?? "—"}</td>
          </tr>)}</tbody>
        </table>
        {!busy && settings && filtered.length === 0 && <div className="empty">{t("serverSettings.noResults")}</div>}
      </div>
      <aside className="process-details" aria-label={t("serverSettings.details")}>
        <div className="process-details-header">{t("serverSettings.details")}</div>
        {current ? <>
          <dl>
            <dt>{t("serverSettings.name")}</dt><dd className="settings-name">{current.name}</dd>
            <dt>{t("serverSettings.kind")}</dt><dd>{t(`serverSettings.kinds.${current.kind}`)}</dd>
            <dt>{t("serverSettings.category")}</dt><dd>{current.category ?? "—"}</dd>
            <dt>{t("serverSettings.source")}</dt><dd>{current.source ?? "—"}</dd>
          </dl>
          {current.description && <p className="settings-description">{current.description}</p>}
          <div className="process-sql-label">{t("serverSettings.value")}</div><pre className="process-sql-detail">{current.value || "—"}</pre>
          <div className="process-details-actions">
            <button onClick={() => { setCopyError(null); void navigator.clipboard.writeText(current.value).catch((e: unknown) => setCopyError(e instanceof Error ? e.message : String(e))); }}>{t("serverSettings.copyValue")}</button>
            {copyError && <span className="muted small" role="alert">{copyError}</span>}
          </div>
        </> : <div className="empty small">{t("serverSettings.selectRow")}</div>}
      </aside>
    </div>
    <footer className="process-footer"><span>{t("serverSettings.count", { shown: filtered.length, total: rows.length })}</span><span>{t("serverSettings.readOnly")}</span></footer>
  </>;
}
