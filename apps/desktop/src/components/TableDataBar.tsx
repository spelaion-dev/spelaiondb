// The bar of a table data tab: WHERE and ORDER BY typed by the user (the core writes and
// checks the SELECT), rows per page, previous / next page and an exact count on demand.

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TableView } from "../store";

const PAGE_SIZES = [100, 500, 1000, 5000];

export function TableDataBar(props: {
  view: TableView;
  /** Rows of the page on screen (`null` while none has loaded). */
  shown: number | null;
  busy: boolean;
  onChange(patch: Partial<TableView>): void;
  onCount(): void;
}) {
  const { view, shown, busy } = props;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filter, setFilter] = useState(view.filter);
  const [orderBy, setOrderBy] = useState(view.orderBy);
  // A header click writes the ORDER BY: show it.
  useEffect(() => setOrderBy(view.orderBy), [view.orderBy]);
  useEffect(() => setFilter(view.filter), [view.filter]);

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
      <button type="submit" disabled={busy}>
        {t("table.apply")}
      </button>
      <div className="spacer" />
      <select
        aria-label={t("table.pageSize")}
        value={view.pageSize}
        disabled={busy}
        onChange={(e) => props.onChange({ pageSize: Number(e.target.value), page: 0 })}
      >
        {PAGE_SIZES.map((n) => (
          <option key={n} value={n}>
            {t("table.perPage", { count: n, formattedCount: numbers.format(n) })}
          </option>
        ))}
      </select>
      <button type="button" className="ghost" aria-label={t("table.previous")} disabled={busy || view.page === 0} onClick={() => props.onChange({ page: view.page - 1 })}>
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
      <button type="button" className="ghost" aria-label={t("table.next")} disabled={busy || !full} onClick={() => props.onChange({ page: view.page + 1 })}>
        ▶
      </button>
      <button type="button" className="ghost" disabled={busy} title={t("table.countHint")} onClick={props.onCount}>
        {t("table.count")}
      </button>
    </form>
  );
}
