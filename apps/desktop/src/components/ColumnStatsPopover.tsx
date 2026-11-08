// Column statistics of a table data tab (the column header's menu): rows, distinct values,
// NULLs, minimum, maximum and the most frequent values, counted by the server. The core writes
// every statement (reads only, in the engine's read-only transaction, in a session of its own)
// and samples big tables; this popover asks, shows what came back and says it was read-only.
// A most frequent value adds the core's condition for it to the tab's filter.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ColumnStatsItem, type StatStateItem } from "../api";
import { Icon } from "./Icons";

/** Ids of statistics runs: far from the export dialog's, which share the core's cancel map. */
let nextStatsId = 1_000_000_000;

type State =
  | { kind: "running"; id: number }
  | { kind: "done"; stats: ColumnStatsItem }
  | { kind: "cancelled" }
  | { kind: "failed"; error: string };

const MARGIN = 8;

export function ColumnStatsPopover(props: {
  x: number;
  y: number;
  session: string;
  resultId: number;
  column: number;
  name: string;
  table: { schema: string; name: string; filter: string };
  /** A most frequent value was picked: the tab's filter with its condition. */
  onFilter(filter: string): void;
  onClose(): void;
}) {
  const { session, resultId, column, table, onClose } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = new Intl.NumberFormat(i18n.language);
  const box = useRef<HTMLDivElement>(null);
  const [state, setState] = useState<State | null>(null);
  const [run, setRun] = useState(0);
  const [filterError, setFilterError] = useState<string | null>(null);
  const runningId = useRef<number | null>(null);

  useEffect(() => {
    let active = true;
    const id = ++nextStatsId;
    runningId.current = id;
    setState({ kind: "running", id });
    call(commands.columnStats(session, id, table, resultId, column))
      .then((outcome) => active && setState(outcome.kind === "done" ? { kind: "done", stats: outcome.stats } : { kind: "cancelled" }))
      .catch((e: unknown) => active && setState({ kind: "failed", error: e instanceof Error ? e.message : String(e) }))
      .finally(() => {
        if (runningId.current === id) runningId.current = null;
      });
    return () => {
      active = false;
      // Closed while counting: stop the statement and close its session.
      if (runningId.current === id) void call(commands.cancelColumnStats(id)).catch(() => undefined);
    };
  }, [session, resultId, column, table.schema, table.name, table.filter, run]);

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    el.style.left = `${Math.max(MARGIN, Math.min(props.x, window.innerWidth - rect.width - MARGIN))}px`;
    el.style.top = `${Math.max(MARGIN, Math.min(props.y, window.innerHeight - rect.height - MARGIN))}px`;
  });

  useEffect(() => {
    box.current?.focus();
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && box.current?.contains(e.target as Node)) return;
      onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [onClose]);

  const count = (n: number) => numbers.format(n);
  const unavailable = (s: StatStateItem) =>
    s.kind === "failed" ? <span className="muted" title={s.message}>{t("columnStats.notAvailable")}</span> : <span className="muted">{t("columnStats.notForType")}</span>;
  const failures = (stats: Pick<ColumnStatsItem, "distinctState" | "rangeState" | "topState">) =>
    [stats.distinctState, stats.rangeState, stats.topState].flatMap((s) => (s.kind === "failed" ? [s.message] : []));

  async function filterBy(condition: string) {
    setFilterError(null);
    try {
      const next = await call(commands.addTableCondition(session, table.filter, condition));
      props.onFilter(next);
    } catch (e) {
      setFilterError(e instanceof Error ? e.message : String(e));
    }
  }

  const raw = state?.kind === "done" ? state.stats : null;
  // Counts travel as f64 (exact to 2^53); the generated type allows null for NaN.
  const stats = raw && {
    ...raw,
    rows: raw.rows ?? 0,
    sampleRows: raw.sampleRows ?? 0,
    nulls: raw.nulls ?? 0,
    top: raw.top.map((v) => ({ ...v, count: v.count ?? 0 })),
  };
  return (
    <div
      ref={box}
      className="column-stats"
      role="dialog"
      aria-label={t("columnStats.title", { column: props.name })}
      tabIndex={-1}
      style={{ left: props.x, top: props.y }}
    >
      <header className="column-stats-header">
        <div className="column-stats-title">
          <strong title={props.name}>{props.name}</strong>
          <span className="muted small" title={`${table.schema}.${table.name}`}>{table.name}</span>
        </div>
        <button className="ghost cell-viewer-close" aria-label={t("columnStats.close")} title={t("columnStats.close")} onClick={onClose}><Icon name="x" /></button>
      </header>
      <p className="column-stats-scope small muted">
        {t(table.filter.trim() ? "columnStats.scopeFiltered" : "columnStats.scope")}
        {stats?.sampled && <span className="column-stats-sampled">{t("columnStats.sampled")}</span>}
      </p>
      {state?.kind === "running" && (
        <div className="calc-busy" role="status">
          <span>{t("columnStats.counting")}</span>
          <button type="button" onClick={() => void call(commands.cancelColumnStats(state.id)).catch(() => undefined)}>{t("columnStats.cancel")}</button>
        </div>
      )}
      {state?.kind === "cancelled" && (
        <div className="calc-busy" role="status">
          <span>{t("columnStats.cancelled")}</span>
          <button type="button" className="ghost" onClick={() => setRun((n) => n + 1)}>{t("columnStats.again")}</button>
        </div>
      )}
      {state?.kind === "failed" && (
        <div className="column-stats-failed">
          <p className="error small" role="alert">{t("columnStats.failed", { detail: state.error })}</p>
          <button type="button" onClick={() => setRun((n) => n + 1)}>{t("columnStats.retry")}</button>
        </div>
      )}
      {stats && (
        <>
          <dl className="reference-fields calc-fields column-stats-fields">
            <div className="reference-field">
              <dt>{t("columnStats.rows")}</dt>
              <dd>{stats.sampled ? t("columnStats.sampleRows", { rows: count(stats.sampleRows) }) : count(stats.rows)}</dd>
            </div>
            <div className="reference-field"><dt>{t("columnStats.distinct")}</dt><dd>{stats.distinct === null ? unavailable(stats.distinctState) : count(stats.distinct)}</dd></div>
            <div className="reference-field"><dt>{t("columnStats.nulls")}</dt><dd>{count(stats.nulls)}</dd></div>
            <div className="reference-field">
              <dt>{t("columnStats.min")}</dt>
              <dd>{stats.rangeState.kind !== "done" ? unavailable(stats.rangeState) : stats.min === null ? <span className="muted">NULL</span> : <span className="calc-value" title={stats.min}>{stats.min}</span>}</dd>
            </div>
            <div className="reference-field">
              <dt>{t("columnStats.max")}</dt>
              <dd>{stats.rangeState.kind !== "done" ? unavailable(stats.rangeState) : stats.max === null ? <span className="muted">NULL</span> : <span className="calc-value" title={stats.max}>{stats.max}</span>}</dd>
            </div>
          </dl>
          <h3 className="reference-section column-stats-section">{t("columnStats.top")}</h3>
          {stats.topState.kind !== "done" ? (
            <p className="reference-note column-stats-note">{unavailable(stats.topState)}</p>
          ) : stats.top.length === 0 ? (
            <p className="reference-note column-stats-note">{t("columnStats.noRows")}</p>
          ) : (
            <ul className="column-stats-top" aria-label={t("columnStats.top")}>
              {stats.top.map((v, i) => {
                const share = stats.rows > 0 ? Math.round((v.count / stats.rows) * 100) : 0;
                const label = v.text ?? "NULL";
                const condition = v.condition;
                return (
                  <li key={i}>
                    <button
                      type="button"
                      className="ghost column-stats-value"
                      disabled={condition === null}
                      title={condition === null ? t("columnStats.tooLong") : t("columnStats.filterBy", { value: label })}
                      onClick={() => condition !== null && void filterBy(condition)}
                    >
                      <span className={v.text === null ? "muted column-stats-text" : "column-stats-text"}>{label}</span>
                      <span className="column-stats-count">{count(v.count)}</span>
                      <span className="column-stats-share muted">{t("columnStats.share", { percent: share })}</span>
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
          {filterError && <p className="error small column-stats-note" role="alert">{t("columnStats.filterFailed", { detail: filterError })}</p>}
          {failures(stats).length > 0 && (
            <ul className="column-stats-errors">
              {failures(stats).map((message, i) => <li key={i} className="small muted">{t("columnStats.refused", { detail: message })}</li>)}
            </ul>
          )}
          <details className="column-stats-sql">
            <summary className="small">{t("columnStats.statements", { count: stats.statements.length })}</summary>
            <pre>{stats.statements.join(";\n\n")}</pre>
          </details>
        </>
      )}
    </div>
  );
}
