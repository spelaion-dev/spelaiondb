// The bar of a table data tab: WHERE and ORDER BY typed by the user (the core writes and
// checks the SELECT), rows per page, previous / next page and an exact count on demand.

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CellEditItem, ColumnItem } from "../api";
import type { TableView } from "../store";

const PAGE_SIZES = [100, 500, 1000, 5000];

export function TableDataBar(props: {
  view: TableView;
  /** Rows of the page on screen (`null` while none has loaded). */
  shown: number | null;
  busy: boolean;
  onChange(patch: Partial<TableView>): void;
  onCount(): void;
  /** Editing (ADR-014): `null` while the primary key is unknown, `false` without one. */
  editable: boolean | null;
  pending: number;
  columns: ColumnItem[];
  onInsert(values: CellEditItem[]): void;
  onApply(): void;
  onDiscard(): void;
}) {
  const { view, shown, busy } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filter, setFilter] = useState(view.filter);
  const [orderBy, setOrderBy] = useState(view.orderBy);
  // A header click writes the ORDER BY: show it.
  useEffect(() => setOrderBy(view.orderBy), [view.orderBy]);
  useEffect(() => setFilter(view.filter), [view.filter]);

  const [inserting, setInserting] = useState(false);
  const locked = busy || props.pending > 0;
  const first = view.page * view.pageSize;
  const full = shown !== null && shown >= view.pageSize;
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
          ...(filter.trim() !== view.filter ? { total: undefined } : {}),
        });
      }}
    >
      <label className="table-clause">
        <span className="mono">WHERE</span>
        <input value={filter} onChange={(e) => setFilter(e.target.value)} placeholder={t("table.filterPlaceholder")} spellCheck={false} aria-label="WHERE" />
      </label>
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
        onChange={(e) => props.onChange({ pageSize: Number(e.target.value), page: 0 })}
      >
        {PAGE_SIZES.map((n) => (
          <option key={n} value={n}>
            {t("table.perPage", { count: n, formattedCount: numbers.format(n) })}
          </option>
        ))}
      </select>
      <button type="button" className="ghost" aria-label={t("table.previous")} disabled={locked || view.page === 0} onClick={() => props.onChange({ page: view.page - 1 })}>
        ◀
      </button>
      <span className="small table-range" role="status">
        {shown === null
          ? ""
          : shown === 0
            ? t("table.noRows")
            : t("table.range", { from: numbers.format(first + 1), to: numbers.format(first + shown) })}
        {view.total !== undefined ? ` ${t("table.ofTotal", { total: numbers.format(view.total) })}` : ""}
      </span>
      <button type="button" className="ghost" aria-label={t("table.next")} disabled={locked || !full} onClick={() => props.onChange({ page: view.page + 1 })}>
        ▶
      </button>
      <button type="button" className="ghost" disabled={busy} title={t("table.countHint")} onClick={props.onCount}>
        {t("table.count")}
      </button>
      {props.editable === false && <span className="muted small" title={t("edit.readOnlyHint")}>{t("edit.readOnly")}</span>}
      {props.editable && (
        <button type="button" className="ghost" disabled={busy || props.columns.length === 0} onClick={() => setInserting(true)}>
          {t("edit.addRow")}
        </button>
      )}
      {props.pending > 0 && (
        <span className="pending-changes">
          <span className="small">{t("edit.pending", { count: props.pending })}</span>
          <button type="button" className="primary" disabled={busy} onClick={props.onApply}>
            {t("edit.apply")}
          </button>
          <button type="button" className="ghost" disabled={busy} onClick={props.onDiscard}>
            {t("edit.discard")}
          </button>
        </span>
      )}
      {inserting && (
        <InsertRowDialog
          columns={props.columns}
          onAdd={(values) => {
            setInserting(false);
            props.onInsert(values);
          }}
          onCancel={() => setInserting(false)}
        />
      )}
    </form>
  );
}

/** A new row: only the columns filled in are sent; the others take their default. */
function InsertRowDialog(props: { columns: ColumnItem[]; onAdd(values: CellEditItem[]): void; onCancel(): void }) {
  const { t } = useTranslation("editor");
  const [values, setValues] = useState<Record<number, { text: string; isNull: boolean }>>({});
  const add = () =>
    props.onAdd(
      Object.entries(values)
        .filter(([, v]) => v.isNull || v.text !== "")
        .map(([column, v]) => ({ column: Number(column), value: v.isNull ? null : v.text })),
    );
  return (
    <div className="modal-backdrop">
      <div className="modal insert-row" role="dialog" aria-modal="true" aria-label={t("edit.addRowTitle")}>
        <h2>{t("edit.addRowTitle")}</h2>
        <p className="muted small">{t("edit.addRowHint")}</p>
        <div
          className="insert-row-fields"
          onKeyDown={(e) => {
            if (e.key === "Escape") props.onCancel();
          }}
        >
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
