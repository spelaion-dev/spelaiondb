// Calc panel of the result side panel: count, distinct, NULLs, sum, average, median, minimum,
// maximum and mode of the selected cells. The core computes them over the stored result
// (ADR-010): this panel sends only the selection's bounds and shows the numbers it gets back.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type CellRange, type SelectionStatsItem } from "../api";
import { Icon } from "./Icons";

/** How long a selection must stay before it is calculated (dragging and arrow keys go fast). */
const SETTLE_MS = 150;

/** "Calculating…" and Cancel show only for calculations longer than this. */
const BUSY_MS = 300;

type State =
  | { key: string; kind: "done"; stats: SelectionStatsItem }
  | { key: string; kind: "cancelled" }
  | { key: string; kind: "failed"; error: string };

export function CalcPanel(props: {
  resultId: number;
  /** The selection's bounds in the current view; `null` without a selection. */
  range: CellRange | null;
  /** Changes when the view's rows move (a sort or a filter), so the same bounds are new cells. */
  view: string;
  /** There is a selection but the result is still arriving: nothing is calculated yet. */
  waiting: boolean;
  modes: ReactNode;
  onClose(): void;
}) {
  const { resultId, range, view } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = new Intl.NumberFormat(i18n.language);
  const key = range ? `${resultId}|${view}|${range.firstRow}|${range.rows}|${range.columns.join(",")}` : "";
  const [state, setState] = useState<State | null>(null);
  const [busy, setBusy] = useState(false);
  // Bumped to calculate the same selection again after a cancel.
  const [again, setAgain] = useState(0);
  const running = useRef<string | null>(null);

  useEffect(() => {
    if (!range) return;
    let active = true;
    let busyTimer: ReturnType<typeof setTimeout> | undefined;
    const timer = setTimeout(() => {
      running.current = key;
      busyTimer = setTimeout(() => active && setBusy(true), BUSY_MS);
      call(commands.selectionStats(resultId, range))
        .then((stats) => {
          if (!active) return;
          // `null`: a newer calculation replaced this one, or Cancel stopped it.
          setState(stats ? { key, kind: "done", stats } : { key, kind: "cancelled" });
        })
        .catch((e: unknown) => active && setState({ key, kind: "failed", error: e instanceof Error ? e.message : String(e) }))
        .finally(() => {
          clearTimeout(busyTimer);
          if (running.current === key) running.current = null;
          if (active) setBusy(false);
        });
    }, SETTLE_MS);
    return () => {
      active = false;
      clearTimeout(timer);
      clearTimeout(busyTimer);
      setBusy(false);
    };
    // `range` is part of `key`.
  }, [key, again]);

  // Closing the panel stops a calculation still running.
  useEffect(() => () => {
    if (running.current !== null) void call(commands.cancelSelectionStats(resultId)).catch(() => undefined);
  }, [resultId]);

  const current = state?.key === key ? state : null;
  // While a new selection settles, the previous numbers stay, dimmed, instead of a blank panel.
  const stale = state?.kind === "done" && state.key !== key;
  const raw = state?.kind === "done" && (current || stale) ? state.stats : null;
  // Counts travel as f64 (exact to 2^53); the generated type allows null for NaN.
  const stats = raw && {
    ...raw,
    nulls: raw.nulls ?? 0,
    values: raw.values ?? 0,
    distinct: raw.distinct ?? 0,
    numbers: raw.numbers ?? 0,
    modeCount: raw.modeCount ?? 0,
  };
  const count = (n: number) => numbers.format(n);
  const dash = <span className="muted">—</span>;
  const value = (text: string | null | undefined, why?: string) =>
    text == null ? <span className="muted" title={why}>{why ?? "—"}</span> : <span className="calc-value" title={text}>{text}</span>;

  return (
    <aside className="cell-viewer calc-panel" aria-label={t("calc.title")} onKeyDown={(e) => e.key === "Escape" && props.onClose()}>
      <header>
        <div className="cell-viewer-heading">
          <div className="cell-viewer-title">
            <strong>{t("calc.title")}</strong>
            {range && (
              <span className="muted small">
                {t("calc.size", { rows: count(range.rows), columns: count(range.columns.length), count: range.rows * range.columns.length })}
              </span>
            )}
          </div>
          <button className="ghost cell-viewer-close" aria-label={t("calc.close")} title={t("calc.close")} onClick={props.onClose}><Icon name="x" /></button>
        </div>
        <div className="cell-viewer-controls">{props.modes}</div>
      </header>
      <div className="references-body calc-body" aria-busy={busy}>
        {!range && <p className="reference-note">{t(props.waiting ? "calc.waiting" : "calc.selectCells")}</p>}
        {range && busy && (
          <div className="calc-busy" role="status">
            <span>{t("calc.calculating")}</span>
            <button type="button" onClick={() => void call(commands.cancelSelectionStats(resultId)).catch(() => undefined)}>
              {t("calc.cancel")}
            </button>
          </div>
        )}
        {range && !busy && current?.kind === "cancelled" && (
          <div className="calc-busy" role="status">
            <span>{t("calc.cancelled")}</span>
            <button type="button" className="ghost" onClick={() => setAgain((n) => n + 1)}>{t("calc.again")}</button>
          </div>
        )}
        {range && current?.kind === "failed" && <p className="error small" role="alert">{t("calc.failed", { detail: current.error })}</p>}
        {range && stats && (
          <div className={stale ? "calc-stale" : undefined}>
            <dl className="reference-fields calc-fields" aria-label={t("calc.counts")}>
              <div className="reference-field"><dt>{t("calc.count")}</dt><dd>{count(stats.values)}</dd></div>
              <div className="reference-field">
                <dt>{t("calc.distinct")}</dt>
                <dd>{stats.distinctCapped ? t("calc.atLeast", { value: count(stats.distinct) }) : count(stats.distinct)}</dd>
              </div>
              <div className="reference-field"><dt>{t("calc.nulls")}</dt><dd>{count(stats.nulls)}</dd></div>
            </dl>
            <h3 className="reference-section">{t("calc.numbers")}</h3>
            {stats.numbers === 0 ? (
              <p className="reference-note calc-note">{t("calc.noNumbers")}</p>
            ) : (
              <dl className="reference-fields calc-fields" aria-label={t("calc.numbers")}>
                <div className="reference-field"><dt>{t("calc.sum")}</dt><dd>{value(stats.sum)}</dd></div>
                <div className="reference-field"><dt>{t("calc.avg")}</dt><dd>{value(stats.avg)}</dd></div>
                <div className="reference-field">
                  <dt>{t("calc.median")}</dt>
                  <dd>{stats.medianCapped ? value(null, t("calc.tooMany")) : value(stats.median)}</dd>
                </div>
              </dl>
            )}
            {stats.numbers > 0 && stats.numbers < stats.values && (
              <p className="reference-note calc-note">{t("calc.numbersOf", { numbers: count(stats.numbers), values: count(stats.values) })}</p>
            )}
            <h3 className="reference-section">{t("calc.values")}</h3>
            <dl className="reference-fields calc-fields" aria-label={t("calc.values")}>
              <div className="reference-field"><dt>{t("calc.min")}</dt><dd>{stats.values === 0 ? dash : value(stats.min, t("calc.mixed"))}</dd></div>
              <div className="reference-field"><dt>{t("calc.max")}</dt><dd>{stats.values === 0 ? dash : value(stats.max, t("calc.mixed"))}</dd></div>
              <div className="reference-field">
                <dt>{t("calc.mode")}</dt>
                <dd>
                  {stats.mode == null ? (
                    value(null, stats.values === 0 ? undefined : t(stats.distinctCapped ? "calc.tooMany" : "calc.noRepeats"))
                  ) : (
                    <>
                      {value(stats.mode)} <span className="muted calc-times">{t("calc.times", { count: stats.modeCount, formattedCount: count(stats.modeCount) })}</span>
                    </>
                  )}
                </dd>
              </div>
            </dl>
          </div>
        )}
      </div>
    </aside>
  );
}
