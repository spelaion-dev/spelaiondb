// The bar of a table data tab: WHERE and ORDER BY typed by the user (the core writes and
// checks the SELECT), rows per page, the pages (first, previous, a page by number, next, last),
// an exact count on demand and "Load every row" (SDB-386).

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CellEditItem, ColumnItem } from "../api";
import type { TableView } from "../store";
import { TableFilterBox } from "./TableFilterBox";
import { keyText } from "../shortcuts";
import { Icon } from "./Icons";

export const PAGE_SIZES = [100, 500, 1000, 5000];

export function TableDataBar(props: {
  view: TableView;
  /** Rows of the page on screen (`null` while none has loaded). */
  shown: number | null;
  busy: boolean;
  onChange(patch: Partial<TableView>): void;
  onCount(): void;
  /** The last page: the core counts the rows, then the tab shows the page holding the last. */
  onLast(): void;
  /** "Load every row…": says how many rows and how much memory first. */
  onLoadAll(): void;
  /** The table has no primary key: its pages are not exact (ADR-054), and the bar says so. */
  keyless: boolean;
  /** Editing (ADR-014): `null` while the primary key is unknown, `false` without one. */
  editable: boolean | null;
  readOnlyConnection: boolean;
  pending: number;
  /** For the filter box's memory (SDB-381): the tab's connection and session, the columns of
   *  the page on screen, and whether that page loaded without an error. */
  connectionId: string | null;
  session: string;
  columns: string[];
  loaded: boolean;
}) {
  const { view, shown, busy } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filter, setFilter] = useState(view.filter);
  const [orderBy, setOrderBy] = useState(view.orderBy);
  // A header click writes the ORDER BY: show it.
  useEffect(() => setOrderBy(view.orderBy), [view.orderBy]);
  useEffect(() => setFilter(view.filter), [view.filter]);

  const locked = busy || props.pending > 0;
  const first = view.page * view.pageSize;
  const full = shown !== null && shown >= view.pageSize;
  const pages = view.total === undefined ? undefined : Math.max(1, Math.ceil(view.total / view.pageSize));
  return (
    <form
      className="table-bar"
      aria-label={t("table.bar", { name: view.name })}
      onSubmit={(e) => {
        e.preventDefault();
        const orderChanged = orderBy.trim() !== view.orderBy.trim();
        props.onChange({
          filter: filter.trim(),
          orderBy: orderBy.trim(),
          page: 0,
          ...(orderChanged ? { sort: undefined } : {}),
          // Every row was loaded for the rows the old filter kept: back to pages.
          ...(filter.trim() !== view.filter ? { total: undefined, all: undefined } : {}),
        });
      }}
    >
      <TableFilterBox
        connectionId={props.connectionId}
        session={props.session}
        schema={view.schema}
        name={view.name}
        applied={view.filter}
        loaded={props.loaded}
        value={filter}
        onChange={setFilter}
        onApply={(next) => props.onChange({ filter: next.trim(), page: 0, ...(next.trim() !== view.filter ? { total: undefined, all: undefined } : {}) })}
        columns={props.columns}
        disabled={locked}
      />
      <label className="table-clause">
        <span className="mono">ORDER BY</span>
        <input value={orderBy} onChange={(e) => setOrderBy(e.target.value)} placeholder={t("table.orderPlaceholder")} spellCheck={false} aria-label="ORDER BY" />
      </label>
      <button type="submit" disabled={locked}>
        {t("table.apply")}
      </button>
      <div className="spacer" />
      <select
        aria-label={t("table.pageSize")}
        value={view.pageSize}
        disabled={locked}
        onChange={(e) => props.onChange({ pageSize: Number(e.target.value), page: 0, all: undefined })}
      >
        {PAGE_SIZES.map((n) => (
          <option key={n} value={n}>
            {t("table.perPage", { count: n, formattedCount: numbers.format(n) })}
          </option>
        ))}
      </select>
      {view.all ? (
        <>
          <span className="small table-range" role="status">
            {shown === null ? "" : shown === 0 ? t("table.noRows") : `${t("table.range", { from: numbers.format(1), to: numbers.format(shown) })} · ${t("pages.allRows")}`}
          </span>
          <button type="button" className="ghost" disabled={locked} title={t("pages.showPagesHint")} onClick={() => props.onChange({ all: undefined, page: 0 })}>
            {t("pages.showPages")}
          </button>
        </>
      ) : (
        <PageControls
          page={view.page}
          pages={pages}
          locked={locked}
          canNext={full}
          hint={t(props.keyless ? "pages.keylessHint" : "pages.numberedHint")}
          onPage={(page) => props.onChange({ page })}
          onLast={props.onLast}
          range={
            shown === null
              ? ""
              : `${shown === 0 ? t("table.noRows") : t("table.range", { from: numbers.format(first + 1), to: numbers.format(first + shown) })}${view.total !== undefined ? ` ${t("table.ofTotal", { total: numbers.format(view.total) })}` : ""}`
          }
        />
      )}
      {props.keyless && !view.all && <span className="muted small" title={t("pages.keylessHint")}>{t("pages.keyless")}</span>}
      <button type="button" className="ghost" disabled={busy} title={t("table.countHint")} onClick={props.onCount}>
        {t("table.count")}
      </button>
      {!view.all && (
        <button type="button" className="ghost" disabled={locked || shown === null} title={t("pages.loadAllHint")} onClick={props.onLoadAll}>
          {t("pages.loadAll")}
        </button>
      )}
      {props.editable === false && <span className="muted small" title={t(props.readOnlyConnection ? "edit.readOnlyConnection" : "edit.readOnlyHint")}>{t("edit.readOnly")}</span>}
    </form>
  );
}

/** First, previous, the page's number (typed to jump), next and last (SDB-386): a table data
 *  tab's numbered pages and a paged query's pages share it. `pages` is the number of pages
 *  once the rows are counted; `hint` says how exact a page is on that path. */
export function PageControls(props: {
  /** From 0. */
  page: number;
  pages?: number;
  locked: boolean;
  canNext: boolean;
  /** `false` when the last page cannot be reached (nothing more to load). */
  canLast?: boolean;
  hint: string;
  lastHint?: string;
  range: string;
  onPage(page: number): void;
  onLast(): void;
}) {
  const { page, pages, locked } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [text, setText] = useState(String(page + 1));
  useEffect(() => setText(String(page + 1)), [page]);
  const jump = () => {
    const typed = Number.parseInt(text.trim(), 10);
    if (!Number.isFinite(typed) || typed < 1) {
      setText(String(page + 1));
      return;
    }
    const target = (pages === undefined ? typed : Math.min(typed, pages)) - 1;
    setText(String(target + 1));
    if (target !== page) props.onPage(target);
  };
  return (
    <div className="page-controls" role="group" aria-label={t("pages.label")} title={props.hint}>
      <button type="button" className="ghost icon-button" aria-label={t("pages.first")} title={t("pages.first")} disabled={locked || page === 0} onClick={() => props.onPage(0)}>
        <Icon name="chevronsLeft" />
      </button>
      <button type="button" className="ghost icon-button" aria-label={t("table.previous")} title={t("table.previous")} disabled={locked || page === 0} onClick={() => props.onPage(page - 1)}>
        <Icon name="chevronLeft" />
      </button>
      <input
        className="page-number"
        inputMode="numeric"
        aria-label={t("pages.page")}
        title={t("pages.pageHint", { enter: keyText("submit") })}
        value={text}
        disabled={locked}
        onChange={(e) => setText(e.target.value.replace(/[^0-9]/g, ""))}
        onBlur={() => setText(String(page + 1))}
        onKeyDown={(e) => {
          if (e.key === "Enter") jump();
          else if (e.key === "Escape") setText(String(page + 1));
          else return;
          // Never the form's submit (the table bar's Apply) nor a dialog's Escape.
          e.preventDefault();
          e.stopPropagation();
        }}
      />
      {pages !== undefined && <span className="small muted">{t("pages.ofPages", { pages: numbers.format(pages) })}</span>}
      <span className="small table-range" role="status">{props.range}</span>
      <button type="button" className="ghost icon-button" aria-label={t("table.next")} title={t("table.next")} disabled={locked || !props.canNext} onClick={() => props.onPage(page + 1)}>
        <Icon name="chevronRight" />
      </button>
      <button type="button" className="ghost icon-button" aria-label={t("pages.last")} title={props.lastHint ?? t("pages.last")} disabled={locked || props.canLast === false} onClick={props.onLast}>
        <Icon name="chevronsRight" />
      </button>
    </div>
  );
}

/** A new row: only the columns filled in are sent; the others take their default. */
export function InsertRowDialog(props: { columns: ColumnItem[]; initial?: CellEditItem[]; onAdd(values: CellEditItem[]): void; onCancel(): void }) {
  const { t } = useTranslation("editor");
  const [values, setValues] = useState<Record<number, { text: string; isNull: boolean }>>(() =>
    Object.fromEntries((props.initial ?? []).map(({ column, value }) => [column, { text: value ?? "", isNull: value === null }])),
  );
  const add = () =>
    props.onAdd(
      Object.entries(values)
        .map(([column, v]) => ({ column: Number(column), value: v.isNull ? null : v.text })),
    );
  return (
    <div className="modal-backdrop">
      <div className="modal insert-row" role="dialog" aria-modal="true" aria-label={t("edit.addRowTitle")} onKeyDown={(e) => {
        if (e.key === "Escape") { e.stopPropagation(); props.onCancel(); }
      }}>
        <h2>{t("edit.addRowTitle")}</h2>
        <p className="muted small">{t("edit.addRowHint")}</p>
        <div className="insert-row-fields">
          {props.columns.map((c, i) => {
            const v = values[i] ?? { text: "", isNull: false };
            return (
              <label key={i} className="field">
                <span>
                  {c.name} <span className="muted small">{c.nativeType}</span>
                </span>
                <input
                  autoFocus={i === 0}
                  aria-label={c.name}
                  value={v.text}
                  disabled={v.isNull}
                  placeholder={t("edit.defaultValue")}
                  onChange={(e) => setValues({ ...values, [i]: { ...v, text: e.target.value } })}
                  spellCheck={false}
                />
                <label className="small">
                  <input type="checkbox" checked={v.isNull} onChange={(e) => setValues({ ...values, [i]: { ...v, isNull: e.target.checked } })} /> NULL
                </label>
              </label>
            );
          })}
        </div>
        <div className="modal-actions">
          <button type="button" onClick={props.onCancel}>
            {t("results.cancel")}
          </button>
          <button type="button" className="primary" onClick={add}>
            {t("edit.addRowConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
