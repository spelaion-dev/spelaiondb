// Canvas result grid (ADR-010). Only what is visible is painted; scrolling is native
// (a container with a "spacer" of the full size) and the canvas is redrawn on top.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type CellRange, type ColumnFilterItem, type FilterOpItem, type TextFormat } from "../api";
import { THEME_CHANGE_EVENT } from "../preferences";
import { RowCache } from "../grid/rowCache";
import type { ResultMeta, SortState } from "../store";
import { CellViewer, type ViewerMode } from "./CellViewer";

const ROW_H = 26;
const HEADER_H = 36;
const PAD = 8;
const MIN_COL = 72;
const MAX_COL = 420;
/** Limits when the user resizes a column (wider than the automatic maximum). */
const MIN_RESIZE = 48;
const MAX_RESIZE = 2000;
/** Pixels on each side of a header edge that grab it. */
const EDGE = 4;
const DATA_FONT = '12.5px ui-monospace, "Cascadia Mono", Consolas, monospace';
const HEADER_FONT = '600 12.5px system-ui, "Segoe UI", sans-serif';
const TYPE_FONT = '11px system-ui, "Segoe UI", sans-serif';
const NUMERIC = new Set(["int", "float", "decimal"]);
const MENU_MARGIN = 8;

/** Keep a measured popup inside the window, including when the window is resized. */
function placeMenu(menu: HTMLElement, x: number, y: number) {
  const rect = menu.getBoundingClientRect();
  menu.style.left = `${Math.max(MENU_MARGIN, Math.min(x, window.innerWidth - rect.width - MENU_MARGIN))}px`;
  menu.style.top = `${Math.max(MENU_MARGIN, Math.min(y, window.innerHeight - rect.height - MENU_MARGIN))}px`;
}

interface Cell {
  row: number;
  col: number;
}

/** A rectangle from the cell where the selection started (`anchor`) to the active one. */
interface Selection {
  anchor: Cell;
  focus: Cell;
}

interface Bounds {
  r0: number;
  r1: number;
  c0: number;
  c1: number;
}

function bounds(selection: Selection): Bounds {
  return {
    r0: Math.min(selection.anchor.row, selection.focus.row),
    r1: Math.max(selection.anchor.row, selection.focus.row),
    c0: Math.min(selection.anchor.col, selection.focus.col),
    c1: Math.max(selection.anchor.col, selection.focus.col),
  };
}

const inside = (b: Bounds, row: number, col: number) => row >= b.r0 && row <= b.r1 && col >= b.c0 && col <= b.c1;

/** Copy menu entries: format and whether the column names go first. */
const COPY_ITEMS: { key: string; format: TextFormat; header: boolean }[] = [
  { key: "copy", format: "tsv", header: false },
  { key: "copyWithHeaders", format: "tsv", header: true },
  { key: "copyCsv", format: "csv", header: true },
  { key: "copyJson", format: "json", header: false },
  { key: "copyMarkdown", format: "markdown", header: true },
  { key: "copyInList", format: "inList", header: false },
  { key: "copyInsert", format: "sqlInsert", header: false },
];

/** Truncates a text to the available width with "…". */
function fit(ctx: CanvasRenderingContext2D, text: string, max: number): string {
  const sample = text.length > 300 ? text.slice(0, 300) : text;
  if (ctx.measureText(sample).width <= max) return sample;
  let lo = 0;
  let hi = sample.length;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (ctx.measureText(sample.slice(0, mid) + "…").width <= max) lo = mid;
    else hi = mid - 1;
  }
  return sample.slice(0, lo) + "…";
}

function cssVar(style: CSSStyleDeclaration, name: string): string {
  return style.getPropertyValue(name).trim() || "#888";
}

/** Pending changes of a table data tab (ADR-014): drawn over the page until applied. */
export interface GridEditing {
  /** `"row:col"` → the value typed (`null` is NULL). */
  cells: Record<string, string | null>;
  deleted: number[];
  /** `undefined` takes the cell back to its value in the database. */
  onEdit(row: number, col: number, value: string | null | undefined): void;
  onDelete(rows: number[]): void;
  onRevert(rows: number[]): void;
}

const EDITED_BG = "rgba(250, 204, 21, 0.30)";
const DELETED_BG = "rgba(239, 68, 68, 0.20)";

/** SQL spelling of each column filter operator, as SQLyog shows them. */
const OPS: { op: FilterOpItem; sql: string }[] = [
  { op: "eq", sql: "=" },
  { op: "notEq", sql: "<>" },
  { op: "gt", sql: ">" },
  { op: "lt", sql: "<" },
  { op: "ge", sql: ">=" },
  { op: "le", sql: "<=" },
  { op: "like", sql: "LIKE" },
  { op: "notLike", sql: "NOT LIKE" },
  { op: "isNull", sql: "IS NULL" },
  { op: "isNotNull", sql: "IS NOT NULL" },
];
const takesValue = (op: FilterOpItem) => op !== "isNull" && op !== "isNotNull";

/** `Command <> 'Sleep'`, `Id > 10`, `Info IS NULL`: how a condition reads. */
function filterLabel(filter: ColumnFilterItem, columns: ResultMeta["columns"]): string {
  const column = columns[filter.column];
  const sql = OPS.find((o) => o.op === filter.op)?.sql ?? filter.op;
  if (!takesValue(filter.op)) return `${column?.name ?? "?"} ${sql}`;
  const numeric = NUMERIC.has(column?.logical ?? "") && filter.op !== "like" && filter.op !== "notLike";
  const value = numeric ? filter.value : `'${filter.value.replace(/'/g, "''")}'`;
  return `${column?.name ?? "?"} ${sql} ${value}`;
}

/** Text for `LIKE '%…%'`: its own `%`, `_` and `\` are escaped. */
const likeContaining = (text: string) => `%${text.replace(/[\\%_]/g, (c) => `\\${c}`)}%`;

export function ResultGrid({
  result,
  session,
  onSorted,
  onColumnFiltered,
  serverSort,
  editing,
}: {
  result: ResultMeta;
  session: string;
  onSorted(sort: SortState | undefined): void;
  onColumnFiltered(filters: ColumnFilterItem[], rows: number, total: number): void;
  /** In a table data tab, header clicks sort in the server (ORDER BY) instead. */
  serverSort?: { sort: SortState | undefined; onSort(sort: SortState | undefined): void };
  /** Editable table data tab: pending changes and how to change them. */
  editing?: GridEditing;
}) {
  const sortShown = serverSort ? serverSort.sort : result.sort;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const frame = useRef<number | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [selection, setSelection] = useState<Selection | null>(null);
  /** The context menu, with the cell it was opened on (for the Filter submenu). */
  const [menu, setMenu] = useState<{ x: number; y: number; cell: Cell | null } | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  /** The custom filter dialog, starting from a column and a value. */
  const [customFilter, setCustomFilter] = useState<ColumnFilterItem | null>(null);
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  const dragging = useRef(false);
  const [viewer, setViewer] = useState(false);
  const [viewerMode, setViewerMode] = useState<ViewerMode>("cell");
  /** The cell being typed into (table data tabs). */
  const [cellEditor, setCellEditor] = useState<{ row: number; col: number; value: string } | null>(null);

  /** Opens the inline editor on a cell, with its pending value or the one loaded. */
  function startEdit(cell: Cell) {
    if (!editing) return;
    const key = `${cell.row}:${cell.col}`;
    const current = key in editing.cells ? editing.cells[key] : cache.get(cell.row, cell.col);
    setSelection({ anchor: cell, focus: cell });
    scrollIntoView(cell);
    setCellEditor({ row: cell.row, col: cell.col, value: current ?? "" });
  }

  function commitEdit(value: string) {
    if (!editing || !cellEditor) return;
    const original = cache.get(cellEditor.row, cellEditor.col);
    // Typing back the loaded value takes the cell back to it.
    editing.onEdit(cellEditor.row, cellEditor.col, original !== undefined && original !== null && value === original ? undefined : value);
    setCellEditor(null);
    scrollerRef.current?.focus();
  }

  /** Rows of the selection (the active row without one). */
  const selectedRows = (): number[] => {
    if (!selection) return [];
    const b = bounds(selection);
    return Array.from({ length: b.r1 - b.r0 + 1 }, (_, i) => b.r0 + i);
  };

  /** Ctrl+Shift+J: the column picker's search text (`null` when closed). */
  const [jump, setJump] = useState<string | null>(null);

  const redraw = useCallback(() => {
    if (frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      draw();
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // A sort or filter changes which row is at each position: a new cache asks the core again
  // (blocks still on their way to the old one are dropped with it).
  const filtersKey = JSON.stringify(result.columnFilters ?? []);
  const view = `${result.sort ? `${result.sort.column}:${result.sort.descending}` : ""}|${result.filter ?? ""}|${filtersKey}`;
  const cache = useMemo(() => new RowCache(result.id, () => redraw()), [result.id, view, redraw]);
  cache.grewTo(result.rows);

  // Widths: header and type, bounded. (Fitting to content: next iteration.)
  // Widths the user set by dragging (or fitted with a double-click), per column.
  const [resized, setResized] = useState<Record<number, number>>({});
  const resizing = useRef<{ col: number; startX: number; startWidth: number } | null>(null);
  const automatic = useMemo(() => {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return result.columns.map(() => 140);
    return result.columns.map((c) => {
      ctx.font = HEADER_FONT;
      const name = ctx.measureText(c.name).width;
      ctx.font = TYPE_FONT;
      const type = ctx.measureText(c.nativeType).width;
      const base = NUMERIC.has(c.logical) ? 110 : 160;
      return Math.round(Math.min(MAX_COL, Math.max(MIN_COL, name + PAD * 2 + 8, type + PAD * 2, base)));
    });
  }, [result.columns]);
  const widths = useMemo(() => automatic.map((w, i) => resized[i] ?? w), [automatic, resized]);
  const offsets = useMemo(() => {
    const out = [0];
    for (const w of widths) out.push((out[out.length - 1] ?? 0) + w);
    return out;
  }, [widths]);
  const rowNumW = Math.max(52, numbers.format(result.rows).length * 8 + PAD * 2);
  const totalW = rowNumW + (offsets[offsets.length - 1] ?? 0);
  const totalH = HEADER_H + result.rows * ROW_H;

  // What `draw` needs, always up to date (draw is called outside the React cycle).
  const live = useRef({ result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, editing });
  live.current = { result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, editing };

  function draw() {
    const canvas = canvasRef.current;
    const scroller = scrollerRef.current;
    if (!canvas || !scroller) return;
    const { result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, editing } = live.current;
    const deletedRows = new Set(editing?.deleted ?? []);
    const range = selection ? bounds(selection) : null;
    const dpr = window.devicePixelRatio || 1;
    if (canvas.width !== size.w * dpr || canvas.height !== size.h * dpr) {
      canvas.width = size.w * dpr;
      canvas.height = size.h * dpr;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const style = getComputedStyle(scroller);
    const c = {
      bg: cssVar(style, "--grid-bg"),
      stripe: cssVar(style, "--grid-stripe"),
      line: cssVar(style, "--grid-line"),
      header: cssVar(style, "--grid-header"),
      text: cssVar(style, "--fg"),
      muted: cssVar(style, "--muted"),
      select: cssVar(style, "--accent-soft"),
      accent: cssVar(style, "--accent"),
    };
    const left = scroller.scrollLeft;
    const top = scroller.scrollTop;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = c.bg;
    ctx.fillRect(0, 0, size.w, size.h);
    ctx.textBaseline = "middle";

    const first = Math.floor(top / ROW_H);
    const visible = Math.ceil((size.h - HEADER_H) / ROW_H) + 1;
    const last = Math.min(result.rows, first + visible);
    let firstCol = 0;
    while (firstCol < widths.length && rowNumW + (offsets[firstCol + 1] ?? 0) - left < rowNumW) firstCol++;

    // Cells.
    ctx.font = DATA_FONT;
    for (let row = first; row < last; row++) {
      const y = HEADER_H + row * ROW_H - top;
      if (row % 2 === 1) {
        ctx.fillStyle = c.stripe;
        ctx.fillRect(rowNumW, y, size.w, ROW_H);
      }
      const deleted = deletedRows.has(row);
      if (deleted) {
        ctx.fillStyle = DELETED_BG;
        ctx.fillRect(rowNumW, y, size.w, ROW_H);
      }
      for (let col = firstCol; col < widths.length; col++) {
        const x = rowNumW + (offsets[col] ?? 0) - left;
        if (x > size.w) break;
        const w = widths[col] ?? 0;
        if (range && inside(range, row, col)) {
          ctx.fillStyle = c.select;
          ctx.fillRect(x, y, w, ROW_H);
        }
        if (selection && selection.focus.row === row && selection.focus.col === col) {
          ctx.strokeStyle = c.accent;
          ctx.strokeRect(x + 0.5, y + 0.5, w - 1, ROW_H - 1);
        }
        const editKey = `${row}:${col}`;
        const edited = editing !== undefined && editKey in editing.cells;
        if (edited) {
          ctx.fillStyle = EDITED_BG;
          ctx.fillRect(x, y, w, ROW_H);
        }
        const value = edited ? (editing?.cells[editKey] ?? null) : cache.get(row, col);
        const numeric = NUMERIC.has(result.columns[col]?.logical ?? "");
        let text: string;
        if (value === undefined) {
          ctx.fillStyle = c.muted;
          text = "…";
        } else if (value === null) {
          ctx.fillStyle = c.muted;
          text = "NULL";
        } else {
          ctx.fillStyle = c.text;
          text = value.replace(/[\r\n\t]+/g, " ");
        }
        const shown = fit(ctx, text, w - PAD * 2);
        if (numeric && value !== null && value !== undefined) {
          ctx.textAlign = "right";
          ctx.fillText(shown, x + w - PAD, y + ROW_H / 2);
          ctx.textAlign = "left";
        } else {
          ctx.fillText(shown, x + PAD, y + ROW_H / 2);
        }
      }
      if (deleted) {
        // Struck through until the change is applied or reverted.
        ctx.fillStyle = c.muted;
        ctx.fillRect(rowNumW, y + ROW_H / 2, size.w, 1);
      }
      ctx.fillStyle = c.line;
      ctx.fillRect(rowNumW, y + ROW_H - 1, size.w, 1);
    }
    // Vertical lines.
    ctx.fillStyle = c.line;
    for (let col = firstCol; col <= widths.length; col++) {
      const x = rowNumW + (offsets[col] ?? 0) - left;
      if (x > size.w) break;
      ctx.fillRect(Math.round(x) - 1, HEADER_H, 1, size.h);
    }

    // Row number column (fixed on the left).
    ctx.fillStyle = c.header;
    ctx.fillRect(0, HEADER_H, rowNumW, size.h);
    ctx.textAlign = "right";
    for (let row = first; row < last; row++) {
      const y = HEADER_H + row * ROW_H - top;
      if (range && row >= range.r0 && row <= range.r1) {
        ctx.fillStyle = c.select;
        ctx.fillRect(0, y, rowNumW, ROW_H);
      }
      ctx.fillStyle = c.muted;
      ctx.fillText(numbers.format(row + 1), rowNumW - PAD, y + ROW_H / 2);
    }
    ctx.textAlign = "left";
    ctx.fillStyle = c.line;
    ctx.fillRect(rowNumW - 1, 0, 1, size.h);

    // Header (fixed at the top): name and native type.
    ctx.fillStyle = c.header;
    ctx.fillRect(0, 0, size.w, HEADER_H);
    for (let col = firstCol; col < widths.length; col++) {
      const x = rowNumW + (offsets[col] ?? 0) - left;
      if (x > size.w) break;
      const w = widths[col] ?? 0;
      const meta = result.columns[col];
      if (!meta) continue;
      ctx.font = HEADER_FONT;
      ctx.fillStyle = c.text;
      const sorted = sortShown?.column === col;
      ctx.fillText(fit(ctx, meta.name, w - PAD * 2 - (sorted ? 14 : 0)), x + PAD, 13);
      if (sorted) {
        ctx.fillStyle = c.accent;
        ctx.textAlign = "right";
        ctx.fillText(sortShown?.descending ? "▼" : "▲", x + w - PAD, 13);
        ctx.textAlign = "left";
      }
      ctx.font = TYPE_FONT;
      ctx.fillStyle = c.muted;
      ctx.fillText(fit(ctx, meta.nativeType, w - PAD * 2), x + PAD, 27);
      ctx.fillStyle = c.line;
      ctx.fillRect(Math.round(x + w) - 1, 0, 1, HEADER_H);
    }
    ctx.fillStyle = c.header;
    ctx.fillRect(0, 0, rowNumW, HEADER_H);
    ctx.fillStyle = c.line;
    ctx.fillRect(0, HEADER_H - 1, size.w, 1);
    ctx.fillRect(rowNumW - 1, 0, 1, HEADER_H);
  }

  useEffect(() => {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const observer = new ResizeObserver(() => setSize({ w: scroller.clientWidth, h: scroller.clientHeight }));
    observer.observe(scroller);
    window.addEventListener(THEME_CHANGE_EVENT, redraw);
    return () => {
      observer.disconnect();
      window.removeEventListener(THEME_CHANGE_EVENT, redraw);
    };
  }, [redraw]);

  useEffect(() => {
    redraw();
  });

  useEffect(() => {
    // Other rows: the selection would point at rows that moved or are gone.
    setSelection(null);
    setViewer(false);
  }, [result.filter, filtersKey]);

  useEffect(() => {
    // New result: scroll to the top and clear the selection.
    setResized({});
    setSelection(null);
    setMenu(null);
    setViewer(false);
    scrollerRef.current?.scrollTo(0, 0);
  }, [result.id]);

  /** The cell under the pointer; `"row"` as column for the row-number gutter. With `clamp`,
   *  positions outside the data snap to the nearest cell (while dragging). */
  function cellAt(clientX: number, clientY: number, clamp = false): (Cell & { gutter: boolean }) | null {
    const scroller = scrollerRef.current;
    if (!scroller || result.rows === 0 || widths.length === 0) return null;
    const rect = scroller.getBoundingClientRect();
    const x = clientX - rect.left;
    const y = clientY - rect.top;
    if (!clamp && (y < HEADER_H || x > scroller.clientWidth || y > scroller.clientHeight)) return null;
    const row = Math.min(result.rows - 1, Math.max(0, Math.floor((y - HEADER_H + scroller.scrollTop) / ROW_H)));
    if (!clamp && Math.floor((y - HEADER_H + scroller.scrollTop) / ROW_H) >= result.rows) return null;
    const gutter = x < rowNumW;
    const dataX = x - rowNumW + scroller.scrollLeft;
    let col = offsets.findIndex((o, i) => i < widths.length && dataX >= o && dataX < (offsets[i + 1] ?? 0));
    if (col < 0) col = dataX < 0 ? 0 : widths.length - 1;
    return { row, col, gutter };
  }

  /** Applies the column conditions in the core (an empty list removes them). */
  async function applyColumnFilters(filters: ColumnFilterItem[]) {
    setMenu(null);
    setCustomFilter(null);
    if (!result.complete) {
      setNotice({ text: t("results.filterWhenComplete"), error: true });
      return;
    }
    try {
      const outcome = await call(commands.filterColumns(result.id, filters));
      onColumnFiltered(filters, outcome.rows, outcome.total);
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  /** Adds a condition to those in force, unless it is already there. */
  function addColumnFilter(filter: ColumnFilterItem) {
    const current = result.columnFilters ?? [];
    const same = (f: ColumnFilterItem) => f.column === filter.column && f.op === filter.op && f.value === filter.value;
    void applyColumnFilters(current.some(same) ? current : [...current, filter]);
  }

  /** The column whose right header edge is under the pointer, if any (to resize it). */
  function edgeAt(clientX: number, clientY: number): number | null {
    const scroller = scrollerRef.current;
    if (!scroller) return null;
    const rect = scroller.getBoundingClientRect();
    const x = clientX - rect.left;
    const y = clientY - rect.top;
    if (y < 0 || y >= HEADER_H || x < rowNumW) return null;
    const dataX = x - rowNumW + scroller.scrollLeft;
    for (let col = 0; col < widths.length; col++) {
      if (Math.abs((offsets[col + 1] ?? 0) - dataX) <= EDGE) return col;
    }
    return null;
  }

  /** Width that shows the header and the loaded cells of a column in full (bounded). */
  function fitWidth(col: number): number {
    const ctx = canvasRef.current?.getContext("2d");
    const meta = result.columns[col];
    if (!ctx || !meta) return widths[col] ?? MIN_RESIZE;
    ctx.font = HEADER_FONT;
    let widest = ctx.measureText(meta.name).width + 14;
    ctx.font = TYPE_FONT;
    widest = Math.max(widest, ctx.measureText(meta.nativeType).width);
    ctx.font = DATA_FONT;
    const scroller = scrollerRef.current;
    const first = scroller ? Math.floor(scroller.scrollTop / ROW_H) : 0;
    const last = Math.min(result.rows, first + Math.ceil((size.h || 600) / ROW_H) + 1);
    for (let row = first; row < last; row++) {
      const value = cache.get(row, col);
      const text = value === null ? "NULL" : value === undefined ? "" : value.replace(/[\r\n\t]+/g, " ").slice(0, 300);
      widest = Math.max(widest, ctx.measureText(text).width);
    }
    return Math.round(Math.min(MAX_RESIZE, Math.max(MIN_RESIZE, widest + PAD * 2 + 2)));
  }

  /** The column whose header is under the pointer, if any. */
  function headerAt(clientX: number, clientY: number): number | null {
    const scroller = scrollerRef.current;
    if (!scroller || widths.length === 0) return null;
    const rect = scroller.getBoundingClientRect();
    const x = clientX - rect.left;
    const y = clientY - rect.top;
    if (y < 0 || y >= HEADER_H || x < rowNumW || x > scroller.clientWidth) return null;
    const dataX = x - rowNumW + scroller.scrollLeft;
    const col = offsets.findIndex((o, i) => i < widths.length && dataX >= o && dataX < (offsets[i + 1] ?? 0));
    return col < 0 ? null : col;
  }

  /** Sorts in the core and asks for the rows again; `undefined` restores the server order. */
  // Clicks can come faster than the core answers: the next click cycles from the last sort
  // asked for, and the requests reach the core one after another, in order.
  const requestedSort = useRef(result.sort);
  const sortQueue = useRef<Promise<void>>(Promise.resolve());
  useEffect(() => {
    requestedSort.current = serverSort ? serverSort.sort : result.sort;
    // Only when another result is shown; within one result the ref leads.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result.id]);

  function sortBy(sort: SortState | undefined) {
    setMenu(null);
    if (serverSort) {
      requestedSort.current = sort;
      serverSort.onSort(sort);
      return;
    }
    if (!result.complete) {
      setNotice({ text: t("results.sortWhenComplete"), error: true });
      return;
    }
    const previous = requestedSort.current;
    requestedSort.current = sort;
    sortQueue.current = sortQueue.current.then(async () => {
      try {
        await call(commands.sortResult(result.id, sort?.column ?? null, sort?.descending ?? false));
        onSorted(sort);
      } catch (e) {
        requestedSort.current = previous;
        setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
      }
    });
  }

  /** Header clicks cycle: ascending, descending, server order. */
  function cycleSort(col: number) {
    const current = requestedSort.current;
    if (current?.column !== col) sortBy({ column: col, descending: false });
    else if (!current.descending) sortBy({ column: col, descending: true });
    else sortBy(undefined);
  }

  /** Moves the active cell to `col` in the same row (the first row if none) and shows it. */
  function jumpTo(col: number) {
    setJump(null);
    if (result.rows === 0) return;
    const cell = { row: selection?.focus.row ?? 0, col };
    setSelection({ anchor: cell, focus: cell });
    scrollIntoView(cell);
    scrollerRef.current?.focus();
  }

  function scrollIntoView(cell: Cell) {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const y = cell.row * ROW_H;
    if (y < scroller.scrollTop) scroller.scrollTop = y;
    else if (y + ROW_H > scroller.scrollTop + scroller.clientHeight - HEADER_H)
      scroller.scrollTop = y + ROW_H - scroller.clientHeight + HEADER_H;
    const x = offsets[cell.col] ?? 0;
    const w = widths[cell.col] ?? 0;
    if (x < scroller.scrollLeft) scroller.scrollLeft = x;
    else if (x + w > scroller.scrollLeft + scroller.clientWidth - rowNumW)
      scroller.scrollLeft = x + w - scroller.clientWidth + rowNumW;
  }

  function selectAt(e: React.MouseEvent | MouseEvent, extend: boolean) {
    const cell = cellAt(e.clientX, e.clientY, extend);
    if (!cell) return;
    const last = widths.length - 1;
    setSelection((current) => {
      if (cell.gutter) {
        // The row-number gutter selects whole rows.
        const anchor = extend && current ? { row: current.anchor.row, col: 0 } : { row: cell.row, col: 0 };
        return { anchor, focus: { row: cell.row, col: last } };
      }
      return extend && current ? { anchor: current.anchor, focus: cell } : { anchor: cell, focus: cell };
    });
  }

  async function copy(format: TextFormat, header: boolean) {
    setMenu(null);
    if (!selection) return;
    const b = bounds(selection);
    const range: CellRange = { firstRow: b.r0, rows: b.r1 - b.r0 + 1, firstColumn: b.c0, columns: b.c1 - b.c0 + 1 };
    try {
      const text = await call(commands.copyRows(result.id, session, range, format, header));
      await navigator.clipboard.writeText(text);
      setNotice({
        text: t(range.columns === 1 ? "results.copiedOneColumn" : "results.copied", {
          count: range.rows,
          formattedCount: numbers.format(range.rows),
          columns: numbers.format(range.columns),
        }),
        error: false,
      });
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), notice.error ? 6000 : 2000);
    return () => clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    if (!menu) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && (e.target as Element | null)?.closest(".grid-menu")) return;
      setMenu(null);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [menu]);

  useLayoutEffect(() => {
    if (!menu || !menuRef.current) return;
    const position = () => { if (menuRef.current) placeMenu(menuRef.current, menu.x, menu.y); };
    position();
    window.addEventListener("resize", position);
    return () => window.removeEventListener("resize", position);
  }, [menu]);

  function onKeyDown(e: React.KeyboardEvent) {
    const mod = e.ctrlKey || e.metaKey;
    if (e.key === "Escape" && viewer) {
      e.preventDefault();
      setViewer(false);
      return;
    }
    if (e.key === "Enter" && selection) {
      e.preventDefault();
      setViewer(true);
      return;
    }
    if (editing && e.key === "F2" && selection) {
      e.preventDefault();
      startEdit(selection.focus);
      return;
    }
    if (editing && e.key === "Delete" && selection) {
      e.preventDefault();
      editing.onDelete(selectedRows());
      return;
    }
    if (mod && e.shiftKey && e.key.toLowerCase() === "j" && widths.length > 0) {
      e.preventDefault();
      setJump("");
      return;
    }
    if (mod && e.key.toLowerCase() === "a" && result.rows > 0) {
      e.preventDefault();
      setSelection({ anchor: { row: 0, col: 0 }, focus: { row: result.rows - 1, col: widths.length - 1 } });
      return;
    }
    if (!selection) return;
    if (mod && e.key.toLowerCase() === "c") {
      e.preventDefault();
      void copy("tsv", e.shiftKey);
      return;
    }
    const moves: Record<string, [number, number]> = {
      ArrowUp: [-1, 0],
      ArrowDown: [1, 0],
      ArrowLeft: [0, -1],
      ArrowRight: [0, 1],
    };
    const move = moves[e.key];
    if (!move) return;
    e.preventDefault();
    const next = {
      row: Math.min(result.rows - 1, Math.max(0, selection.focus.row + move[0])),
      col: Math.min(widths.length - 1, Math.max(0, selection.focus.col + move[1])),
    };
    // Shift extends the selection; plain arrows move a single cell.
    setSelection(e.shiftKey ? { anchor: selection.anchor, focus: next } : { anchor: next, focus: next });
    scrollIntoView(next);
  }

  useEffect(() => {
    const move = (e: MouseEvent) => {
      const resize = resizing.current;
      if (resize && e.buttons === 1) {
        const width = Math.round(Math.min(MAX_RESIZE, Math.max(MIN_RESIZE, resize.startWidth + e.clientX - resize.startX)));
        setResized((r) => (r[resize.col] === width ? r : { ...r, [resize.col]: width }));
        return;
      }
      if (dragging.current && e.buttons === 1) selectAt(e, true);
    };
    const up = () => {
      dragging.current = false;
      resizing.current = null;
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
  });

  const focusMeta = selection ? result.columns[selection.focus.col] : undefined;
  return (
    <div className="grid-wrap">
    <div className="grid-main">
    {(result.columnFilters?.length ?? 0) > 0 && (
      <div className="filter-bar" role="toolbar" aria-label={t("results.columnFilters")}>
        <span className="muted small">{t("results.filteredBy")}</span>
        {(result.columnFilters ?? []).map((f, i) => (
          <span key={i} className="filter-chip">
            <code>{filterLabel(f, result.columns)}</code>
            <button
              className="ghost small"
              aria-label={t("results.removeFilter", { filter: filterLabel(f, result.columns) })}
              onClick={() => void applyColumnFilters((result.columnFilters ?? []).filter((_, j) => j !== i))}
            >
              ×
            </button>
          </span>
        ))}
        <button className="ghost small" onClick={() => void applyColumnFilters([])}>
          {t("results.resetFilters")}
        </button>
      </div>
    )}
    <div className="grid-host">
      <canvas ref={canvasRef} className="grid-canvas" style={{ width: size.w, height: size.h }} />
      <div
        ref={scrollerRef}
        className="grid-scroller"
        tabIndex={0}
        onScroll={() => { redraw(); setMenu(null); }}
        onMouseDown={(e) => {
          if (e.button !== 0) return;
          const edge = edgeAt(e.clientX, e.clientY);
          if (edge !== null) {
            e.preventDefault();
            resizing.current = { col: edge, startX: e.clientX, startWidth: widths[edge] ?? MIN_RESIZE };
            return;
          }
          const header = headerAt(e.clientX, e.clientY);
          if (header !== null) {
            e.preventDefault();
            cycleSort(header);
            return;
          }
          dragging.current = true;
          selectAt(e, e.shiftKey);
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          const cell = cellAt(e.clientX, e.clientY);
          if (!cell) return;
          // Right-click outside the selection selects that cell first.
          if (!selection || (!cell.gutter && !inside(bounds(selection), cell.row, cell.col))) selectAt(e, false);
          setMenu({ x: e.clientX, y: e.clientY, cell: cell.gutter ? null : cell });
        }}
        onMouseMove={(e) => {
          e.currentTarget.style.cursor =
            resizing.current || edgeAt(e.clientX, e.clientY) !== null
              ? "col-resize"
              : headerAt(e.clientX, e.clientY) === null
                ? ""
                : "pointer";
        }}
        onDoubleClick={(e) => {
          const edge = edgeAt(e.clientX, e.clientY);
          if (edge !== null) {
            // The two clicks were a sort pair or a resize; the double-click fits the column.
            setResized((r) => ({ ...r, [edge]: fitWidth(edge) }));
            return;
          }
          const cell = cellAt(e.clientX, e.clientY);
          if (cell && !cell.gutter) {
            if (editing) {
              startEdit(cell);
              return;
            }
            setSelection({ anchor: cell, focus: cell });
            setViewer(true);
          }
        }}
        onKeyDown={onKeyDown}
        data-column-widths={widths.join(",")}
        aria-label={
          t("results.gridLabel", { count: result.rows, formattedCount: numbers.format(result.rows) }) +
          (sortShown
            ? `, ${t("results.gridSorted", {
                column: result.columns[sortShown.column]?.name ?? "",
                direction: t(sortShown.descending ? "results.descending" : "results.ascending"),
              })}`
            : "")
        }
      >
        <div style={{ width: totalW, height: totalH }} />
      </div>
      {menu && (
        <div ref={menuRef} className="grid-menu result-grid-menu" role="menu" style={{ left: menu.x, top: menu.y }}>
          {editing && menu.cell && (
            <>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  if (menu.cell) startEdit(menu.cell);
                }}
              >
                {t("edit.editCell")}
              </button>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  if (menu.cell) editing.onEdit(menu.cell.row, menu.cell.col, null);
                }}
              >
                {t("edit.setNull")}
              </button>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  editing.onDelete(selectedRows());
                }}
              >
                {t("edit.deleteRows", { count: selectedRows().length })}
              </button>
              {selectedRows().some((r) => editing.deleted.includes(r) || Object.keys(editing.cells).some((k) => k.startsWith(`${r}:`))) && (
                <button
                  role="menuitem"
                  onClick={() => {
                    setMenu(null);
                    editing.onRevert(selectedRows());
                  }}
                >
                  {t("edit.revert")}
                </button>
              )}
              <hr />
            </>
          )}
          <button
            role="menuitem"
            onClick={() => {
              setMenu(null);
              setViewer(true);
            }}
          >
            {t("viewer.open")}
          </button>
          <hr />
          {COPY_ITEMS.map((item) => (
            <button key={item.key} role="menuitem" onClick={() => void copy(item.format, item.header)}>
              {t(`results.${item.key}`)}
            </button>
          ))}
          {menu.cell && !(editing && (editing.deleted.length > 0 || Object.keys(editing.cells).length > 0)) && (
            <>
              <hr />
              <FilterSubmenu
                cell={menu.cell}
                value={cache.get(menu.cell.row, menu.cell.col)}
                columns={result.columns}
                active={result.columnFilters ?? []}
                onAdd={addColumnFilter}
                onCustom={(start) => {
                  setMenu(null);
                  setCustomFilter(start);
                }}
                onReset={() => void applyColumnFilters([])}
              />
            </>
          )}
          {selection && (
            <>
              <hr />
              <button role="menuitem" onClick={() => sortBy({ column: selection.focus.col, descending: false })}>
                {t("results.sortAsc")}
              </button>
              <button role="menuitem" onClick={() => sortBy({ column: selection.focus.col, descending: true })}>
                {t("results.sortDesc")}
              </button>
              {sortShown && (
                <button role="menuitem" onClick={() => sortBy(undefined)}>
                  {t("results.sortClear")}
                </button>
              )}
            </>
          )}
        </div>
      )}
      {cellEditor && (
        <input
          className="cell-editor"
          aria-label={t("edit.cellLabel", { column: result.columns[cellEditor.col]?.name ?? "" })}
          autoFocus
          spellCheck={false}
          style={{
            left: rowNumW + (offsets[cellEditor.col] ?? 0) - (scrollerRef.current?.scrollLeft ?? 0),
            top: HEADER_H + cellEditor.row * ROW_H - (scrollerRef.current?.scrollTop ?? 0),
            width: widths[cellEditor.col] ?? MIN_COL,
            height: ROW_H,
          }}
          value={cellEditor.value}
          onChange={(e) => setCellEditor({ ...cellEditor, value: e.target.value })}
          onFocus={(e) => e.currentTarget.select()}
          onBlur={(e) => commitEdit(e.currentTarget.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") commitEdit(e.currentTarget.value);
            else if (e.key === "Escape") {
              setCellEditor(null);
              scrollerRef.current?.focus();
            } else return;
            e.preventDefault();
            e.stopPropagation();
          }}
        />
      )}
      {jump !== null && (
        <ColumnJump
          columns={result.columns.map((c) => c.name)}
          search={jump}
          onSearch={setJump}
          onPick={jumpTo}
          onClose={() => {
            setJump(null);
            scrollerRef.current?.focus();
          }}
        />
      )}
      {notice && (
        <div className={`grid-notice ${notice.error ? "error" : ""}`} role="status">
          {notice.text}
        </div>
      )}
      {customFilter && (
        <CustomFilterDialog
          start={customFilter}
          columns={result.columns}
          onApply={addColumnFilter}
          onCancel={() => {
            setCustomFilter(null);
            scrollerRef.current?.focus();
          }}
        />
      )}
    </div>
    </div>
    {viewer && selection && focusMeta && (
      <CellViewer
        key={view}
        resultId={result.id}
        row={selection.focus.row}
        column={selection.focus.col}
        meta={focusMeta}
        columns={result.columns}
        mode={viewerMode}
        onMode={(mode) => {
          setViewerMode(mode);
          // Arrow keys keep moving through the grid.
          scrollerRef.current?.focus();
        }}
        onPickColumn={(col) => {
          const cell = { row: selection.focus.row, col };
          setSelection({ anchor: cell, focus: cell });
          scrollIntoView(cell);
          setViewerMode("cell");
          scrollerRef.current?.focus();
        }}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    )}
    </div>
  );
}

/** Column picker (Ctrl+Shift+J): names containing the typed text, in order; Enter picks the
 *  highlighted one. */
function ColumnJump(props: {
  columns: string[];
  search: string;
  onSearch(text: string): void;
  onPick(col: number): void;
  onClose(): void;
}) {
  const { t } = useTranslation("editor");
  const [active, setActive] = useState(0);
  const query = props.search.trim().toLowerCase();
  const matches = props.columns.map((name, col) => ({ name, col })).filter((c) => c.name.toLowerCase().includes(query));
  const current = Math.min(active, Math.max(0, matches.length - 1));
  return (
    <div className="column-jump" role="dialog" aria-label={t("results.jumpTitle")}>
      <input
        autoFocus
        value={props.search}
        placeholder={t("results.jumpPlaceholder")}
        aria-label={t("results.jumpTitle")}
        onChange={(e) => {
          props.onSearch(e.target.value);
          setActive(0);
        }}
        onBlur={props.onClose}
        onKeyDown={(e) => {
          if (e.key === "Escape") props.onClose();
          else if (e.key === "ArrowDown") setActive(Math.min(current + 1, matches.length - 1));
          else if (e.key === "ArrowUp") setActive(Math.max(current - 1, 0));
          else if (e.key === "Enter") {
            const pick = matches[current];
            if (pick) props.onPick(pick.col);
          } else return;
          e.preventDefault();
        }}
      />
      <ul role="listbox">
        {matches.slice(0, 50).map((m, i) => (
          <li
            key={m.col}
            role="option"
            aria-selected={i === current}
            className={i === current ? "active" : ""}
            // Before the input's blur closes the picker.
            onMouseDown={(e) => {
              e.preventDefault();
              props.onPick(m.col);
            }}
          >
            {m.name}
          </li>
        ))}
        {matches.length === 0 && <li className="muted">{t("results.jumpNone")}</li>}
      </ul>
    </div>
  );
}

/** "Filter ▸" in the grid's context menu, from the cell it was opened on (like SQLyog). */
function FilterSubmenu(props: {
  cell: Cell;
  value: string | null | undefined;
  columns: ResultMeta["columns"];
  active: ColumnFilterItem[];
  onAdd(filter: ColumnFilterItem): void;
  onCustom(start: ColumnFilterItem): void;
  onReset(): void;
}) {
  const { t } = useTranslation("editor");
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const submenu = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (!open || !trigger.current || !submenu.current) return;
    const position = () => {
      if (!trigger.current || !submenu.current) return;
      const anchor = trigger.current.getBoundingClientRect();
      const width = submenu.current.getBoundingClientRect().width;
      const right = anchor.right;
      const x = right + width <= window.innerWidth - MENU_MARGIN ? right : anchor.left - width;
      placeMenu(submenu.current, x, anchor.top);
    };
    position();
    window.addEventListener("resize", position);
    window.addEventListener("scroll", position, true);
    return () => {
      window.removeEventListener("resize", position);
      window.removeEventListener("scroll", position, true);
    };
  }, [open, props.value, props.active.length, props.columns]);
  const column = props.cell.col;
  const value = props.value;
  const items: ColumnFilterItem[] =
    value === null
      ? [
          { column, op: "isNull", value: "" },
          { column, op: "isNotNull", value: "" },
        ]
      : value === undefined
        ? []
        : [
            ...(["eq", "notEq", "gt", "lt"] as const).map((op) => ({ column, op, value })),
            { column, op: "like" as const, value: likeContaining(value) },
            { column, op: "notLike" as const, value: likeContaining(value) },
            { column, op: "isNull" as const, value: "" },
            { column, op: "isNotNull" as const, value: "" },
          ];
  return (
    <div className="grid-submenu" onMouseEnter={() => setOpen(true)} onMouseLeave={(e) => {
      if (!e.currentTarget.contains(document.activeElement)) setOpen(false);
    }} onFocus={() => setOpen(true)} onBlur={(e) => {
      if (!e.currentTarget.contains(e.relatedTarget)) setOpen(false);
    }}>
      <button ref={trigger} role="menuitem" aria-haspopup="menu" aria-expanded={open}>
        {t("results.filterMenu")} ▸
      </button>
      {open && <div ref={submenu} className="grid-menu grid-filter-menu" role="menu" aria-label={t("results.filterMenu")}>
        {items.map((filter) => (
          <button key={filter.op} role="menuitem" onClick={() => props.onAdd(filter)}>
            <code>{filterLabel(filter, props.columns)}</code>
          </button>
        ))}
        <hr />
        <button role="menuitem" onClick={() => props.onCustom({ column, op: value === null ? "isNull" : "eq", value: value ?? "" })}>
          {t("results.customFilter")}
        </button>
        {props.active.length > 0 && (
          <button role="menuitem" onClick={props.onReset}>
            {t("results.resetFilters")}
          </button>
        )}
      </div>}
    </div>
  );
}

/** One condition chosen by hand: column, operator and value. */
function CustomFilterDialog(props: {
  start: ColumnFilterItem;
  columns: ResultMeta["columns"];
  onApply(filter: ColumnFilterItem): void;
  onCancel(): void;
}) {
  const { t } = useTranslation("editor");
  const [filter, setFilter] = useState(props.start);
  const needsValue = takesValue(filter.op);
  const apply = () => props.onApply(needsValue ? filter : { ...filter, value: "" });
  return (
    <div className="modal-backdrop">
      <div className="modal custom-filter" role="dialog" aria-modal="true" aria-label={t("results.customFilterTitle")}>
        <h2>{t("results.customFilterTitle")}</h2>
        <div
          className="custom-filter-row"
          onKeyDown={(e) => {
            if (e.key === "Enter") apply();
            if (e.key === "Escape") props.onCancel();
          }}
        >
          <select
            aria-label={t("results.filterColumn")}
            value={filter.column}
            onChange={(e) => setFilter({ ...filter, column: Number(e.target.value) })}
          >
            {props.columns.map((c, i) => (
              <option key={i} value={i}>
                {c.name}
              </option>
            ))}
          </select>
          <select
            aria-label={t("results.filterOperator")}
            value={filter.op}
            onChange={(e) => setFilter({ ...filter, op: e.target.value as FilterOpItem })}
          >
            {OPS.map((o) => (
              <option key={o.op} value={o.op}>
                {o.sql}
              </option>
            ))}
          </select>
          <input
            autoFocus
            aria-label={t("results.filterValue")}
            value={needsValue ? filter.value : ""}
            disabled={!needsValue}
            onChange={(e) => setFilter({ ...filter, value: e.target.value })}
            spellCheck={false}
          />
        </div>
        <p className="muted small">{t("results.customFilterHint")}</p>
        <div className="modal-actions">
          <button onClick={props.onCancel}>{t("results.cancel")}</button>
          <button className="primary" onClick={apply}>
            {t("results.applyFilter")}
          </button>
        </div>
      </div>
    </div>
  );
}
