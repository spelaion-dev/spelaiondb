// Grouping panel of the result side panel (SDB-387): the rows of the grid's current view
// counted by the columns chosen, each group with its count and percent, the largest first.
// The core does the counting over the stored result (ADR-010): this panel sends the columns
// and shows the groups it gets back, never rows. A group filters the grid to its rows through
// the result's column filters, which the core writes from the group's own values.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ColumnFilterItem, type ColumnItem } from "../api";
import type { FilterModeItem, GroupingItem } from "../bindings";
import { keyText } from "../shortcuts";
import { Icon } from "./Icons";

/** How long the columns must stay before they are grouped. */
const SETTLE_MS = 150;

/** "Grouping…" and Cancel show only for groupings longer than this. */
const BUSY_MS = 300;

type State =
  // `columns` and `duplicates`: what this grouping was asked for. While a newer one is being
  // counted the old groups stay on screen, dimmed, and must still be described as what they are.
  | { key: string; kind: "done"; grouping: GroupingItem; columns: number[]; duplicates: boolean }
  | { key: string; kind: "cancelled" }
  | { key: string; kind: "failed"; error: string };

export function GroupPanel(props: {
  resultId: number;
  columns: ColumnItem[];
  /** The grouped columns, by position in the result, in the panel's order. */
  grouped: number[];
  onGrouped(columns: number[]): void;
  /** Changes when the view's rows move (a sort, a filter, more rows): the groups are counted again. */
  view: string;
  /** The result is still arriving: nothing is grouped yet. */
  waiting: boolean;
  /** The grid holds only part of the rows: a table's page, or the pages of a query loaded so far. */
  scope?: "page" | "loaded";
  /** A header is being dragged over the grid: the panel shows where to drop it. */
  dropping: boolean;
  /** The core filtered the grid to a group: the filters now in force and the rows left. */
  /** The filters now in force and how they are read (`mode`: the core may switch on Match
   *  case, when the group's conditions are the only ones, so the rows left are exactly its). */
  onFiltered(filters: ColumnFilterItem[], rows: number, total: number, mode: FilterModeItem): void;
  /** Filtering is not possible now (pending edits, a page loading). */
  locked: boolean;
  modes: ReactNode;
  onClose(): void;
}) {
  const { resultId, grouped, view } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = new Intl.NumberFormat(i18n.language);
  const percents = new Intl.NumberFormat(i18n.language, { style: "percent", maximumFractionDigits: 1 });
  const [duplicates, setDuplicates] = useState(false);
  const key = grouped.length ? `${resultId}|${view}|${grouped.join(",")}|${duplicates}` : "";
  const [state, setState] = useState<State | null>(null);
  const [busy, setBusy] = useState(false);
  const [again, setAgain] = useState(0);
  const [failure, setFailure] = useState<string | null>(null);
  const [caseSwitched, setCaseSwitched] = useState(false);
  const running = useRef<string | null>(null);

  useEffect(() => {
    if (!key || props.waiting) return;
    let active = true;
    let busyTimer: ReturnType<typeof setTimeout> | undefined;
    const timer = setTimeout(() => {
      running.current = key;
      busyTimer = setTimeout(() => active && setBusy(true), BUSY_MS);
      call(commands.groupResult(resultId, grouped, duplicates))
        .then((grouping) => {
          if (!active) return;
          // `null`: a newer grouping replaced this one, or Cancel stopped it.
          setState(grouping ? { key, kind: "done", grouping, columns: grouped, duplicates } : { key, kind: "cancelled" });
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
    // `grouped` and `duplicates` are part of `key`.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, again, props.waiting]);

  // Closing the panel stops a grouping still running.
  useEffect(() => () => {
    if (running.current !== null) void call(commands.cancelGrouping(resultId)).catch(() => undefined);
  }, [resultId]);

  const current = state?.key === key ? state : null;
  // While new columns settle, the previous groups stay, dimmed, instead of a blank panel.
  const stale = state?.kind === "done" && state.key !== key && key !== "";
  const shown = state?.kind === "done" && (current || stale) ? state : null;
  const grouping = shown?.grouping ?? null;
  const name = (column: number) => props.columns[column]?.name ?? String(column);
  const free = props.columns.map((_, index) => index).filter((index) => !grouped.includes(index));

  const filterTo = (columns: number[], firstRow: number, generation: number) => {
    if (props.locked) return;
    setFailure(null);
    setCaseSwitched(false);
    call(commands.filterToGroup(resultId, columns, firstRow, generation))
      .then((outcome) => {
        // The core matched case to keep exactly the group's rows, which changes how the other
        // filters in force are read: say so (SDB-410).
        setCaseSwitched(outcome.caseSwitched);
        props.onFiltered(outcome.filters, outcome.rows, outcome.total, outcome.mode);
      })
      .catch((e: unknown) => {
        // Refused (the rows moved since these groups were counted, or the grid cannot be
        // filtered now): say why, and count the groups again so the next click is on fresh ones.
        setFailure(e instanceof Error ? e.message : String(e));
        setAgain((n) => n + 1);
      });
  };

  return (
    <aside className={`cell-viewer group-panel${props.dropping ? " group-dropping" : ""}`} aria-label={t("group.title")} onKeyDown={(e) => e.key === "Escape" && props.onClose()}>
      <header>
        <div className="cell-viewer-heading">
          <div className="cell-viewer-title">
            <strong>{t("group.title")}</strong>
          </div>
          <button className="ghost cell-viewer-close" aria-label={t("group.close", { shortcut: keyText("close") })} title={t("group.close", { shortcut: keyText("close") })} onClick={props.onClose}><Icon name="x" /></button>
        </div>
        <div className="cell-viewer-controls">{props.modes}</div>
      </header>
      <div className="references-body group-body" aria-busy={busy}>
        <div className="group-columns" role="group" aria-label={t("group.columns")}>
          {grouped.map((column) => (
            <span className="group-chip" key={column}>
              <span>{name(column)}</span>
              <button type="button" className="ghost" aria-label={t("group.remove", { column: name(column) })} title={t("group.remove", { column: name(column) })} onClick={() => props.onGrouped(grouped.filter((c) => c !== column))}><Icon name="x" /></button>
            </span>
          ))}
          {free.length > 0 && (
            <select
              className="group-add"
              aria-label={t("group.add")}
              value=""
              onChange={(e) => {
                if (e.target.value !== "") props.onGrouped([...grouped, Number(e.target.value)]);
              }}
            >
              <option value="">{t("group.add")}</option>
              {free.map((column) => <option key={column} value={column}>{name(column)}</option>)}
            </select>
          )}
        </div>
        <p className="reference-note group-hint">{t(props.dropping ? "group.dropHere" : "group.hint")}</p>
        <label className="check group-duplicates">
          <input type="checkbox" name="groupDuplicates" checked={duplicates} onChange={(e) => setDuplicates(e.target.checked)} />
          {t("group.duplicatesOnly")}
        </label>
        {props.scope && <p className="reference-note group-scope" role="note">{t(props.scope === "page" ? "group.pageOnly" : "group.loadedOnly")}</p>}
        {grouped.length > 0 && props.waiting && <p className="reference-note">{t("group.waiting")}</p>}
        {grouped.length > 0 && busy && (
          <div className="calc-busy" role="status">
            <span>{t("group.grouping")}</span>
            <button type="button" onClick={() => void call(commands.cancelGrouping(resultId)).catch(() => undefined)}>
              {t("group.cancel")}
            </button>
          </div>
        )}
        {grouped.length > 0 && !busy && current?.kind === "cancelled" && (
          <div className="calc-busy" role="status">
            <span>{t("group.cancelled")}</span>
            <button type="button" className="ghost" onClick={() => setAgain((n) => n + 1)}>{t("group.again")}</button>
          </div>
        )}
        {grouped.length > 0 && current?.kind === "failed" && <p className="error small" role="alert">{t("group.failed", { detail: current.error })}</p>}
        {failure && <p className="error small" role="alert">{t("group.filterFailed", { detail: failure })}</p>}
        {caseSwitched && <p className="muted small group-case-switched" role="status">{t("group.caseSwitched")}</p>}
        {grouped.length > 0 && grouping && (
          <div className={stale ? "calc-stale" : undefined}>
            <p className="reference-note group-summary" role="status">
              {t(shown?.duplicates ? "group.summaryDuplicates" : "group.summary", {
                count: grouping.totalGroups ?? 0,
                groups: numbers.format(grouping.totalGroups ?? 0),
                rows: numbers.format(grouping.rows ?? 0),
              })}
            </p>
            {grouping.groups.length > 0 && (
              <table className="group-table">
                <thead>
                  <tr>
                    {(shown?.columns ?? grouped).map((column) => <th key={column} scope="col">{name(column)}</th>)}
                    <th scope="col" className="group-number">{t("group.count")}</th>
                    <th scope="col" className="group-number">{t("group.percent")}</th>
                  </tr>
                </thead>
                <tbody>
                  {grouping.groups.map((group) => {
                    const count = group.count ?? 0;
                    const label = group.key.map((value) => (value === null ? "NULL" : value)).join(", ");
                    return (
                      <tr key={group.firstRow ?? 0}>
                        {group.key.map((value, index) => (
                          <td key={index}>
                            {index === 0 ? (
                              <button
                                type="button"
                                className="group-filter"
                                disabled={props.locked || stale}
                                aria-label={t("group.filterTo", { group: label })}
                                title={t("group.filterTo", { group: label })}
                                onClick={() => filterTo(shown?.columns ?? grouped, group.firstRow ?? 0, grouping.generation ?? 0)}
                              >
                                <GroupValue value={value} empty={t("group.empty")} />
                              </button>
                            ) : (
                              <GroupValue value={value} empty={t("group.empty")} />
                            )}
                          </td>
                        ))}
                        <td className="group-number">{numbers.format(count)}</td>
                        <td className="group-number">{grouping.rows ? percents.format(count / grouping.rows) : "—"}</td>
                      </tr>
                    );
                  })}
                </tbody>
              </table>
            )}
            {(grouping.totalGroups ?? 0) > grouping.groups.length && (
              <p className="reference-note" role="note">{t("group.largestOnly", { shown: numbers.format(grouping.groups.length), count: grouping.maxGroups })}</p>
            )}
            {(grouping.uncountedRows ?? 0) > 0 && (
              <p className="reference-note" role="note">{t("group.uncounted", { rows: numbers.format(grouping.uncountedRows ?? 0), count: grouping.uncountedRows ?? 0 })}</p>
            )}
          </div>
        )}
      </div>
    </aside>
  );
}

/** One value of a group's key: NULL and the empty text are told apart from any text. */
function GroupValue({ value, empty }: { value: string | null; empty: string }) {
  if (value === null) return <span className="muted group-null">NULL</span>;
  if (value === "") return <span className="muted">{empty}</span>;
  return <span title={value}>{value}</span>;
}
