// A result's first chart (ADR-040): which column is X, which numbers are Y, and whether it is a
// line (over time) or bars. Kept apart from ResultChart.tsx so `node --test` can run it
// (scripts/chart-defaults.test.mjs).

import type { ChartAggregateItem, ChartKindItem, ColumnItem } from "./bindings";

/** What a result's chart draws; kept per result while the tab is open. */
export interface ChartSettings {
  kind: ChartKindItem;
  x: number;
  y: number[];
  aggregate: ChartAggregateItem;
}

/** The colors of the theme's chart palette (styles.css), one per series. */
export const CHART_SERIES = 8;
export const NUMERIC = new Set(["int", "float", "decimal"]);
const TEMPORAL = new Set(["date", "time", "timestamp", "timestamptz"]);
/** Native types of text columns that hold dates and times, as the core's
 *  `temporal::text_temporal` reads them: MySQL's and MariaDB's `DATE`, `DATETIME` and `TIMESTAMP`,
 *  SQL Server's `datetime2`, `datetimeoffset` and `time`. */
const TEXT_TEMPORAL = new Set(["date", "datetime", "timestamp", "datetime2", "datetimeoffset", "time"]);

/** A column of dates or times: a temporal type, or text whose native type is one (the same rule
 *  as the core's chart, which puts such a column on a time axis). */
export function isTemporal(column: ColumnItem | undefined): boolean {
  if (!column) return false;
  if (TEMPORAL.has(column.logical)) return true;
  if (column.logical !== "text") return false;
  const base = column.nativeType.trim().toLowerCase().split(/[(\s]/)[0] ?? "";
  return TEXT_TEMPORAL.has(base);
}

export function numericColumns(columns: ColumnItem[]): number[] {
  return columns.flatMap((c, i) => (NUMERIC.has(c.logical) ? [i] : []));
}

/** A first chart for a result: `picked` are the columns selected in the grid, if any (the first
 *  is X, the numeric ones after it are Y). `null` when no column holds numbers. */
export function defaultChart(columns: ColumnItem[], picked: number[] = []): ChartSettings | null {
  const numeric = numericColumns(columns);
  if (numeric.length === 0) return null;
  const looksLikeKey = (i: number) => /^id$|_id$/i.test(columns[i]?.name ?? "");
  let x: number;
  let y: number[];
  const first = picked[0];
  if (first !== undefined) {
    x = first;
    y = picked.slice(1).filter((c) => NUMERIC.has(columns[c]?.logical ?? "")).slice(0, CHART_SERIES);
  } else {
    const temporal = columns.findIndex((c) => isTemporal(c));
    const label = columns.findIndex((c) => !NUMERIC.has(c.logical));
    x = temporal >= 0 ? temporal : label >= 0 ? label : 0;
    y = [];
  }
  if (y.length === 0) {
    const candidates = numeric.filter((c) => c !== x);
    const value = candidates.find((c) => !looksLikeKey(c)) ?? candidates[0] ?? numeric[0];
    y = value === undefined ? [] : [value];
  }
  const kind: ChartKindItem = isTemporal(columns[x]) ? "line" : "bar";
  return { kind, x, y, aggregate: "sum" };
}
