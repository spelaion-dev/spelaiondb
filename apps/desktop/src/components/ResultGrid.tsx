// Canvas result grid (ADR-010). Only what is visible is painted; scrolling is native
// (a container with a "spacer" of the full size) and the canvas is redrawn on top. Past the
// browser's height cap the spacer stops growing and its scroll is scaled to the rows
// (`grid/scroll.ts`, SDB-109): every row position read or set goes through `contentTop` and
// `scrollToContent`, never the container's `scrollTop`.

import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { call, commands, type CopySpecialItem, type UpdateSourceItem, type BlockItem, type BulkActionItem, type CellEditItem, type CellRange, type ColumnFilterItem, type FilterOpItem, type KeyTargetItem, type NameStyleItem, type TableKeyItem, type TextFormat } from "../api";
import { startJob, type Job } from "../jobs";
import { readClipboardText } from "../clipboardText";
import { openTableData } from "../connect";
import { THEME_CHANGE_EVENT, onPixelRatioChange, usePreferences } from "../preferences";
import { FindCache } from "../grid/findCache";
import { RowCache, type ValueKind } from "../grid/rowCache";
import { printableGridKey } from "../gridTyping";
import { formatsValues, useDisplayFormat } from "../displayFormat";
import { isScaled, maxContentTop, scrollMap, toContent, toScroll, wheelPixels } from "../grid/scroll";
import { GRID_COMMAND_EVENT, useDialogs, type FilterMode, type GridCommand, type ResultMeta, type SortState } from "../store";
import { commandKeys, commandSpec, KEYS, matchesShortcut, type KeyId } from "../commandRegistry";
import { commandShortcut, keyText, shortcutText, withShortcut } from "../shortcuts";
import { useExport } from "./ExportDialog";
import { CellViewer, ViewerModes, chooseValueFile, saveValueToFile, type ViewerMode } from "./CellViewer";
import { Icon, PATHS } from "./Icons";
import { ReferencesPanel } from "./ReferencesPanel";
import { CalcPanel } from "./CalcPanel";
import { GroupPanel } from "./GroupPanel";
import { ColumnStatsPopover } from "./ColumnStatsPopover";
import { CopySpecialDialog } from "./CopySpecialDialog";
import { RevealPopover } from "./RevealPopover";
import { FindBar } from "./GridFind";
import { InsertRowDialog, PAGE_SIZES, PageControls } from "./TableDataBar";
import { placeMenu } from "../menuPlacement";
import { Submenu } from "./Submenu";
import { ended, partialText } from "../resultEnd";
import { errorText, type Loaded } from "../dialogState";

/** No sort: one list, so an unsorted result keeps the same value between renders. */
const NO_SORT: SortState[] = [];

/** Whether two sorts have the same keys in the same order. */
function sameSort(a: SortState[], b: SortState[]): boolean {
  return a.length === b.length && a.every((key, i) => key.column === b[i]?.column && key.descending === b[i]?.descending);
}

// The grid's own font size is a personal setting (SDB-385): every grid shares it, so the row
// height and the fonts that follow it are set once per render by `setGridFont`.
let ROW_H = 24;
const HEADER_H = 34;
const PAD = 6;
const MIN_COL = 72;
const MAX_COL = 420;
/** Limits when the user resizes a column (wider than the automatic maximum). */
const MIN_RESIZE = 48;
const MAX_RESIZE = 2000;
/** Pixels on each side of a header edge that grab it. */
const EDGE = 4;
/** The size of the grid's values in pixels, the user's setting (SDB-385). */
let GRID_FONT_PX = 12;
/** Sets the size of the grid's values, in pixels; a row is that plus the usual room. */
function setGridFont(size: number) {
  GRID_FONT_PX = Math.min(20, Math.max(10, Math.round(size) || 12));
  ROW_H = GRID_FONT_PX + 12;
}

/** What the canvas draws with, from the theme's tokens (SDB-75). */
interface GridTheme {
  bg: string;
  stripe: string;
  line: string;
  header: string;
  text: string;
  muted: string;
  select: string;
  accent: string;
  vNumber: string;
  vDate: string;
  vBool: string;
  vText: string;
  vNull: string;
  /** An edited cell, the cells a search found and the one it is on, and a row to delete. */
  edited: string;
  found: string;
  foundCurrent: string;
  deleted: string;
  /** The column's name, and the line under it. */
  headerFont: string;
  typeFont: string;
  /** The values' family; their size is the user's (`GRID_FONT_PX`). */
  family: string;
  /** The row numbers never grow past this size, in pixels. */
  rowNumberMax: number;
}
let gridTheme: { key: string; theme: GridTheme } | null = null;
/** The theme's tokens, read once and again only when the theme or the interface size changes:
 *  never per frame (a computed style per frame costs a style pass while scrolling). The key is
 *  read on every draw, but it touches no style. */
function currentGridTheme(): GridTheme {
  const key = `${document.documentElement.dataset.theme ?? ""}|${window.devicePixelRatio || 1}`;
  if (gridTheme?.key === key) return gridTheme.theme;
  const style = getComputedStyle(document.documentElement);
  const token = (name: string) => style.getPropertyValue(name).trim();
  const family = token("--font-ui");
  const theme: GridTheme = {
    bg: token("--grid-bg"),
    stripe: token("--grid-stripe"),
    line: token("--grid-line"),
    header: token("--grid-header"),
    text: token("--grid-text"),
    muted: token("--muted"),
    select: token("--accent-soft"),
    accent: token("--accent"),
    vNumber: token("--value-number"),
    vDate: token("--value-date"),
    vBool: token("--value-bool"),
    vText: token("--value-text"),
    vNull: token("--value-null"),
    edited: token("--grid-edited"),
    found: token("--grid-found"),
    foundCurrent: token("--grid-found-current"),
    deleted: token("--grid-deleted"),
    headerFont: `600 ${token("--text-sm")} ${family}`,
    typeFont: `400 ${token("--text-xs")} ${family}`,
    family,
    rowNumberMax: parseFloat(token("--text-md")) || GRID_FONT_PX,
  };
  gridTheme = { key, theme };
  return theme;
}
const dataFont = (theme: GridTheme) => `400 ${GRID_FONT_PX}px ${theme.family}`;
const rowNumberFont = (theme: GridTheme) => `400 ${Math.min(GRID_FONT_PX, theme.rowNumberMax)}px ${theme.family}`;
/** The token that colors each kind of value (SDB-385), when the user asks for it. */
const KIND_TOKEN: Partial<Record<ValueKind, "vNumber" | "vDate" | "vBool" | "vText">> = {
  number: "vNumber",
  date: "vDate",
  time: "vDate",
  timestamp: "vDate",
  timestamptz: "vDate",
  bool: "vBool",
  text: "vText",
};
/** A column's kind before its first rows arrive, from its type. */
function kindOf(logical: string): ValueKind {
  if (NUMERIC.has(logical)) return "number";
  if (logical === "date" || logical === "time" || logical === "timestamp" || logical === "timestamptz" || logical === "bool" || logical === "text") return logical;
  return "other";
}
const NUMERIC = new Set(["int", "float", "decimal"]);
/** Texts of a boolean cell, as each engine's text reads (`t`, `true`, a `bit`'s `1`). */
const BOOL_TRUE = new Set(["t", "true", "1"]);
const BOOL_FALSE = new Set(["f", "false", "0"]);
const CHECK = 12;
/** The link drawn at the right of a foreign key cell: a click on it opens the referenced row. */
const KEY_ICON = 12;
const KEY_GAP = 4;
/** The grid's own key for following a foreign key (Ctrl+click does it too). */
const FOLLOW_SHORTCUT = KEYS.followReference.shortcut;
/** A column's sort direction in its header: Lucide's arrow at this size, then its place among
 *  several sort keys. */
const SORT_ICON = 12;
const pathCache = new Map<keyof typeof PATHS, Path2D[]>();

/** One of Lucide's icons from `PATHS` at `size` pixels, its top-left corner at (x, y). */
function drawPaths(ctx: CanvasRenderingContext2D, name: keyof typeof PATHS, x: number, y: number, size: number, color: string) {
  let paths = pathCache.get(name);
  if (!paths) {
    paths = PATHS[name].map((d) => new Path2D(d));
    pathCache.set(name, paths);
  }
  ctx.save();
  ctx.translate(x, y);
  ctx.scale(size / 24, size / 24);
  ctx.strokeStyle = color;
  ctx.lineWidth = 2;
  ctx.lineCap = "round";
  ctx.lineJoin = "round";
  for (const path of paths) ctx.stroke(path);
  ctx.restore();
}

/** Lucide's arrow-up-right at `KEY_ICON` pixels, its top-left corner at (x, y). */
function drawKeyIcon(ctx: CanvasRenderingContext2D, x: number, y: number, color: string) {
  drawPaths(ctx, "arrowUpRight", x, y, KEY_ICON, color);
}

interface Cell {
  row: number;
  col: number;
}

/** A rectangle from the cell where the selection started (`anchor`) to the active one. */
interface Selection {
  /** Individual whole rows in display order, when the selection is discontinuous. */
  rows?: number[];
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
/** The copy defaults "Copy special…" opens with (SDB-388), as the core keeps them. */
const loadCopyDefaults = (): Promise<CopySpecialItem> => call(commands.copyDefaults());

const COPY_ITEMS: { key: string; format: TextFormat; header: boolean; shortcut?: KeyId }[] = [
  { key: "copy", format: "tsv", header: false, shortcut: "copy" },
  { key: "copyWithHeaders", format: "tsv", header: true, shortcut: "copyWithHeaders" },
  { key: "copyCsv", format: "csv", header: true },
  { key: "copyJson", format: "json", header: false },
  { key: "copyMarkdown", format: "markdown", header: true },
  { key: "copyInList", format: "inList", header: false },
  { key: "copyHtml", format: "html", header: true },
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

/** Pending edits of a table data tab, as the core holds them (ADR-044): drawn over the page
 *  until applied. */
export interface GridEditing {
  /** `"row:col"` → the value typed (`null` is NULL). */
  cells: Record<string, string | null>;
  /** Keys of `cells` set to the column's default. */
  defaults: Set<string>;
  deleted: number[];
  inserts: CellEditItem[][];
  /** Rows with a pending value, not counting rows to delete. */
  updatedRows: number;
  canUndo: boolean;
  canRedo: boolean;
  primaryKey: string[];
  /** The table the rows are from: its columns may take a value picker in the cell panel. */
  table: { schema: string; name: string };
  busy: boolean;
  onInsert(values: CellEditItem[]): void;
  onUpdateInsert(index: number, values: CellEditItem[]): void;
  onRemoveInsert(index: number): void;
  onApply(): void;
  onApplyCell(row: number, col: number, value: string | undefined, dirty: boolean): void;
  /** Loads a file into one cell now: one UPDATE by primary key, written by the core. */
  onLoadFile(row: number, col: number, path: string): Promise<void>;
  onDiscard(): void;
  /** `undefined` takes the cell back to its value in the database. */
  onEdit(row: number, col: number, value: string | null | undefined): void;
  /** A keystroke in the cell panel's text box: typing on in one cell is one undo step. */
  onType(row: number, col: number, value: string | null | undefined): void;
  onDelete(rows: number[]): void;
  onRevert(rows: number[]): void;
  /** A bulk edit of the selection's block (its shown columns). */
  onBulk(block: BlockItem, action: BulkActionItem): void;
  /** Tab-separated text pasted on the selection; `columns` are the shown ones from its first. */
  onPaste(selection: BlockItem, columns: number[], text: string): void;
  onUndo(): void;
  onRedo(): void;
}

/** The grid's own keys for pending edits (undo and redo are app commands, `commandRegistry.ts`). */
const undoKeys = () => commandKeys(commandSpec("edits.undo")) ?? "";
const redoKeys = () => commandKeys(commandSpec("edits.redo")) ?? "";
const FILL_DOWN_SHORTCUT = KEYS.fillDown.shortcut;
const PASTE_SHORTCUT = KEYS.pasteCells.shortcut;
const SET_NULL_SHORTCUT = KEYS.setNull.shortcut;
const REVERT_SHORTCUT = KEYS.revertRows.shortcut;
/** "Go to row" (SDB-386): an app command the grid handles, as undo and redo. */
const goToRowKeys = () => commandKeys(commandSpec("results.goToRow")) ?? "";

/** Ways of copying column names, in menu order (qualified only with a known table). */
const NAME_STYLES: { style: NameStyleItem; key: string }[] = [
  { style: "plain", key: "copyNamesPlain" },
  { style: "quoted", key: "copyNamesQuoted" },
  { style: "qualified", key: "copyNamesQualified" },
  { style: "list", key: "copyNamesList" },
];

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
  { op: "contains", sql: "contains" },
  { op: "notContains", sql: "does not contain" },
  { op: "isNull", sql: "IS NULL" },
  { op: "isNotNull", sql: "IS NOT NULL" },
];
const takesValue = (op: FilterOpItem) => op !== "isNull" && op !== "isNotNull";

/** `Command <> 'Sleep'`, `Id > 10`, `Info IS NULL`: how a condition reads. */
function filterLabel(filter: ColumnFilterItem, columns: ResultMeta["columns"]): string {
  const column = columns[filter.column];
  const sql = OPS.find((o) => o.op === filter.op)?.sql ?? filter.op;
  if (!takesValue(filter.op)) return `${column?.name ?? "?"} ${sql}`;
  const numeric = NUMERIC.has(column?.logical ?? "") && !["like", "notLike", "contains", "notContains"].includes(filter.op);
  const value = numeric ? filter.value : `'${filter.value.replace(/'/g, "''")}'`;
  return `${column?.name ?? "?"} ${sql} ${value}`;
}


export function ResultGrid({
  result,
  session,
  onSorted,
  onColumnFiltered,
  serverSort,
  onQuerySort,
  editing,
  viewerOpen: viewer,
  onViewerOpen: setViewer,
  viewerWidth,
  onViewerWidth,
  pagingLocked,
  onLoadMore,
  onCountQuery,
  onQueryPageSize,
  tableData, keyTable, columnComments,
  onHiddenColumns,
  onChart,
  refresh,
  restoringView,
  rowOffset = 0,
  onGoToRow,
  onLoadRows,
  onCancelLoad,
  onSelectedRows,
  onRunAgain,
  onTranspose,
}: {
  result: ResultMeta;
  session: string;
  /** The core sorted the loaded rows by these keys (none: the order they arrived in). */
  onSorted(sort: SortState[]): void;
  onColumnFiltered(filters: ColumnFilterItem[], rows: number, total: number, mode?: FilterMode): void;
  /** In a table data tab, header clicks sort in the server (ORDER BY) instead. */
  serverSort?: { sort: SortState[]; onSort(sort: SortState[]): void };
  /** A paged query (ADR-054): reads it again from the server ordered by these keys. Resolves
   *  to `false` when the core cannot page it in that order; the grid then sorts the rows it
   *  has loaded and says so. */
  onQuerySort?(sort: SortState[]): Promise<boolean>;
  /** Editable table data tab: pending changes and how to change them. */
  editing?: GridEditing;
  viewerOpen: boolean;
  onViewerOpen(open: boolean): void;
  viewerWidth: number;
  onViewerWidth(width: number): void;
  pagingLocked: boolean;
  onLoadMore(): Promise<number>;
  onCountQuery(): void;
  onQueryPageSize(size: number): void;
  /** The columns hidden in this grid changed (for the export dialog). */
  onHiddenColumns?(hidden: number[]): void;
  /** "Chart selected columns": the first is the X axis, the numeric ones after it the values. */
  onChart?(columns: number[]): void;
  /** Refresh lives in the results toolbar and reads the grid's current page and sort state. */
  refresh?: { target: HTMLElement; disabled: boolean; run(row: number): void };
  restoringView?: boolean;
  /** A table data tab: its table, and the WHERE that "filter by this value" adds to (the
   *  core writes the condition; the page's SELECT is checked when it loads). Its foreign keys
   *  can be followed to new table data tabs of `connectionId`. */
  tableData?: { schema: string; name: string; filter: string; onFilter(filter: string): void; connectionId: string | null };
  /** The table whose foreign keys the rows follow, both ways (SDB-11): a table data tab's own, or
   *  the one table the core matched a query's result to (SDB-93). */
  keyTable?: { schema: string; name: string; connectionId: string | null };
  /** The comments of the columns, by name, where they are known (a table's data): shown in
   *  the header when the user asks for it. */
  columnComments?: Record<string, string>;
  /** Rows before the first one shown (a table data tab's page): row numbers start after it. */
  rowOffset?: number;
  /** "Go to row" (Ctrl+G, SDB-386) for a row this grid does not hold: another page of a table
   *  data tab, or a paged query's rows past the ones loaded. `row` counts from 1, as the grid
   *  numbers rows. */
  onGoToRow?(row: number): void;
  /** Loads a paged query's rows until it holds `until` (every row when `null`, after asking how
   *  many and how much memory); resolves to the rows held, or `null` if nothing was loaded. */
  onLoadRows?(until: number | null): Promise<number | null>;
  /** Stops a load of a paged query's rows. */
  onCancelLoad?(): void;
  /** The rows selected (positions in the view; the current row without a selection), for the
   *  results' Transpose button. */
  onSelectedRows?(rows: number[]): void;
  /** "Transpose selected rows": these rows of the view shown as columns. */
  onTranspose?(rows: number[]): void;
  /** Runs the query of a result the core no longer keeps (SDB-367). */
  onRunAgain?(): void;
}) {
  // The sort the headers show: the table data tab's, else the core's sort of the loaded rows,
  // else the one a paged query's pages were read with.
  const localSort = result.sort ?? NO_SORT;
  const sortShown = serverSort ? serverSort.sort : localSort.length > 0 ? localSort : result.paging?.sort ?? NO_SORT;
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const scrollerRef = useRef<HTMLDivElement>(null);
  /** The content position shown and the container position it was set at. While the scroll is
   *  scaled the container's own position is too coarse to keep it, so it is kept here; a
   *  container position other than `scroll` means the scroll bar was moved. */
  const scrollPos = useRef({ content: 0, scroll: 0 });
  /** The content's height (header and rows), for the helpers called outside the render. */
  const contentH = useRef(0);
  const accessibleCell = useRef<HTMLDivElement>(null);
  const accessibleCellId = useId();
  const wrapRef = useRef<HTMLDivElement>(null);
  const [wrapWidth, setWrapWidth] = useState(0);
  const viewerDrag = useRef<{ startX: number; startWidth: number } | null>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const frame = useRef<number | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  // The device pixel ratio the spacer was sized for: its height depends on it (`maxSpacerH`).
  const [, setPixelRatio] = useState(() => window.devicePixelRatio || 1);
  const [selection, setSelection] = useState<Selection | null>(null);
  const [queryPage, setQueryPage] = useState(0);
  const requestedPageScroll = useRef<number | null>(null);
  /** The context menu, with the cell it was opened on (for the Filter submenu), or the column
   *  whose header it was opened on. */
  const [menu, setMenu] = useState<{ x: number; y: number; cell: Cell | null; header?: number } | null>(null);
  /** Columns hidden in this result tab (in memory), by position in the result. */
  const [hidden, setHidden] = useState<ReadonlySet<number>>(() => new Set());
  /** The first shown column stays on the left while scrolling sideways. */
  const [frozen, setFrozen] = useState(false);
  /** The column chooser, where it was opened. */
  const [chooser, setChooser] = useState<{ x: number; y: number } | null>(null);
  /** A table data tab column's statistics popover, where it was opened. */
  const [columnStats, setColumnStats] = useState<{ x: number; y: number; column: number } | null>(null);
  /** A masked cell revealed on request (ADR-051): its value lives only in the popover. */
  const [reveal, setReveal] = useState<{ x: number; y: number; row: number; col: number } | null>(null);
  const gridLook = usePreferences((s) => s.grid);
  setGridFont(gridLook.fontSize);
  // How values are shown (SDB-385): the core formats each window; when the format changes
  // the rows are asked for again.
  const displayRevision = useDisplayFormat((s) => s.revision);
  const displayFormats = useDisplayFormat((s) => formatsValues(s.format));
  useEffect(() => { void useDisplayFormat.getState().load(); }, []);
  /** The find bar (Ctrl+F): what is searched, and what the core found. */
  const [find, setFind] = useState<{ text: string; matchCase: boolean } | null>(null);
  const [found, setFound] = useState<Loaded<{ count: number; truncated: boolean }>>({ kind: "idle" });
  const [findCurrent, setFindCurrent] = useState<{ index: number; row: number; col: number } | null>(null);
  const [findCache, setFindCache] = useState<FindCache | null>(null);
  const findInput = useRef<HTMLInputElement>(null);
  /** The match stepped to last, known before React renders it, and the steps in order: quick
   *  F3 presses each start from where the previous one ended. */
  const findAt = useRef<{ row: number; col: number } | null>(null);
  const findSteps = useRef<Promise<void>>(Promise.resolve());
  const menuRef = useRef<HTMLDivElement>(null);
  /** The custom filter dialog, starting from a column and a value. */
  const [customFilter, setCustomFilter] = useState<ColumnFilterItem | null>(null);
  /** The selection "Copy special…" copies, while its dialog is open (SDB-388). */
  const [copySpecial, setCopySpecial] = useState<CellRange | null>(null);
  /** `slow`: a sentence to read, kept on screen as long as an error. */
  const [notice, setNotice] = useState<{ text: string; error: boolean; slow?: boolean } | null>(null);
  /** The result the core said it no longer keeps (SDB-367). */
  const [freed, setFreed] = useState<number | null>(null);
  const dragging = useRef(false);
  const [viewerMode, setViewerMode] = useState<ViewerMode>("cell");
  /** The foreign keys of the rows' table (`keyTable`): its own and those of other tables that point to it. */
  const [keys, setKeys] = useState<TableKeyItem[]>([]);
  const keySchema = keyTable?.schema;
  const keyTableName = keyTable?.name;
  useEffect(() => {
    if (keySchema === undefined || keyTableName === undefined) {
      setKeys((current) => (current.length === 0 ? current : []));
      return;
    }
    let active = true;
    call(commands.tableKeys(session, keySchema, keyTableName))
      .then((found) => active && setKeys(found))
      .catch(() => active && setKeys([]));
    return () => {
      active = false;
    };
  }, [session, keySchema, keyTableName]);
  /** The result's columns that are part of a key of the table, with those keys. */
  const keyColumns = useMemo(() => {
    const byColumn = new Map<number, TableKeyItem[]>();
    const position = (name: string) => {
      const exact = result.columns.findIndex((c) => c.name === name);
      if (exact >= 0) return exact;
      const loose = result.columns.flatMap((c, i) => (c.name.toLowerCase() === name.toLowerCase() ? [i] : []));
      return loose.length === 1 ? (loose[0] ?? -1) : -1;
    };
    for (const key of keys) {
      if (key.direction !== "outgoing") continue;
      for (const column of key.columns) {
        const at = position(column);
        if (at >= 0) byColumn.set(at, [...(byColumn.get(at) ?? []), key]);
      }
    }
    return byColumn;
  }, [keys, result.columns]);
  const viewerModes: ViewerMode[] = keyTable && keys.length > 0 ? ["cell", "row", "references", "calc", "group"] : ["cell", "row", "calc", "group"];
  const shownMode: ViewerMode = viewerModes.includes(viewerMode) ? viewerMode : "cell";
  const referencesShown = viewer && shownMode === "references";
  const calcShown = viewer && shownMode === "calc";
  const groupShown = viewer && shownMode === "group";
  /** Counts the changes of the view (a sort, a filter, more rows). The groups belong to the
   *  view they were counted over: a sort and its undo leave the same rows in the same order,
   *  but the core's view is a newer one, so the groups are counted again. */
  const viewChanges = useRef({ key: "", count: 0 });
  /** The columns of the grouping panel, by position in the result (kept while the result is). */
  const [grouped, setGrouped] = useState<number[]>([]);
  /** A header pressed while the grouping panel is open: dragged to the panel it joins the
   *  grouping, released where it was it sorts as a click does. */
  const headerDrag = useRef<{ col: number; x: number; y: number; moved: boolean } | null>(null);
  const [headerGhost, setHeaderGhost] = useState<{ col: number; x: number; y: number } | null>(null);
  const [sorting, setSorting] = useState(false);
  const sortsPending = useRef(0);
  const [inserting, setInserting] = useState<{ values: CellEditItem[]; index?: number } | null>(null);
  /** The cell being typed into (table data tabs). */
  const [cellEditor, setCellEditor] = useState<{ row: number; col: number; value: string; original: string | null; dirty: boolean; typed: boolean } | null>(null);
  const skipEditorBlur = useRef(false);
  const editRequest = useRef(0);
  const typedWhileLoading = useRef<{ request: number; text: string } | null>(null);

  /** Opens the inline editor on a cell, with its pending value or the one loaded. */
  async function startEdit(cell: Cell, initial?: string) {
    if (!editing || editing.deleted.includes(cell.row) || editing.busy || sorting) return;
    const meta = result.columns[cell.col];
    if (meta?.masked) {
      setNotice({ text: t("masking.noEdit", { column: meta.name }), error: true });
      return;
    }
    const request = ++editRequest.current;
    typedWhileLoading.current = initial === undefined ? null : { request, text: initial };
    const key = `${cell.row}:${cell.col}`;
    setSelection({ anchor: cell, focus: cell });
    scrollIntoView(cell);
    let original = cache.get(cell.row, cell.col);
    if (original === undefined) {
      try { original = await call(commands.cellValue(result.id, cell.row, cell.col)); }
      catch (e) {
        if (request === editRequest.current) {
          typedWhileLoading.current = null;
          setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
        }
        return;
      }
    }
    if (request !== editRequest.current) return;
    const latest = live.current;
    if (latest.result.id !== result.id || !latest.editing || latest.editing.busy || latest.sorting || latest.editing.deleted.includes(cell.row) || latest.result.columns[cell.col]?.masked) {
      typedWhileLoading.current = null;
      return;
    }
    const current = key in latest.editing.cells ? latest.editing.cells[key] : original;
    const typed = typedWhileLoading.current?.request === request ? typedWhileLoading.current.text : undefined;
    typedWhileLoading.current = null;
    skipEditorBlur.current = false;
    setCellEditor({ row: cell.row, col: cell.col, value: typed ?? current ?? "", original, dirty: typed !== undefined, typed: typed !== undefined });
  }

  function commitEdit(value: string) {
    if (!editing || !cellEditor) return;
    const original = cellEditor.original;
    // Typing back the loaded value takes the cell back to it.
    if (cellEditor.dirty) editing.onEdit(cellEditor.row, cellEditor.col, original !== undefined && original !== null && value === original ? undefined : value);
    setCellEditor(null);
    scrollerRef.current?.focus();
  }

  /** Rows of the selection (the active row without one). */
  const selectedRows = (): number[] => {
    if (!selection) return [];
    if (selection.rows) return selection.rows;
    const b = bounds(selection);
    return Array.from({ length: b.r1 - b.r0 + 1 }, (_, i) => b.r0 + i);
  };

  async function duplicateRow() {
    if (!editing || !selection || editing.deleted.includes(selection.focus.row)) return;
    if (selection.rows) { setNotice({ text: t("results.disjointUnsupported"), error: true }); return; }
    try {
      const row = selection.focus.row;
      const values = await call(commands.rowValues(result.id, row));
      const initial = values.flatMap((value, column): CellEditItem[] => {
        const key = `${row}:${column}`;
        if (editing.primaryKey.includes(result.columns[column]?.name ?? "")) return [];
        if (result.columns[column]?.masked) return [];
        return [{ column, value: key in editing.cells ? editing.cells[key] ?? null : value }];
      });
      setInserting({ values: initial });
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  /** One cell switches between NULL and its value; a larger selection is set to NULL. */
  function setSelectedNull() {
    if (!editing || !selection) return;
    if (selection.rows) { setNotice({ text: t("results.disjointUnsupported"), error: true }); return; }
    const block = selectedBlock();
    if (block && (block.firstRow !== block.lastRow || block.columns.length > 1)) {
      editing.onBulk(block, { kind: "set", value: { kind: "null" } });
      return;
    }
    if (editing.deleted.includes(selection.focus.row)) return;
    const { row, col } = selection.focus;
    const original = cache.get(row, col);
    if (original === undefined) return;
    editing.onEdit(row, col, original === null ? undefined : null);
  }

  /** The selection's rows and shown columns. */
  function selectedBlock(): BlockItem | null {
    if (!selection) return null;
    if (selection.rows) { setNotice({ text: t("results.disjointUnsupported"), error: true }); return null; }
    const b = bounds(selection);
    const columns = visibleCols.filter((c) => c >= b.c0 && c <= b.c1);
    return columns.length ? { firstRow: b.r0, lastRow: b.r1, columns } : null;
  }

  function bulkEdit(action: BulkActionItem) {
    setMenu(null);
    const block = selectedBlock();
    if (editing && block && !editing.busy && !sorting) editing.onBulk(block, action);
    scrollerRef.current?.focus();
  }

  /** "Set value…": one value typed for every selected cell. */
  async function setSelectedValue() {
    setMenu(null);
    const block = selectedBlock();
    if (!editing || !block) return;
    const cells = (block.lastRow - block.firstRow + 1) * block.columns.length;
    const text = await useDialogs.getState().askText({
      title: t("edit.setValueTitle", { count: cells }),
      label: t("edit.setValueLabel"),
      initial: "",
      confirm: t("edit.setValueConfirm"),
    });
    scrollerRef.current?.focus();
    if (text !== null) editing.onBulk(block, { kind: "set", value: { kind: "text", text } });
  }

  /** Text pasted on the selection: the core lays it out and checks every value. */
  function pasteText(text: string) {
    const block = selectedBlock();
    if (!editing || !block || editing.busy || sorting) return;
    const from = visibleCols.indexOf(block.columns[0] ?? -1);
    editing.onPaste(block, from < 0 ? block.columns : visibleCols.slice(from), text);
  }

  async function pasteFromMenu() {
    setMenu(null);
    try {
      pasteText(await readClipboardText());
    } catch (e) {
      setNotice({ text: t("edit.pasteUnavailable", { detail: e instanceof Error ? e.message : String(e) }), error: true });
    }
    scrollerRef.current?.focus();
  }

  /** Ctrl+Shift+J: the column picker's search text (`null` when closed). */
  const [jump, setJump] = useState<string | null>(null);
  /** Ctrl+G: "Go to row" is open (SDB-386). */
  const [rowJump, setRowJump] = useState(false);

  /** Selects a whole row of the view, shows it and gives the grid the focus. */
  function selectRow(row: number) {
    const cell = { row, col: firstVisible };
    // The focus first: the grid's focus handler selects the first cell when it has no
    // selection, and it would replace the row set here.
    scrollerRef.current?.focus();
    // The whole row, the active cell on its first column.
    setSelection({ anchor: { row, col: lastVisible }, focus: { row, col: firstVisible } });
    scrollIntoView(cell);
  }

  /** "Go to row" (SDB-386): `row` counts from 1 as the grid numbers rows. A row this grid
   *  holds is selected here; another page of a table data tab, or a paged query's rows past the
   *  ones loaded, are the tab's to bring (`onGoToRow`), never guessed. */
  function goToRow(row: number) {
    setRowJump(false);
    const index = row - 1 - rowOffset;
    const reachable = !!onGoToRow && (!!tableData || (!!result.paging?.hasMore && !pageViewChanged));
    if (index >= 0 && index < result.rows) {
      selectRow(index);
      return;
    }
    if (reachable && onGoToRow) {
      onGoToRow(row);
      return;
    }
    if (result.rows === 0) return;
    setNotice({ text: t("goToRow.past", { rows: numbers.format(rowOffset + result.rows), row: numbers.format(row) }), error: false });
    selectRow(index < 0 ? 0 : result.rows - 1);
  }

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
  const view = `${localSort.map((key) => `${key.column}:${key.descending}`).join(",")}|${result.filter ?? ""}|${filtersKey}`;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const cache = useMemo(
    () =>
      new RowCache(result.id, () => redraw(), (error) => {
        // The core said why the rows could not be read (SDB-367): a freed result is shown where
        // the grid is, with Run again; any other failure in the grid's notice, once.
        switch (error.kind) {
          case "freed":
            setFreed(result.id);
            break;
          case "failed":
            setNotice({ text: t("results.rowsFailed", { detail: error.message }), error: true });
            break;
          default: {
            const unknown: never = error;
            return unknown;
          }
        }
      }),
    [result.id, view, redraw, displayRevision],
  );
  if (viewChanges.current.key !== `${view}|${result.rows}`) viewChanges.current = { key: `${view}|${result.rows}`, count: viewChanges.current.count + 1 };
  cache.grewTo(result.rows);

  // Widths: header and type, bounded. (Fitting to content: next iteration.)
  // Widths the user set by dragging (or fitted with a double-click), per column.
  const [resized, setResized] = useState<Record<number, number>>({});
  const resizing = useRef<{ col: number; startX: number; startWidth: number } | null>(null);
  const automatic = useMemo(() => {
    const ctx = document.createElement("canvas").getContext("2d");
    if (!ctx) return result.columns.map(() => 140);
    const theme = currentGridTheme();
    return result.columns.map((c) => {
      ctx.font = theme.headerFont;
      const name = ctx.measureText(c.name).width;
      ctx.font = theme.typeFont;
      const type = ctx.measureText(c.nativeType).width;
      const base = NUMERIC.has(c.logical) ? 96 : 132;
      return Math.round(Math.min(MAX_COL, Math.max(MIN_COL, name + PAD * 2 + 8, type + PAD * 2, base)));
    });
  }, [result.columns]);
  // A hidden column keeps its place with no width: nothing is drawn or hit there.
  const widths = useMemo(() => automatic.map((w, i) => (hidden.has(i) ? 0 : resized[i] ?? w)), [automatic, resized, hidden]);
  const visibleCols = useMemo(() => result.columns.map((_, i) => i).filter((i) => !hidden.has(i)), [result.columns, hidden]);
  const firstVisible = visibleCols[0] ?? 0;
  const lastVisible = visibleCols[visibleCols.length - 1] ?? 0;
  const frozenCol = frozen && visibleCols.length > 1 ? firstVisible : null;
  const frozenW = frozenCol === null ? 0 : (widths[frozenCol] ?? 0);
  const offsets = useMemo(() => {
    const out = [0];
    for (const w of widths) out.push((out[out.length - 1] ?? 0) + w);
    return out;
  }, [widths]);
  const rowNumW = Math.max(52, numbers.format(rowOffset + result.rows).length * 8 + PAD * 2);
  const totalW = rowNumW + (offsets[offsets.length - 1] ?? 0);
  const totalH = HEADER_H + result.rows * ROW_H;
  contentH.current = totalH;

  // The canvas has no accessible cells. Expose the active one without putting result data in
  // React state; stale asynchronous answers must not announce a previously selected cell.
  // Screen readers speak a cell when the grid's active descendant changes (SDB-425: Orca
  // ignores new text in the same element), so each announcement is a cell with an id of its
  // own, written whole (row, visible column, value) and only then made the active descendant:
  // once per move, again after an edit changes its value, never a loading text in between.
  const activeRow = selection?.focus.row;
  const activeColumn = selection?.focus.col;
  const activeEdits = editing?.cells;
  const activeDeleted = editing?.deleted;
  const announced = useRef({ serial: 0, said: "" });
  useEffect(() => {
    const target = accessibleCell.current;
    const grid = scrollerRef.current;
    if (!target || !grid) return;
    if (activeRow === undefined || activeColumn === undefined || activeRow >= result.rows) {
      grid.removeAttribute("aria-activedescendant");
      if (target.parentElement) target.parentElement.hidden = true;
      announced.current.said = "";
      return;
    }
    const column = result.columns[activeColumn];
    if (!column) return;
    const row = numbers.format(rowOffset + activeRow + 1);
    const colIndex = Math.max(1, visibleCols.indexOf(activeColumn) + 1);
    let current = true;
    const expose = (text: string) => {
      if (!current) return;
      // The same cell saying the same thing again (a re-render) is not a new announcement; the
      // same cell at another visible position (a column hidden or shown) is.
      const said = `${activeRow}:${activeColumn}:${colIndex}:${text}`;
      if (said === announced.current.said && grid.getAttribute("aria-activedescendant") === target.id) return;
      announced.current = { serial: announced.current.serial + 1, said };
      target.id = `${accessibleCellId}-${announced.current.serial}`;
      target.setAttribute("aria-colindex", String(colIndex));
      target.parentElement?.setAttribute("aria-rowindex", String(activeRow + 2));
      if (target.parentElement) target.parentElement.hidden = false;
      target.textContent = text;
      grid.setAttribute("aria-activedescendant", target.id);
    };
    const describe = (value: string | null) => {
      const text = value === null ? "NULL" : value.length > 500 ? t("results.accessibleTruncated", { value: value.slice(0, 500) }) : value;
      expose(t("results.accessibleCell", { row, column: column.name, type: column.nativeType, value: text }) +
        (value !== null && keyColumns.has(activeColumn) ? ` ${t("references.accessibleKey", { shortcut: shortcutText(FOLLOW_SHORTCUT) })}` : ""));
    };
    const key = `${activeRow}:${activeColumn}`;
    if (activeEdits && key in activeEdits) describe(activeEdits[key] ?? null);
    else void call(commands.cellValue(result.id, activeRow, activeColumn)).then(describe, () => {
      expose(t("results.accessibleCellError", { row, column: column.name }));
    });
    return () => { current = false; };
  }, [activeRow, activeColumn, activeEdits, activeDeleted, result.id, result.rows, result.columns, visibleCols, view, t, numbers, keyColumns, rowOffset, accessibleCellId]);

  // What `draw` needs, always up to date (draw is called outside the React cycle).
  const live = useRef({ result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, sorting, editing, gridLook, frozenCol, findCache, findCurrent, keyColumns, rowOffset });
  live.current = { result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, sorting, editing, gridLook, frozenCol, findCache, findCurrent, keyColumns, rowOffset };

  function draw() {
    const canvas = canvasRef.current;
    const scroller = scrollerRef.current;
    if (!canvas || !scroller) return;
    const { result, cache, widths, offsets, rowNumW, selection, size, numbers, sortShown, editing, gridLook, frozenCol, findCache, findCurrent, keyColumns, rowOffset } = live.current;
    const deletedRows = new Set(editing?.deleted ?? []);
    const range = selection ? bounds(selection) : null;
    const sparseRows = selection?.rows ? new Set(selection.rows) : null;
    const dpr = window.devicePixelRatio || 1;
    // Whole device pixels: at a fractional ratio (an interface size of 110 %, SDB-389) the
    // canvas would otherwise be resized on every frame.
    const deviceW = Math.round(size.w * dpr), deviceH = Math.round(size.h * dpr);
    if (canvas.width !== deviceW || canvas.height !== deviceH) {
      canvas.width = deviceW;
      canvas.height = deviceH;
    }
    const ctx = canvas.getContext("2d");
    if (!ctx) return;
    const c = currentGridTheme();
    const left = scroller.scrollLeft;
    const top = contentTop(scroller);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = c.bg;
    ctx.fillRect(0, 0, size.w, size.h);
    ctx.textBaseline = "middle";

    // A frozen column is drawn last, over the columns that scroll under it.
    const frozenW = frozenCol === null ? 0 : (widths[frozenCol] ?? 0);
    const fixedRight = rowNumW + frozenW;
    const first = Math.floor(top / ROW_H);
    const visible = Math.ceil((size.h - HEADER_H) / ROW_H) + 1;
    const last = Math.min(result.rows, first + visible);
    let firstCol = 0;
    while (firstCol < widths.length && rowNumW + (offsets[firstCol + 1] ?? 0) - left < fixedRight) firstCol++;
    findCache?.load(first, last);

    const drawCell = (row: number, col: number, x: number, y: number, w: number) => {
      if (range && (sparseRows ? sparseRows.has(row) : inside(range, row, col))) {
        ctx.fillStyle = c.select;
        ctx.fillRect(x, y, w, ROW_H);
      }
      if (findCache?.has(row, col)) {
        ctx.fillStyle = findCurrent && findCurrent.row === row && findCurrent.col === col ? c.foundCurrent : c.found;
        ctx.fillRect(x, y, w, ROW_H);
      }
      if (selection && selection.focus.row === row && selection.focus.col === col) {
        ctx.strokeStyle = c.accent;
        ctx.strokeRect(x + 0.5, y + 0.5, w - 1, ROW_H - 1);
      }
      const editKey = `${row}:${col}`;
      const edited = editing !== undefined && editKey in editing.cells;
      if (edited) {
        ctx.fillStyle = c.edited;
        ctx.fillRect(x, y, w, ROW_H);
      }
      const value = edited ? (editing?.cells[editKey] ?? null) : cache.get(row, col);
      const logical = result.columns[col]?.logical ?? "";
      const lower = value?.toLowerCase();
      if (gridLook.boolCheckboxes && logical === "bool" && lower !== undefined && (BOOL_TRUE.has(lower) || BOOL_FALSE.has(lower))) {
        // A checkbox in the middle of the cell, ticked for true.
        const bx = Math.round(x + w / 2 - CHECK / 2) + 0.5;
        const by = Math.round(y + ROW_H / 2 - CHECK / 2) + 0.5;
        ctx.lineWidth = 1;
        if (BOOL_TRUE.has(lower)) {
          ctx.fillStyle = c.accent;
          ctx.fillRect(bx - 0.5, by - 0.5, CHECK, CHECK);
          ctx.strokeStyle = c.bg;
          ctx.lineWidth = 1.6;
          ctx.beginPath();
          ctx.moveTo(bx + 2.5, by + CHECK / 2);
          ctx.lineTo(bx + CHECK / 2 - 0.5, by + CHECK - 3.5);
          ctx.lineTo(bx + CHECK - 2.5, by + 2.5);
          ctx.stroke();
          ctx.lineWidth = 1;
        } else {
          ctx.strokeStyle = c.muted;
          ctx.strokeRect(bx, by, CHECK - 1, CHECK - 1);
        }
        return;
      }
      const numeric = NUMERIC.has(logical);
      let text: string;
      if (edited && editing?.defaults.has(editKey)) {
        ctx.fillStyle = c.muted;
        text = "DEFAULT";
      } else if (value === undefined) {
        ctx.fillStyle = c.muted;
        text = "…";
      } else if (value === null) {
        ctx.fillStyle = gridLook.colorize ? c.vNull : c.muted;
        text = gridLook.nullText ? "NULL" : "";
      } else {
        const masked = result.columns[col]?.masked && !edited;
        // Colors by type (SDB-385), from the theme's tokens: the kind is the core's.
        const token = gridLook.colorize && !masked ? KIND_TOKEN[cache.column(col)?.kind ?? kindOf(logical)] : undefined;
        // A masked value (ADR-051) holds the mask, not data: drawn like NULL.
        ctx.fillStyle = masked ? c.muted : token ? c[token] : c.text;
        // The user's display format (SDB-385), for drawing only: an edit shows what was typed.
        text = (edited ? value : (cache.shown(row, col) ?? value)).replace(/[\r\n\t]+/g, " ");
      }
      // A foreign key value: a link at the right, which opens the referenced row.
      const linked = value !== null && value !== undefined && keyColumns.has(col) && !edited;
      const linkW = linked ? KEY_ICON + KEY_GAP : 0;
      if (linked) {
        const focused = selection !== null && selection.focus.row === row && selection.focus.col === col;
        const fill = ctx.fillStyle;
        drawKeyIcon(ctx, x + w - PAD - KEY_ICON, y + (ROW_H - KEY_ICON) / 2, focused ? c.accent : c.muted);
        ctx.fillStyle = fill;
      }
      if (!text) return;
      const shown = fit(ctx, text, w - PAD * 2 - linkW);
      if (numeric && value !== null && value !== undefined) {
        ctx.textAlign = "right";
        ctx.fillText(shown, x + w - PAD - linkW, y + ROW_H / 2);
        ctx.textAlign = "left";
      } else {
        ctx.fillText(shown, x + PAD, y + ROW_H / 2);
      }
    };

    // Cells.
    ctx.font = dataFont(c);
    for (let row = first; row < last; row++) {
      const y = HEADER_H + row * ROW_H - top;
      const striped = gridLook.zebra && row % 2 === 1;
      if (striped) {
        ctx.fillStyle = c.stripe;
        ctx.fillRect(rowNumW, y, size.w, ROW_H);
      }
      const deleted = deletedRows.has(row);
      if (deleted) {
        ctx.fillStyle = c.deleted;
        ctx.fillRect(rowNumW, y, size.w, ROW_H);
      }
      for (let col = firstCol; col < widths.length; col++) {
        const x = rowNumW + (offsets[col] ?? 0) - left;
        if (x > size.w) break;
        const w = widths[col] ?? 0;
        if (w === 0 || col === frozenCol) continue;
        drawCell(row, col, x, y, w);
      }
      if (frozenCol !== null) {
        ctx.fillStyle = c.bg;
        ctx.fillRect(rowNumW, y, frozenW, ROW_H);
        if (striped) {
          ctx.fillStyle = c.stripe;
          ctx.fillRect(rowNumW, y, frozenW, ROW_H);
        }
        if (deleted) {
          ctx.fillStyle = c.deleted;
          ctx.fillRect(rowNumW, y, frozenW, ROW_H);
        }
        drawCell(row, frozenCol, rowNumW, y, frozenW);
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
      if (x > fixedRight) ctx.fillRect(Math.round(x) - 1, HEADER_H, 1, size.h);
    }
    if (frozenCol !== null) {
      // A double line marks where the frozen column ends.
      ctx.fillRect(Math.round(fixedRight) - 2, HEADER_H, 2, size.h);
    }

    // Row number column (fixed on the left).
    ctx.fillStyle = c.header;
    ctx.fillRect(0, HEADER_H, rowNumW, size.h);
    ctx.font = rowNumberFont(c);
    ctx.textAlign = "right";
    for (let row = first; row < last; row++) {
      const y = HEADER_H + row * ROW_H - top;
      if (range && (sparseRows ? sparseRows.has(row) : row >= range.r0 && row <= range.r1)) {
        ctx.fillStyle = c.select;
        ctx.fillRect(0, y, rowNumW, ROW_H);
      }
      ctx.fillStyle = c.muted;
      ctx.fillText(numbers.format(rowOffset + row + 1), rowNumW - PAD, y + ROW_H / 2);
    }
    ctx.textAlign = "left";
    ctx.fillStyle = c.line;
    ctx.fillRect(rowNumW - 1, 0, 1, size.h);

    // Header (fixed at the top): name and native type; tinted where whole columns are selected.
    const wholeColumns = !selection?.rows && range !== null && result.rows > 0 && range.r0 === 0 && range.r1 === result.rows - 1;
    const drawHeader = (col: number, x: number, w: number) => {
      const meta = result.columns[col];
      if (!meta) return;
      if (wholeColumns && range && col >= range.c0 && col <= range.c1) {
        ctx.fillStyle = c.select;
        ctx.fillRect(x, 0, w, HEADER_H);
      }
      ctx.font = c.headerFont;
      ctx.fillStyle = c.text;
      // The column's place in the sort: its direction and, with several keys, its position.
      const place = sortShown.findIndex((key) => key.column === col);
      const sorted = place >= 0;
      const position = sorted && sortShown.length > 1 ? String(place + 1) : "";
      const positionW = position ? Math.ceil(ctx.measureText(position).width) + 2 : 0;
      const markW = sorted ? SORT_ICON + positionW + 4 : 0;
      // Under the name (SDB-385): the type, that it is masked, the zone its values are shown
      // in and the column's comment, each when it is asked for and known.
      const zone = cache.column(col)?.zone;
      const comment = gridLook.headerComment ? columnComments?.[meta.name] : undefined;
      // The zone goes first: it changes what the cells say, and a long type would cut it off.
      const typeLine = [
        zone ? t("results.shownInZone", { zone }) : "",
        gridLook.headerType ? meta.nativeType : "",
        meta.masked ? t("masking.header") : "",
        comment ?? "",
      ].filter((part) => part !== "").join(" · ");
      const nameY = typeLine ? 13 : HEADER_H / 2;
      ctx.fillText(fit(ctx, meta.name, w - PAD * 2 - markW), x + PAD, nameY);
      if (sorted) {
        const right = x + w - PAD;
        if (position) {
          ctx.fillStyle = c.accent;
          ctx.textAlign = "right";
          ctx.fillText(position, right, nameY);
          ctx.textAlign = "left";
        }
        const arrow = sortShown[place]?.descending ? "arrowDown" : "arrowUp";
        drawPaths(ctx, arrow, right - positionW - SORT_ICON, nameY - SORT_ICON / 2, SORT_ICON, c.accent);
      }
      ctx.font = c.typeFont;
      ctx.fillStyle = c.muted;
      if (typeLine) ctx.fillText(fit(ctx, typeLine, w - PAD * 2), x + PAD, 26);
      ctx.fillStyle = c.line;
      ctx.fillRect(Math.round(x + w) - 1, 0, 1, HEADER_H);
    };
    ctx.fillStyle = c.header;
    ctx.fillRect(0, 0, size.w, HEADER_H);
    for (let col = firstCol; col < widths.length; col++) {
      const x = rowNumW + (offsets[col] ?? 0) - left;
      if (x > size.w) break;
      const w = widths[col] ?? 0;
      if (w === 0 || col === frozenCol) continue;
      drawHeader(col, x, w);
    }
    if (frozenCol !== null) {
      ctx.fillStyle = c.header;
      ctx.fillRect(rowNumW, 0, frozenW, HEADER_H);
      drawHeader(frozenCol, rowNumW, frozenW);
      ctx.fillStyle = c.line;
      ctx.fillRect(Math.round(fixedRight) - 2, 0, 2, HEADER_H);
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
    // The interface size (the webview's zoom) or another monitor: the spacer is sized again
    // (the engines' height cap counts zoomed pixels) and the canvas drawn at the new ratio.
    const stopWatching = onPixelRatioChange(() => {
      setPixelRatio(window.devicePixelRatio || 1);
      redraw();
    });
    return () => {
      observer.disconnect();
      window.removeEventListener(THEME_CHANGE_EVENT, redraw);
      stopWatching();
    };
  }, [redraw]);

  useEffect(() => {
    const wrap = wrapRef.current;
    if (!wrap) return;
    const observer = new ResizeObserver(() => setWrapWidth(wrap.clientWidth));
    observer.observe(wrap);
    return () => observer.disconnect();
  }, []);

  useEffect(() => {
    redraw();
  });

  useEffect(() => {
    // Other rows: the selection would point at rows that moved or are gone.
    setSelection(null);
    setQueryPage(0);
  }, [result.filter, filtersKey]);

  useEffect(() => {
    // A disjoint selection names view positions; any changed ordering invalidates it.
    setSelection((current) => current?.rows ? null : current);
  }, [view]);

  // Hidden and frozen columns belong to the tab: running the query again (the same columns)
  // keeps them; another set of columns starts with every column shown.
  const columnNames = result.columns.map((c) => c.name).join("\u0000");
  const shownColumnNames = useRef(columnNames);
  useEffect(() => {
    if (shownColumnNames.current === columnNames) return;
    shownColumnNames.current = columnNames;
    setHidden(new Set());
    setFrozen(false);
    setChooser(null);
  }, [columnNames]);

  useEffect(() => {
    // A selection on a column just hidden would act on what is not shown.
    setSelection((current) => (current && (hidden.has(current.anchor.col) || hidden.has(current.focus.col)) ? null : current));
    onHiddenColumns?.([...hidden].sort((a, b) => a - b));
    // Only when the hidden columns change.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hidden]);

  useEffect(() => {
    // New result: scroll to the top and clear the selection.
    setResized({});
    setSelection(null);
    setMenu(null);
    setQueryPage(0);
    requestedPageScroll.current = null;
    scrollerRef.current?.scrollTo(0, 0);
    scrollPos.current = { content: 0, scroll: 0 };
  }, [result.id]);

  useEffect(() => {
    const row = requestedPageScroll.current;
    if (row !== null && result.rows > row) {
      requestedPageScroll.current = null;
      scrollToContent(row * ROW_H);
    }
  }, [result.rows]);

  useEffect(() => {
    if (result.refreshScrollRow !== undefined && result.rows > 0) scrollToContent(result.refreshScrollRow * ROW_H);
  }, [result.refreshScrollRow, result.id]);

  // "Go to row" that needed another page or more rows (SDB-386): the row, once it is here.
  useEffect(() => {
    const target = result.goToRow;
    if (target && target.row < result.rows) selectRow(target.row);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result.goToRow?.seq, result.id]);

  // "Go to row" and the selection's copies and export from the command palette, for this
  // result's grid (SDB-386, SDB-441): done by `runGridCommand`, with the selection of now.
  const gridCommandHandler = useRef<(command: GridCommand) => void>(() => undefined);
  useEffect(() => {
    const onCommand = (e: Event) => {
      const detail = (e as CustomEvent<{ result: number; command: GridCommand }>).detail;
      if (detail.result === result.id) gridCommandHandler.current(detail.command);
    };
    window.addEventListener(GRID_COMMAND_EVENT, onCommand);
    return () => window.removeEventListener(GRID_COMMAND_EVENT, onCommand);
  }, [result.id]);

  // The rows the results' Transpose button shows: the selected ones, or the current one.
  useEffect(() => {
    onSelectedRows?.(selection ? selectedRows() : []);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selection]);

  /** The cell under the pointer; `"row"` as column for the row-number gutter. With `clamp`,
   *  positions outside the data snap to the nearest cell (while dragging). */
  function cellAt(clientX: number, clientY: number, clamp = false): (Cell & { gutter: boolean }) | null {
    const scroller = scrollerRef.current;
    if (!scroller || result.rows === 0 || widths.length === 0) return null;
    const rect = scroller.getBoundingClientRect();
    const x = clientX - rect.left;
    const y = clientY - rect.top;
    if (!clamp && (y < HEADER_H || x > scroller.clientWidth || y > scroller.clientHeight)) return null;
    const top = contentTop(scroller);
    const row = Math.min(result.rows - 1, Math.max(0, Math.floor((y - HEADER_H + top) / ROW_H)));
    if (!clamp && Math.floor((y - HEADER_H + top) / ROW_H) >= result.rows) return null;
    const gutter = x < rowNumW;
    let col = columnAtX(x, scroller.scrollLeft);
    if (col < 0) col = x - rowNumW + scroller.scrollLeft < 0 ? firstVisible : lastVisible;
    return { row, col, gutter };
  }

  /** The column drawn `x` pixels from the grid's left edge, -1 if none: the frozen column
   *  first, then the shown columns under the scroll. */
  function columnAtX(x: number, scrollLeft: number): number {
    if (x < rowNumW) return -1;
    if (frozenCol !== null && x < rowNumW + frozenW) return frozenCol;
    const dataX = x - rowNumW + scrollLeft;
    return offsets.findIndex((o, i) => i < widths.length && (widths[i] ?? 0) > 0 && dataX >= o && dataX < (offsets[i + 1] ?? 0));
  }

  /** The foreign key cell whose link is under the pointer (or, with `anywhere`, the foreign key
   *  cell under it), if it holds a value: what a click on it follows. */
  function keyCellAt(clientX: number, clientY: number, anywhere: boolean): Cell | null {
    const scroller = scrollerRef.current;
    const cell = cellAt(clientX, clientY);
    if (!scroller || !cell || cell.gutter || !keyColumns.has(cell.col)) return null;
    const key = `${cell.row}:${cell.col}`;
    if (editing && key in editing.cells) return null;
    const value = cache.get(cell.row, cell.col);
    if (value === null || value === undefined) return null;
    if (anywhere) return { row: cell.row, col: cell.col };
    const left = scroller.getBoundingClientRect().left + (cell.col === frozenCol ? rowNumW : rowNumW + (offsets[cell.col] ?? 0) - scroller.scrollLeft);
    const right = left + (widths[cell.col] ?? 0) - PAD;
    return clientX >= right - KEY_ICON - KEY_GAP && clientX <= right + KEY_GAP ? { row: cell.row, col: cell.col } : null;
  }

  /** Follows a foreign key of the table from a row: the core finds the key in the catalog,
   *  reads the row's values and writes the filter of the table data tab it opens. */
  async function followKey(cell: Cell, key: TableKeyItem | undefined = keyColumns.get(cell.col)?.[0]) {
    setMenu(null);
    if (!keyTable?.connectionId || !key) return;
    try {
      const target = await call(commands.followKey(session, result.id, cell.row, keyTable.schema, keyTable.name, {
        direction: key.direction,
        schema: key.schema,
        table: key.table,
        name: key.name,
      }));
      openTableData(keyTable.connectionId, session, target.schema, target.table, target.filter);
    } catch (e) {
      setNotice({ text: t("references.followFailed", { detail: e instanceof Error ? e.message : String(e) }), error: true });
    }
  }

  /** Shows the value of the active cell (Enter, double-click, "View value"), even when the
   *  side panel last showed the row's references. */
  /** The cell menu's Save to file…: the extension comes from the core, which recognises images. */
  async function saveCellFromMenu(row: number, col: number) {
    const meta = result.columns[col];
    if (!meta) return;
    try {
      const item = await call(commands.viewCell(result.id, row, col, false));
      if (item.kind === "null") {
        setNotice({ text: t("viewer.saveError", { detail: t("viewer.nullValue") }), error: true });
        return;
      }
      const saved = await saveValueToFile(result.id, row, col, meta.name, item.extension, t, i18n.language);
      if (saved) setNotice(saved);
    } catch (e) {
      setNotice({ text: t("viewer.saveError", { detail: e instanceof Error ? e.message : String(e) }), error: true });
    }
  }

  /** The cell menu's Load from file…: one UPDATE by primary key, applied at once. */
  async function loadCellFromMenu(row: number, col: number) {
    if (!editing) return;
    const path = await chooseValueFile(t);
    if (path) await editing.onLoadFile(row, col, path);
  }

  function openCellViewer() {
    if (viewerMode === "references" || viewerMode === "calc" || viewerMode === "group") setViewerMode("cell");
    setViewer(true);
  }

  /** Shows the grouping panel, with `column` added to its columns when given. */
  function openGroup(column?: number) {
    setMenu(null);
    if (column !== undefined) setGrouped((current) => (current.includes(column) ? current : [...current, column]));
    setViewerMode("group");
    setViewer(true);
  }

  /** Shows the numbers about the selection in the side panel (the core computes them). */
  function openCalc() {
    setViewerMode("calc");
    setViewer(true);
  }

  const closeColumnStats = useCallback(() => {
    setColumnStats(null);
    scrollerRef.current?.focus();
  }, []);

  /** Opens a table data tab where a key of the references panel leads (its filter is the core's). */
  function openTarget(target: KeyTargetItem) {
    if (keyTable?.connectionId) openTableData(keyTable.connectionId, session, target.schema, target.table, target.filter);
  }

  /** The next shown column `delta` steps away (stopping at the first and the last). */
  function stepCol(col: number, delta: number): number {
    const at = visibleCols.indexOf(col);
    if (at < 0) return firstVisible;
    return visibleCols[Math.max(0, Math.min(visibleCols.length - 1, at + delta))] ?? col;
  }

  /** Shown columns inside a selection, in order (hidden ones are left out of copies). */
  const shownIn = (b: Bounds) => visibleCols.filter((c) => c >= b.c0 && c <= b.c1);
  const calcBounds = selection && ended(result) && result.rows > 0 ? bounds(selection) : null;
  const calcColumns = calcBounds ? shownIn(calcBounds).join(",") : "";
  const calcRange = useMemo<CellRange | null>(
    () =>
      calcBounds
        ? {
            firstRow: Math.min(calcBounds.r0, result.rows - 1),
            rows: Math.max(0, Math.min(calcBounds.r1, result.rows - 1) - Math.min(calcBounds.r0, result.rows - 1) + 1),
            columns: calcColumns ? calcColumns.split(",").map(Number) : [],
            selectedRows: selection?.rows ?? null,
          }
        : null,
    // Rebuilt only when the bounds change, not on every render.
    [calcBounds?.r0, calcBounds?.r1, calcColumns, result.rows, selection?.rows],
  );

  function hideColumn(col: number) {
    setMenu(null);
    if (visibleCols.length <= 1 || hidden.has(col)) return;
    if (selection?.focus.col === col) {
      const next = visibleCols.find((candidate) => candidate > col) ?? visibleCols.find((candidate) => candidate !== col);
      if (next !== undefined) {
        const cell = { row: selection.focus.row, col: next };
        setSelection({ anchor: cell, focus: cell });
      }
    }
    setHidden(new Set([...hidden, col]));
  }

  function showColumn(col: number) {
    if (!hidden.has(col)) return;
    setHidden(new Set([...hidden].filter((c) => c !== col)));
  }

  /** Selects whole columns from the anchor's column (with `extend`) to `col`. */
  function selectColumn(col: number, extend: boolean) {
    setMenu(null);
    if (result.rows === 0) return;
    setSelection((current) => ({
      anchor: { row: result.rows - 1, col: extend && current ? current.anchor.col : col },
      focus: { row: 0, col },
    }));
  }

  // ---- Find (Ctrl+F): the core searches the whole result; the grid asks for the matches of
  // the rows it shows and steps from one to the next.
  const findRun = useRef(0);
  // The result the core holds a search for, so closing the search clears it once: this
  // effect also runs on every row count of a streaming result, with no search open.
  const searched = useRef<number | null>(null);
  const shownKey = visibleCols.join(",");
  useEffect(() => {
    const text = find?.text ?? "";
    const run = ++findRun.current;
    setFindCurrent(null);
    findAt.current = null;
    // Highlights of the previous search would point at rows that may have moved.
    setFindCache(null);
    if (!find || !text) {
      setFound({ kind: "idle" });
      if (searched.current !== null) {
        void call(commands.clearFind(searched.current)).catch(() => undefined);
        searched.current = null;
      }
      return;
    }
    if (!ended(result)) {
      setFound({ kind: "failed", message: t("results.findWhenComplete") });
      return;
    }
    setFound({ kind: "loading" });
    // Typing fast searches once, for the last text.
    const timer = setTimeout(() => {
      searched.current = result.id;
      call(commands.findInResult(result.id, text, find.matchCase, visibleCols))
        .then(async (outcome) => {
          if (run !== findRun.current || !outcome) return;
          setFound({ kind: "ready", value: { count: outcome.count, truncated: outcome.truncated } });
          setFindCache(new FindCache(result.id, result.columns.length, () => redraw()));
          if (outcome.count > 0) await goToMatch("first", run);
        })
        .catch((e) => {
          if (run === findRun.current) setFound({ kind: "failed", message: errorText(e) });
        });
    }, 200);
    return () => clearTimeout(timer);
    // A sort, a filter or more rows move the matches: the same search runs again.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [find, result.id, ended(result), result.rows, view, shownKey]);

  /** Moves to the next or previous match, or with "first" to the first one from the active
   *  cell on; the active cell follows the match. */
  function goToMatch(where: "first" | "next" | "previous", run = findRun.current): Promise<void> {
    const selected = selection ? { row: selection.focus.row, col: selection.focus.col } : null;
    findSteps.current = findSteps.current.then(() => stepMatch(where, run, selected));
    return findSteps.current;
  }

  async function stepMatch(where: "first" | "next" | "previous", run: number, selected: { row: number; col: number } | null) {
    if (run !== findRun.current) return;
    const from = findAt.current ?? selected;
    try {
      let step;
      if (where === "first") {
        // The match before the active cell, then the one after it: the first at or after it.
        const before = from ? await call(commands.findStep(result.id, from.row, from.col, true)) : null;
        const start = before ?? { row: Math.max(0, result.rows - 1), column: Math.max(0, result.columns.length - 1) };
        step = await call(commands.findStep(result.id, start.row, start.column, false));
      } else {
        const at = from ?? { row: result.rows - 1, col: result.columns.length - 1 };
        step = await call(commands.findStep(result.id, at.row, at.col, where === "previous"));
      }
      if (run !== findRun.current || !step) return;
      const cell = { row: step.row, col: step.column };
      findAt.current = cell;
      setFindCurrent({ index: step.index, ...cell });
      setSelection({ anchor: cell, focus: cell });
      scrollIntoView(cell);
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  function openFind() {
    setFind((current) => current ?? { text: "", matchCase: false });
    requestAnimationFrame(() => {
      findInput.current?.focus();
      findInput.current?.select();
    });
  }

  function closeFind() {
    setFind(null);
    scrollerRef.current?.focus();
  }

  function findStatus(): string {
    switch (found.kind) {
      case "idle":
        return "";
      case "failed":
        return found.message;
      case "loading":
        return t("results.findSearching");
      case "ready":
        break;
      default: {
        const unexpected: never = found;
        return unexpected;
      }
    }
    const { count, truncated } = found.value;
    if (count === 0) return t("results.findNone");
    const total = `${numbers.format(count)}${truncated ? "+" : ""}`;
    return findCurrent ? t("results.findOf", { current: numbers.format(findCurrent.index + 1), total }) : t("results.findCount", { count, total });
  }

  /** A table data tab filters in the server: the core writes the condition from the typed
   *  value and adds it to the tab's WHERE. */
  async function addServerFilter(filter: ColumnFilterItem, row: number | null) {
    setMenu(null);
    setCustomFilter(null);
    if (!tableData) return;
    try {
      const next = await call(commands.tableFilterCondition(session, result.id, filter.column, filter.op, row, filter.value, tableData.filter));
      tableData.onFilter(next);
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  async function writeClipboard(text: string, html?: string) {
    if (html !== undefined && typeof ClipboardItem !== "undefined") {
      await navigator.clipboard.write([
        new ClipboardItem({ "text/html": new Blob([html], { type: "text/html" }), "text/plain": new Blob([text], { type: "text/plain" }) }),
      ]);
    } else {
      await navigator.clipboard.writeText(text);
    }
  }

  /** Column names of `columns` (shown ones, in order), written by the core. */
  async function copyNames(columns: number[], style: NameStyleItem) {
    setMenu(null);
    try {
      const table = tableData ? { schema: tableData.schema, name: tableData.name } : null;
      const text = await call(commands.copyColumnNames(result.id, session, columns, style, table));
      await writeClipboard(text);
      setNotice({ text: t("results.copiedNames", { count: columns.length, formattedCount: numbers.format(columns.length) }), error: false });
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  /** Every different value of one column (all its rows) as an IN list, written by the core. */
  async function copyColumnInList(col: number) {
    setMenu(null);
    if (!ended(result)) {
      setNotice({ text: t("results.filterWhenComplete"), error: true });
      return;
    }
    try {
      const text = await call(commands.copyColumnInList(result.id, session, col));
      await writeClipboard(text);
      setNotice({ text: t("results.copiedInList", { column: result.columns[col]?.name ?? "" }), error: false });
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  /** The columns a header's menu acts on: the selected whole columns if it is one of them. */
  function headerColumns(col: number): number[] {
    if (selection && result.rows > 0) {
      const b = bounds(selection);
      if (b.r0 === 0 && b.r1 === result.rows - 1 && col >= b.c0 && col <= b.c1) return shownIn(b);
    }
    return [col];
  }

  /** Applies the column conditions in the core (an empty list removes them), read as `mode`
   *  says: all of them or any, telling upper case from lower case or not. */
  async function applyColumnFilters(filters: ColumnFilterItem[], mode: FilterMode | undefined = result.filterMode) {
    setMenu(null);
    setCustomFilter(null);
    if (restoringView || result.paging?.loading) return;
    if (!ended(result)) {
      setNotice({ text: t("results.filterWhenComplete"), error: true });
      return;
    }
    // Pending edits name rows by their place in the view (ADR-044): the core refuses to change
    // it under them, and the grid says so first, as it does for sorting.
    if (editing && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0)) {
      setNotice({ text: t("edit.applyFirst"), error: true });
      return;
    }
    try {
      const outcome = await call(commands.filterColumns(result.id, filters, mode ?? null));
      // Without conditions there is nothing to read one way or the other.
      onColumnFiltered(filters, outcome.rows, outcome.total, filters.length > 0 ? mode : undefined);
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
    if (frozenCol !== null) {
      if (Math.abs(rowNumW + frozenW - x) <= EDGE) return frozenCol;
      // Edges that scrolled under the frozen column cannot be grabbed.
      if (x < rowNumW + frozenW) return null;
    }
    const dataX = x - rowNumW + scroller.scrollLeft;
    for (let col = 0; col < widths.length; col++) {
      if ((widths[col] ?? 0) === 0 || col === frozenCol) continue;
      if (Math.abs((offsets[col + 1] ?? 0) - dataX) <= EDGE) return col;
    }
    return null;
  }

  /** Width that shows the header and the loaded cells of a column in full (bounded). */
  function fitWidth(col: number): number {
    const ctx = canvasRef.current?.getContext("2d");
    const meta = result.columns[col];
    const theme = currentGridTheme();
    if (!ctx || !meta) return widths[col] ?? MIN_RESIZE;
    ctx.font = theme.headerFont;
    let widest = ctx.measureText(meta.name).width + 14;
    ctx.font = theme.typeFont;
    widest = Math.max(widest, ctx.measureText(meta.nativeType).width);
    ctx.font = dataFont(theme);
    const scroller = scrollerRef.current;
    const first = scroller ? Math.floor(contentTop(scroller) / ROW_H) : 0;
    const last = Math.min(result.rows, first + Math.ceil((size.h || 600) / ROW_H) + 1);
    for (let row = first; row < last; row++) {
      const value = cache.shown(row, col);
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
    const col = columnAtX(x, scroller.scrollLeft);
    return col < 0 ? null : col;
  }

  /** Sorts in the core and asks for the rows again; `undefined` restores the server order. */
  // Clicks can come faster than the core answers: the next click cycles from the last sort
  // asked for, and the requests reach the core one after another, in order.
  const requestedSort = useRef(sortShown);
  const sortQueue = useRef<Promise<void>>(Promise.resolve());
  useEffect(() => {
    requestedSort.current = sortShown;
    // Only when another result is shown; within one result the ref leads.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result.id]);

  /** Sorts by `sort`, the most significant key first; none restores the original order. */
  function sortBy(sort: SortState[]) {
    setMenu(null);
    if (restoringView || result.paging?.loading) return;
    const maskedColumn = sort.map((key) => result.columns[key.column]).find((column) => column?.masked);
    if (maskedColumn) {
      setNotice({ text: t("masking.noSort", { column: maskedColumn.name }), error: true });
      return;
    }
    if (editing && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0)) {
      setNotice({ text: t("edit.applyFirst"), error: true });
      return;
    }
    if (serverSort) {
      if (selection?.rows) setSelection(null);
      requestedSort.current = sort;
      serverSort.onSort(sort);
      return;
    }
    if (!ended(result)) {
      setNotice({ text: t("results.sortWhenComplete"), error: true });
      return;
    }
    const previous = requestedSort.current;
    if (selection?.rows) setSelection(null);
    requestedSort.current = sort;
    sortsPending.current += 1;
    setSorting(true);
    const paging = result.paging;
    const pagesSort = paging?.sort ?? NO_SORT;
    sortQueue.current = sortQueue.current.then(async () => {
      try {
        // A paged query whose rows are not all here, or were read in a sort's order, is
        // read again from the server in the new order (ADR-054) when the core can page it
        // that way.
        if (paging && onQuerySort && !sameSort(sort, pagesSort) && (sort.length > 0 ? paging.hasMore || pagesSort.length > 0 : true)) {
          if (await onQuerySort(sort)) return;
        }
        // Otherwise the core sorts the rows it holds; the pages' own order needs no sort.
        const local = paging && sameSort(sort, pagesSort) ? NO_SORT : sort;
        await call(commands.sortResult(result.id, local));
        onSorted(local);
        if (local.length > 0 && paging?.hasMore) {
          setNotice({ text: t("results.sortLoadedOnly", { count: result.rows, formattedCount: numbers.format(result.rows) }), error: false, slow: true });
        }
      } catch (e) {
        requestedSort.current = previous;
        setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
      } finally {
        sortsPending.current -= 1;
        if (sortsPending.current === 0) setSorting(false);
      }
    });
  }

  /** A header click cycles that column: ascending, descending, out of the sort. A plain click
   *  starts over with that column alone; with Shift (`add`) the column is the next key of the
   *  sort in force, or changes in place when it is already one. */
  function cycleSort(col: number, add: boolean) {
    const current = requestedSort.current;
    const place = current.findIndex((key) => key.column === col);
    const key = current[place];
    if (!add) {
      if (!key || current.length > 1) sortBy([{ column: col, descending: false }]);
      else if (!key.descending) sortBy([{ column: col, descending: true }]);
      else sortBy([]);
    } else if (!key) sortBy([...current, { column: col, descending: false }]);
    else if (!key.descending) sortBy(current.map((k, i) => (i === place ? { column: col, descending: true } : k)));
    else sortBy(current.filter((_, i) => i !== place));
  }

  /** Moves the active cell to `col` in the same row (the first row if none) and shows it. */
  function jumpTo(col: number) {
    setJump(null);
    showColumn(col);
    if (result.rows === 0) return;
    const cell = { row: selection?.focus.row ?? 0, col };
    setSelection({ anchor: cell, focus: cell });
    scrollIntoView(cell);
    scrollerRef.current?.focus();
  }

  /** The content position at the grid's top (row `r` starts at `r * ROW_H` in it). The
   *  container's position itself until the result is taller than the spacer can be. */
  function contentTop(scroller: HTMLDivElement | null = scrollerRef.current): number {
    if (!scroller) return 0;
    const map = scrollMap(contentH.current, scroller.clientHeight);
    if (!isScaled(map)) return scroller.scrollTop;
    const known = scrollPos.current;
    if (Math.abs(scroller.scrollTop - known.scroll) < 1) return Math.min(known.content, maxContentTop(map));
    return toContent(map, scroller.scrollTop);
  }

  /** Scrolls so that a content position is at the grid's top. */
  function scrollToContent(top: number) {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const map = scrollMap(contentH.current, scroller.clientHeight);
    if (!isScaled(map)) {
      scroller.scrollTop = top;
      return;
    }
    const content = Math.min(Math.max(0, top), maxContentTop(map));
    const before = scroller.scrollTop;
    scroller.scrollTop = toScroll(map, content);
    scrollPos.current = { content, scroll: scroller.scrollTop };
    // A move smaller than a pixel of the container fires no scroll event: paint it anyway.
    if (scroller.scrollTop === before) onGridScroll();
  }

  // While the scroll is scaled a wheel moves the content by its own amount (a pixel of the
  // container would be many rows); otherwise the browser scrolls as usual.
  const onScaledWheel = useRef<(e: WheelEvent) => void>(() => {});
  onScaledWheel.current = (e: WheelEvent) => {
    const scroller = scrollerRef.current;
    if (!scroller || e.ctrlKey || !isScaled(scrollMap(contentH.current, scroller.clientHeight))) return;
    let dy = wheelPixels(e.deltaY, e.deltaMode, ROW_H, scroller.clientHeight - HEADER_H);
    let dx = wheelPixels(e.deltaX, e.deltaMode, ROW_H, scroller.clientWidth - rowNumW);
    if (e.shiftKey && dx === 0) {
      dx = dy;
      dy = 0;
    }
    e.preventDefault();
    if (dx !== 0) scroller.scrollLeft += dx;
    if (dy !== 0) scrollToContent(contentTop(scroller) + dy);
  };
  useEffect(() => {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    // Not passive: a scaled wheel replaces the browser's scroll.
    const listener = (e: WheelEvent) => onScaledWheel.current(e);
    scroller.addEventListener("wheel", listener, { passive: false });
    return () => scroller.removeEventListener("wheel", listener);
  }, []);

  function scrollIntoView(cell: Cell) {
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const y = cell.row * ROW_H;
    const top = contentTop(scroller);
    if (y < top) scrollToContent(y);
    else if (y + ROW_H > top + scroller.clientHeight - HEADER_H)
      scrollToContent(y + ROW_H - scroller.clientHeight + HEADER_H);
    // The frozen column is always in view; the others scroll into the part it leaves.
    if (cell.col === frozenCol) return;
    const x = offsets[cell.col] ?? 0;
    const w = widths[cell.col] ?? 0;
    if (x - frozenW < scroller.scrollLeft) scroller.scrollLeft = x - frozenW;
    else if (x + w > scroller.scrollLeft + scroller.clientWidth - rowNumW)
      scroller.scrollLeft = x + w - scroller.clientWidth + rowNumW;
  }

  function selectAt(e: React.MouseEvent | MouseEvent, extend: boolean) {
    editRequest.current += 1;
    typedWhileLoading.current = null;
    // The next F3 starts from the cell clicked.
    findAt.current = null;
    const cell = cellAt(e.clientX, e.clientY, extend);
    if (!cell) return;
    if (cell.gutter && (e.ctrlKey || e.metaKey) && !extend) {
      if (selection && !selection.rows && bounds(selection).r1 - bounds(selection).r0 + 1 > 100_000) {
        setNotice({ text: t("results.disjointTooLarge"), error: true }); return;
      }
      const existing = selection?.rows ?? (selection && bounds(selection).c0 === firstVisible && bounds(selection).c1 === lastVisible ? selectedRows() : []);
      if (existing.length >= 100_000 && !existing.includes(cell.row)) { setNotice({ text: t("results.disjointTooLarge"), error: true }); return; }
      const rows = existing.includes(cell.row) ? existing.filter((row) => row !== cell.row) : [...existing, cell.row].sort((a, b) => a - b);
      if (!rows.length) { setSelection(null); return; }
      setSelection({ anchor: { row: rows[0]!, col: firstVisible }, focus: { row: rows[rows.length - 1]!, col: lastVisible }, rows });
      return;
    }
    setSelection((current) => {
      if (cell.gutter) {
        // The row-number gutter selects whole rows.
        const anchor = extend && current ? { row: current.anchor.row, col: firstVisible } : { row: cell.row, col: firstVisible };
        return { anchor, focus: { row: cell.row, col: lastVisible } };
      }
      return extend && current ? { anchor: current.anchor, focus: cell } : { anchor: cell, focus: cell };
    });
  }

  /** The selected rows (the ones chosen one by one, or the block) and shown columns. */
  function selectedRange(): CellRange | null {
    if (!selection) return null;
    const b = bounds(selection);
    return { firstRow: b.r0, rows: selection.rows?.length ?? b.r1 - b.r0 + 1, columns: shownIn(b), selectedRows: selection.rows ?? null };
  }

  function copiedNotice(range: CellRange) {
    setNotice({
      text: t(range.columns.length === 1 ? "results.copiedOneColumn" : "results.copied", {
        count: range.rows,
        formattedCount: numbers.format(range.rows),
        columns: numbers.format(range.columns.length),
      }),
      error: false,
    });
  }

  /** `displayed`: the cells as the grid shows them (SDB-385), only when the user picks it;
   *  every other copy is the database's own text. */
  async function copy(format: TextFormat, header: boolean, displayed = false) {
    setMenu(null);
    const range = selectedRange();
    if (!range) return;
    try {
      const text = await call(commands.copyRows(result.id, session, range, format, header, displayed));
      // HTML goes to the clipboard as HTML (documents and spreadsheets paste a table) and as
      // its source for plain text.
      await writeClipboard(text, format === "html" ? text : undefined);
      copiedNotice(range);
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    }
  }

  /** Where the core reads the table of an UPDATE copy from (SDB-388): the table data tab's
   *  table, or the statement the result came from. The core decides; it may refuse. */
  const updateSource: UpdateSourceItem | null = tableData
    ? { kind: "table", schema: tableData.schema, name: tableData.name }
    : result.sql ? { kind: "query", sql: result.sql } : null;

  /** A command from the palette: what the grid's menu does, on the selection of now. Without a
   *  selection, or where the menu would not offer it, the grid says why. */
  function runGridCommand(command: GridCommand) {
    if (command.kind === "goToRow") {
      if (result.rows > 0) setRowJump(true);
      return;
    }
    if (!selection) {
      setNotice({ text: t("results.selectFirst"), error: true });
      return;
    }
    switch (command.kind) {
      case "copy":
        void copy(command.format, command.header);
        return;
      case "copySpecial":
        setCopySpecial(selectedRange());
        return;
      case "copyUpdates":
        if (updateSource) void copyUpdates();
        else setNotice({ text: t("results.copyUpdatesRefused", { reason: t("results.noUpdateSource") }), error: true });
        return;
      case "exportSelection": {
        if (selection.rows || !ended(result)) {
          setNotice({ text: t("results.exportSelectionBlock"), error: true });
          return;
        }
        const b = bounds(selection);
        useExport.getState().open({ kind: "selection", session, result, range: { firstRow: b.r0, rows: b.r1 - b.r0 + 1, columns: shownIn(b) } });
        return;
      }
      default: {
        const unknown: never = command;
        return unknown;
      }
    }
  }
  gridCommandHandler.current = runGridCommand;

  async function copyUpdates() {
    setMenu(null);
    const range = selectedRange();
    if (!range || !updateSource) return;
    try {
      const text = await call(commands.copyUpdates(result.id, session, range, updateSource));
      await writeClipboard(text);
      setNotice({ text: t("results.copiedUpdates", { count: range.rows, formattedCount: numbers.format(range.rows) }), error: false });
    } catch (e) {
      setNotice({ text: t("results.copyUpdatesRefused", { reason: e instanceof Error ? e.message : String(e) }), error: true });
    }
  }

  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(null), notice.error || notice.slow ? 6000 : 2000);
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
    // Items can appear while it is open ("Export selection…" once the result is complete):
    // a taller menu is placed again so it stays inside the window.
    const resized = new ResizeObserver(position);
    resized.observe(menuRef.current);
    window.addEventListener("resize", position);
    return () => {
      resized.disconnect();
      window.removeEventListener("resize", position);
    };
  }, [menu]);

  function selectAll() {
    setSelection({ anchor: { row: 0, col: firstVisible }, focus: { row: result.rows - 1, col: lastVisible } });
  }

  function onKeyDown(e: React.KeyboardEvent) {
    const mod = e.ctrlKey || e.metaKey;
    const printable = printableGridKey({ key: e.key, ctrlKey: e.ctrlKey, metaKey: e.metaKey, altKey: e.altKey, shiftKey: e.shiftKey, isComposing: e.nativeEvent.isComposing });
    if (typedWhileLoading.current) {
      if (printable) { e.preventDefault(); typedWhileLoading.current.text += e.key; return; }
      if (e.key === "Escape") { e.preventDefault(); editRequest.current += 1; typedWhileLoading.current = null; return; }
      if (!["Shift", "Control", "Alt", "Meta"].includes(e.key)) {
        editRequest.current += 1;
        typedWhileLoading.current = null;
      }
    }
    if (e.key === "Escape" && viewer) {
      e.preventDefault();
      setViewer(false);
      return;
    }
    if (mod && !e.shiftKey && !e.altKey && e.key === "Enter" && selection && keyColumns.has(selection.focus.col)) {
      e.preventDefault();
      void followKey(selection.focus);
      return;
    }
    if (e.key === "Enter" && selection) {
      e.preventDefault();
      openCellViewer();
      return;
    }
    if (editing && e.key === "F2" && selection) {
      e.preventDefault();
      void startEdit(selection.focus);
      return;
    }
    if (editing && !editing.busy && !sorting && e.key === "Delete" && !e.altKey && !mod && selection) {
      e.preventDefault();
      editing.onDelete(selectedRows());
      return;
    }
    if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "f" && result.rows > 0) {
      e.preventDefault();
      openFind();
      return;
    }
    if (e.key === "F3" && find) {
      e.preventDefault();
      void goToMatch(e.shiftKey ? "previous" : "next");
      return;
    }
    if (mod && e.shiftKey && e.key.toLowerCase() === "j" && widths.length > 0) {
      e.preventDefault();
      setJump("");
      return;
    }
    if (matchesShortcut(e, goToRowKeys()) && result.rows > 0) {
      e.preventDefault();
      setRowJump(true);
      return;
    }
    if (mod && e.key.toLowerCase() === "a" && result.rows > 0) {
      e.preventDefault();
      selectAll();
      return;
    }
    if (!selection || result.rows === 0) return;
    if (mod && e.key.toLowerCase() === "c") {
      e.preventDefault();
      void copy("tsv", e.shiftKey);
      return;
    }
    if (e.key === " " && !e.altKey && (mod || e.shiftKey) && !(mod && e.shiftKey)) {
      // Spreadsheet keys: Shift+Space selects the selection's whole rows, Ctrl+Space its
      // whole columns.
      e.preventDefault();
      if (e.shiftKey) {
        setSelection({ anchor: { row: selection.anchor.row, col: firstVisible }, focus: { row: selection.focus.row, col: lastVisible } });
      } else {
        setSelection({ anchor: { row: result.rows - 1, col: selection.anchor.col }, focus: { row: 0, col: selection.focus.col } });
      }
      return;
    }
    if (editing && printable) {
      e.preventDefault();
      void startEdit(selection.focus, e.key);
      return;
    }
    // Arrows move one cell; with Ctrl, to the first or last row or column (a result has no
    // empty cells to stop at). Home / End go along the row, Ctrl+Home / Ctrl+End to the
    // corners, Page Up / Down by a screenful. Shift extends the selection with any of them.
    const lastRow = result.rows - 1;
    const page = Math.max(1, Math.floor((size.h - HEADER_H) / ROW_H) - 1);
    const { row, col } = selection.focus;
    let next: Cell;
    switch (e.key) {
      case "ArrowUp": next = { row: mod ? 0 : row - 1, col }; break;
      case "ArrowDown": next = { row: mod ? lastRow : row + 1, col }; break;
      case "ArrowLeft": next = { row, col: mod ? firstVisible : stepCol(col, -1) }; break;
      case "ArrowRight": next = { row, col: mod ? lastVisible : stepCol(col, 1) }; break;
      case "Home": next = { row: mod ? 0 : row, col: firstVisible }; break;
      case "End": next = { row: mod ? lastRow : row, col: lastVisible }; break;
      case "PageUp": next = { row: row - page, col }; break;
      case "PageDown": next = { row: row + page, col }; break;
      default: return;
    }
    e.preventDefault();
    next = { row: Math.min(lastRow, Math.max(0, next.row)), col: next.col };
    findAt.current = null;
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
      const drag = headerDrag.current;
      if (drag && e.buttons === 1) {
        if (!drag.moved && Math.hypot(e.clientX - drag.x, e.clientY - drag.y) > 5) drag.moved = true;
        if (drag.moved) setHeaderGhost({ col: drag.col, x: e.clientX, y: e.clientY });
        return;
      }
      if (dragging.current && e.buttons === 1) selectAt(e, true);
    };
    const up = (e: MouseEvent) => {
      dragging.current = false;
      resizing.current = null;
      const drag = headerDrag.current;
      if (drag) {
        headerDrag.current = null;
        setHeaderGhost(null);
        // Dropped on the grouping panel: the column joins the grouping. Released where it was
        // pressed: a click, which sorts. Dropped anywhere else: nothing.
        if (!drag.moved) cycleSort(drag.col, false);
        else if (document.elementFromPoint(e.clientX, e.clientY)?.closest(".group-panel")) openGroup(drag.col);
      }
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
    return () => {
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
  });

  /** "3 rows selected", "2 columns selected", "4 rows × 3 columns (12 cells)"… */
  function selectionSummary(): string {
    if (!selection || result.rows === 0) return "";
    const b = bounds(selection);
    const rows = selection.rows?.length ?? b.r1 - b.r0 + 1;
    const columns = shownIn(b).length;
    const allRows = rows === result.rows;
    const allColumns = columns === visibleCols.length;
    const n = (value: number) => numbers.format(value);
    if (rows * columns === 1) return t("results.selectedCell");
    if (allRows && allColumns) return t("results.selectedAll", { count: rows, formattedCount: n(rows) });
    if (allColumns) return t("results.selectedRows", { count: rows, formattedCount: n(rows) });
    if (allRows) return t("results.selectedColumns", { count: columns, formattedCount: n(columns) });
    return t("results.selectedCells", {
      rows: t("results.rowCount", { count: rows, formattedCount: n(rows) }),
      columns: t("results.columnCount", { count: columns, formattedCount: n(columns) }),
      cells: n(rows * columns),
    });
  }

  const closeChooser = useCallback(() => {
    setChooser(null);
    scrollerRef.current?.focus();
  }, []);

  const focusMeta = selection ? result.columns[selection.focus.col] : undefined;
  const viewerMax = wrapWidth ? Math.max(0, wrapWidth - 36 - 5 - 220) : 1200;
  const viewerMin = Math.min(200, viewerMax);
  const shownViewerWidth = Math.max(viewerMin, Math.min(viewerWidth, viewerMax));
  const resizeViewer = (width: number) => onViewerWidth(Math.round(Math.max(viewerMin, Math.min(width, viewerMax))));
  const viewerId = `cell-viewer-${session}`;
  const paging = result.paging;
  const pageViewChanged = !!(localSort.length || result.filter || result.columnFilters?.length);
  const pageBusy = pagingLocked || !!paging?.loading || !!cellEditor || sorting;
  const pageCanLoad = !!paging?.hasMore && (paging.total === undefined || result.rows < paging.total);
  const pageStart = paging ? queryPage * paging.pageSize : 0;
  const hasPendingEdits = !!editing && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0);
  const updatedRows = editing?.updatedRows ?? 0;

  function onGridScroll() {
    const scroller = scrollerRef.current;
    if (scroller) scrollPos.current = { content: contentTop(scroller), scroll: scroller.scrollTop };
    redraw();
    setMenu(null);
    if (!paging || !scroller) return;
    const bottom = Math.max(0, Math.min(result.rows - 1, Math.floor((contentTop(scroller) + scroller.clientHeight - HEADER_H - 1) / ROW_H)));
    setQueryPage(Math.floor(bottom / paging.pageSize));
    if (!pageBusy && !pageViewChanged && pageCanLoad && scroller.scrollTop + scroller.clientHeight >= scroller.scrollHeight - 2) {
      void onLoadMore();
    }
  }

  async function nextQueryPage() {
    if (!paging || pageBusy) return;
    const next = (queryPage + 1) * paging.pageSize;
    if (next >= result.rows) {
      if (pageViewChanged || !pageCanLoad) return;
      requestedPageScroll.current = next;
      if (!(await onLoadMore())) requestedPageScroll.current = null;
      return;
    }
    scrollToContent(next * ROW_H);
  }

  /** Page `page` (from 0) of a paged query: the rows loaded are scrolled to; a page past them
   *  loads every page before it by keyset (ADR-054), never an offset (SDB-386). */
  async function queryPageAt(page: number) {
    if (!paging || pageBusy) return;
    const start = page * paging.pageSize;
    if (start < result.rows) {
      scrollToContent(start * ROW_H);
      return;
    }
    if (pageViewChanged || !pageCanLoad || !onLoadRows) return;
    requestedPageScroll.current = start;
    const rows = await onLoadRows(start + paging.pageSize);
    if (rows === null || rows <= start) {
      requestedPageScroll.current = null;
      // Fewer rows than that: the last page there is.
      if (rows !== null && rows > 0) scrollToContent(Math.floor((rows - 1) / paging.pageSize) * paging.pageSize * ROW_H);
    }
  }

  /** The last page of a paged query: it is only known once every row is loaded, so it asks to
   *  load them (how many and how much memory first), then shows the last page. */
  async function lastQueryPage() {
    if (!paging || pageBusy) return;
    let rows: number | null = result.rows;
    if (pageCanLoad) {
      if (pageViewChanged || !onLoadRows) return;
      rows = await onLoadRows(null);
    }
    if (rows === null || rows === 0) return;
    requestedPageScroll.current = Math.floor((rows - 1) / paging.pageSize) * paging.pageSize;
    if (rows <= result.rows) {
      scrollToContent(requestedPageScroll.current * ROW_H);
      requestedPageScroll.current = null;
    }
  }
  function onResultShortcut(e: React.KeyboardEvent<HTMLDivElement>) {
    const target = e.target instanceof HTMLElement ? e.target : null;
    if (target?.closest('[role="dialog"]')) return;
    const mod = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    const isTextInput = !!target?.closest('input, textarea, [contenteditable="true"]');

    if (mod && !e.altKey && !e.shiftKey && key === "s") {
      e.preventDefault();
      e.stopPropagation();
      if (!editing || editing.busy || sorting) return;
      if (cellEditor && target?.classList.contains("cell-editor")) {
        const value = target instanceof HTMLInputElement ? target.value : cellEditor.value;
        const unchanged = cellEditor.original !== null && value === cellEditor.original;
        skipEditorBlur.current = true;
        editing.onApplyCell(cellEditor.row, cellEditor.col, unchanged ? undefined : value, cellEditor.dirty);
        setCellEditor(null);
        scrollerRef.current?.focus();
      } else {
        editing.onApply();
      }
      return;
    }
    if (!editing || editing.busy || sorting) return;
    if (mod && !e.altKey && !e.shiftKey && key === "r") {
      e.preventDefault();
      e.stopPropagation();
      if (cellEditor) { skipEditorBlur.current = true; setCellEditor(null); scrollerRef.current?.focus(); }
      editing.onDiscard();
      return;
    }
    // Text inputs keep their normal undo and editing keys.
    if (isTextInput) return;
    if (e.altKey && !e.shiftKey && key === "insert") {
      e.preventDefault();
      e.stopPropagation();
      if (mod) void duplicateRow();
      else if (ended(result)) setInserting({ values: [] });
    } else if (e.altKey && !mod && !e.shiftKey && key === "delete") {
      e.preventDefault();
      e.stopPropagation();
      if (selection) editing.onDelete(selectedRows());
    } else if (matchesShortcut(e, undoKeys())) {
      e.preventDefault();
      e.stopPropagation();
      editing.onUndo();
    } else if (matchesShortcut(e, redoKeys()) || (redoKeys() === commandSpec("edits.redo").shortcut && mod && e.shiftKey && !e.altKey && key === "z")) {
      e.preventDefault();
      e.stopPropagation();
      editing.onRedo();
    } else if (matchesShortcut(e, REVERT_SHORTCUT)) {
      e.preventDefault();
      e.stopPropagation();
      if (selection) editing.onRevert(selectedRows());
    } else if (matchesShortcut(e, SET_NULL_SHORTCUT)) {
      e.preventDefault();
      e.stopPropagation();
      setSelectedNull();
    } else if (matchesShortcut(e, FILL_DOWN_SHORTCUT)) {
      e.preventDefault();
      e.stopPropagation();
      bulkEdit({ kind: "fillDown" });
    }
  }
  const selectionText = selectionSummary();
  return (
    <>
    {refresh && createPortal(
      <button
        type="button"
        className="ghost result-refresh"
        aria-label={t("table.refresh")}
        title={t(tableData ? "table.refreshHint" : "resultTabs.rerun")}
        disabled={refresh.disabled || sorting}
        onClick={() => refresh.run(pageStart)}
      >
        <Icon name="refresh" />
      </button>,
      refresh.target,
    )}
    <div
      className="grid-wrap"
      ref={wrapRef}
      onKeyDown={onResultShortcut}
      onPaste={(e) => {
        // Ctrl+V on the grid pastes into the selected cells; text boxes keep their own paste.
        const target = e.target instanceof HTMLElement ? e.target : null;
        if (!editing || target?.closest('input, textarea, [contenteditable="true"], [role="dialog"]')) return;
        e.preventDefault();
        pasteText(e.clipboardData.getData("text/plain"));
      }}
    >
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
              title={hasPendingEdits ? t("edit.applyFirst") : undefined}
              disabled={hasPendingEdits}
              onClick={() => void applyColumnFilters((result.columnFilters ?? []).filter((_, j) => j !== i))}
            >
              <Icon name="x" />
            </button>
          </span>
        ))}
        {/* How the core reads the conditions together (SDB-381). */}
        {(result.columnFilters?.length ?? 0) > 1 && (
          <span className="filter-match" role="group" aria-label={t("results.matchMode")}>
            {([false, true] as const).map((any) => (
              <button
                key={String(any)}
                className="ghost small"
                aria-pressed={(result.filterMode?.any ?? false) === any}
                title={hasPendingEdits ? t("edit.applyFirst") : t(any ? "results.matchAnyHint" : "results.matchAllHint")}
                disabled={hasPendingEdits}
                onClick={() => void applyColumnFilters(result.columnFilters ?? [], { any, matchCase: result.filterMode?.matchCase ?? false })}
              >
                {t(any ? "results.matchAny" : "results.matchAll")}
              </button>
            ))}
          </span>
        )}
        <label className="check small" title={hasPendingEdits ? t("edit.applyFirst") : t("results.matchCaseHint")}>
          <input
            type="checkbox"
            checked={result.filterMode?.matchCase ?? false}
            disabled={hasPendingEdits}
            onChange={(e) => void applyColumnFilters(result.columnFilters ?? [], { any: result.filterMode?.any ?? false, matchCase: e.target.checked })}
          />
          {t("results.matchCase")}
        </label>
        <button
          className="ghost small"
          title={hasPendingEdits ? t("edit.applyFirst") : undefined}
          disabled={hasPendingEdits}
          onClick={() => void applyColumnFilters([])}
        >
          {t("results.resetFilters")}
        </button>
      </div>
    )}
    <div className="grid-host">
      <canvas ref={canvasRef} className="grid-canvas" style={{ width: size.w, height: size.h }} />
      {freed === result.id && (
        <div className="grid-freed" role="status">
          <strong>{t("results.freed.title")}</strong>
          <p>{t("results.freed.body")}</p>
          {onRunAgain && (
            <button type="button" className="primary" onClick={onRunAgain}>
              {t("results.freed.rerun")}
            </button>
          )}
        </div>
      )}
      <div
        ref={scrollerRef}
        className="grid-scroller"
        role="grid"
        tabIndex={0}
        aria-rowcount={result.rows + 1}
        aria-colcount={visibleCols.length}
        onFocus={() => {
          if (!selection && result.rows > 0 && visibleCols.length > 0) {
            const cell = { row: 0, col: firstVisible };
            setSelection({ anchor: cell, focus: cell });
          }
        }}
        onScroll={onGridScroll}
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
            // Ctrl+click selects the column (Ctrl+Shift+click up to it); a plain click sorts by
            // it alone, Shift+click adds it to the sort.
            if (e.ctrlKey || e.metaKey) selectColumn(header, e.shiftKey);
            // With the grouping panel open a header can be dragged to it: the click's sort
            // waits for the button to come up, to tell a click from a drag.
            else if (groupShown && !e.shiftKey) headerDrag.current = { col: header, x: e.clientX, y: e.clientY, moved: false };
            else cycleSort(header, e.shiftKey);
            return;
          }
          // The link of a foreign key cell, or Ctrl+click anywhere on one, follows the key.
          const hit = cellAt(e.clientX, e.clientY);
          if (hit?.gutter && (e.ctrlKey || e.metaKey) && !e.shiftKey) {
            e.preventDefault();
            dragging.current = false;
            selectAt(e, false);
            return;
          }
          const link = keyCellAt(e.clientX, e.clientY, (e.ctrlKey || e.metaKey) && !e.shiftKey);
          if (link) {
            e.preventDefault();
            setSelection({ anchor: link, focus: link });
            void followKey(link);
            return;
          }
          dragging.current = true;
          selectAt(e, e.shiftKey);
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          const header = headerAt(e.clientX, e.clientY);
          if (header !== null) {
            setMenu({ x: e.clientX, y: e.clientY, cell: null, header });
            return;
          }
          const cell = cellAt(e.clientX, e.clientY);
          if (!cell) return;
          // Right-click outside the selection selects that cell first.
          if (!selection || (selection.rows ? !selection.rows.includes(cell.row) : (!cell.gutter && !inside(bounds(selection), cell.row, cell.col)))) selectAt(e, false);
          setMenu({ x: e.clientX, y: e.clientY, cell: cell.gutter ? null : cell });
        }}
        onMouseMove={(e) => {
          e.currentTarget.style.cursor =
            resizing.current || edgeAt(e.clientX, e.clientY) !== null
              ? "col-resize"
              : headerAt(e.clientX, e.clientY) !== null || keyCellAt(e.clientX, e.clientY, e.ctrlKey || e.metaKey)
                ? "pointer"
                : "";
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
              void startEdit(cell);
              return;
            }
            setSelection({ anchor: cell, focus: cell });
            openCellViewer();
          }
        }}
        onKeyDown={onKeyDown}
        data-column-widths={widths.join(",")}
        data-sort={sortShown.map((key) => `${key.column}:${key.descending ? "desc" : "asc"}`).join(",")}
        aria-label={
          t("results.gridLabel", { count: result.rows, formattedCount: numbers.format(result.rows) }) +
          sortShown
            .map((key, i) => `, ${t(i === 0 ? "results.gridSorted" : "results.gridSortedThen", {
              column: result.columns[key.column]?.name ?? "",
              direction: t(key.descending ? "results.descending" : "results.ascending"),
            })}`)
            .join("")
        }
      >
        <div aria-hidden="true" style={{ width: totalW, height: scrollMap(totalH, 0).spacerH }} />
        {/* The active cell for screen readers: its id, indexes and text are written by the
            effect above, together, when its value is known (SDB-425). */}
        <div className="sr-only" role="row" hidden>
          <div ref={accessibleCell} role="gridcell" aria-selected="true" />
        </div>
      </div>
      <button
        type="button"
        className="grid-select-all"
        style={{ width: rowNumW, height: HEADER_H }}
        aria-label={t("results.selectAll")}
        title={withShortcut(t("results.selectAll"), keyText("selectAll"))}
        disabled={!result.rows || !visibleCols.length}
        onClick={() => {
          scrollerRef.current?.focus({ preventScroll: true });
          selectAll();
        }}
      ><Icon name="scan" /></button>
      {menu && menu.header !== undefined && (
        <div ref={menuRef} className="grid-menu result-grid-menu" role="menu" aria-label={result.columns[menu.header]?.name} style={{ left: menu.x, top: menu.y }}>
          <HeaderMenuItems
            column={menu.header}
            names={result.columns.map((c) => c.name)}
            hiddenCount={hidden.size}
            canHide={visibleCols.length > 1}
            frozen={frozen}
            sort={sortShown}
            onSelect={() => selectColumn(menu.header ?? 0, false)}
            onFit={() => {
              const col = menu.header ?? 0;
              setMenu(null);
              setResized((r) => ({ ...r, [col]: fitWidth(col) }));
            }}
            onHide={() => hideColumn(menu.header ?? 0)}
            onChooser={() => {
              setMenu(null);
              setChooser({ x: menu.x, y: menu.y });
            }}
            onShowAll={() => {
              setMenu(null);
              setHidden(new Set());
            }}
            onFreeze={() => {
              setMenu(null);
              setFrozen(!frozen);
            }}
            onSort={(sort) => sortBy(sort)}
            masked={result.columns[menu.header ?? 0]?.masked ?? false}
          >
            <button role="menuitem" onClick={() => openGroup(menu.header ?? 0)}>
              {t("group.byColumn")}
            </button>
            <Submenu label={t("results.copyNamesMenu")}>
              {NAME_STYLES.filter((n) => n.style !== "qualified" || tableData).map((n) => (
                <button key={n.style} role="menuitem" onClick={() => void copyNames(headerColumns(menu.header ?? 0), n.style)}>
                  {t(`results.${n.key}`)}
                </button>
              ))}
            </Submenu>
            <button role="menuitem" onClick={() => void copyColumnInList(menu.header ?? 0)}>
              {t("results.copyColumnInList")}
            </button>
            {tableData && (
              <button
                role="menuitem"
                title={t("columnStats.hint")}
                onClick={() => {
                  setMenu(null);
                  setColumnStats({ x: menu.x, y: menu.y, column: menu.header ?? 0 });
                }}
              >
                {t("columnStats.open")}
              </button>
            )}
          </HeaderMenuItems>
        </div>
      )}
      {menu && menu.header === undefined && (
        <div ref={menuRef} className="grid-menu result-grid-menu" role="menu" style={{ left: menu.x, top: menu.y }}>
          {editing && !editing.busy && !sorting && menu.cell && (
            <>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  if (menu.cell) void startEdit(menu.cell);
                }}
              >
                {t("edit.editCell", { shortcut: keyText("editCell") })}
              </button>
              <button role="menuitem" onClick={() => void setSelectedValue()}>
                {t("edit.setValue")}
              </button>
              <button role="menuitem" onClick={() => void pasteFromMenu()}>
                <span>{t("edit.paste")}</span><span className="menu-shortcut">{shortcutText(PASTE_SHORTCUT)}</span>
              </button>
              <button role="menuitem" onClick={() => bulkEdit({ kind: "fillDown" })}>
                <span>{t("edit.fillDown")}</span><span className="menu-shortcut">{shortcutText(FILL_DOWN_SHORTCUT)}</span>
              </button>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  setSelectedNull();
                }}
              >
                <span>{t("edit.setNull")}</span><span className="menu-shortcut">{shortcutText(SET_NULL_SHORTCUT)}</span>
              </button>
              <Submenu label={t("edit.changeCells")}>
                <button role="menuitem" onClick={() => bulkEdit({ kind: "trim" })}>{t("edit.trim")}</button>
                <button role="menuitem" onClick={() => bulkEdit({ kind: "upperCase" })}>{t("edit.upperCase")}</button>
                <button role="menuitem" onClick={() => bulkEdit({ kind: "lowerCase" })}>{t("edit.lowerCase")}</button>
                <hr />
                <button role="menuitem" onClick={() => bulkEdit({ kind: "set", value: { kind: "default" } })}>{t("edit.setDefault")}</button>
                <button role="menuitem" onClick={() => bulkEdit({ kind: "newUuid" })}>{t("edit.newUuid")}</button>
                <button role="menuitem" onClick={() => bulkEdit({ kind: "now" })}>{t("edit.now")}</button>
              </Submenu>
              <hr />
              <button
                role="menuitem"
                disabled={!editing.canUndo}
                onClick={() => {
                  setMenu(null);
                  editing.onUndo();
                }}
              >
                <span>{t("edit.undo")}</span><span className="menu-shortcut">{shortcutText(undoKeys())}</span>
              </button>
              <button
                role="menuitem"
                disabled={!editing.canRedo}
                onClick={() => {
                  setMenu(null);
                  editing.onRedo();
                }}
              >
                <span>{t("edit.redo")}</span><span className="menu-shortcut">{shortcutText(redoKeys())}</span>
              </button>
              <button
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  editing.onDelete(selectedRows());
                }}
              >
                {t("edit.deleteRows", { count: selectedRows().length, shortcut: keyText("deleteRows") })}
              </button>
              {selectedRows().some((r) => editing.deleted.includes(r) || Object.keys(editing.cells).some((k) => k.startsWith(`${r}:`))) && (
                <button
                  role="menuitem"
                  onClick={() => {
                    setMenu(null);
                    editing.onRevert(selectedRows());
                  }}
                >
                  <span>{t("edit.revert")}</span><span className="menu-shortcut">{shortcutText(REVERT_SHORTCUT)}</span>
                </button>
              )}
              <hr />
            </>
          )}
          {menu.cell && result.columns[menu.cell.col]?.masked && (
            <button
              role="menuitem"
              title={t("masking.revealHint")}
              onClick={() => {
                const cell = menu.cell;
                setMenu(null);
                if (cell) setReveal({ x: menu.x, y: menu.y, row: cell.row, col: cell.col });
              }}
            >
              {t("masking.reveal")}
            </button>
          )}
          <button
            role="menuitem"
            onClick={() => {
              setMenu(null);
              openCellViewer();
            }}
          >
            {t("viewer.open", { shortcut: keyText("cellViewer") })}
          </button>
          {menu.cell && (
            <button
              role="menuitem"
              onClick={() => {
                const cell = menu.cell;
                setMenu(null);
                if (cell) void saveCellFromMenu(cell.row, cell.col);
              }}
            >
              {t("viewer.save")}
            </button>
          )}
          {menu.cell && editing && (
            <button
              role="menuitem"
              title={hasPendingEdits ? t("edit.applyFirst") : t("viewer.loadHint")}
              disabled={editing.busy || sorting || hasPendingEdits || editing.deleted.includes(menu.cell.row)}
              onClick={() => {
                const cell = menu.cell;
                setMenu(null);
                if (cell) void loadCellFromMenu(cell.row, cell.col);
              }}
            >
              {t("viewer.load")}
            </button>
          )}
          {selection && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                openCalc();
              }}
            >
              {t("calc.open")}
            </button>
          )}
          {selection && onTranspose && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                onTranspose(selectedRows());
              }}
            >
              {t("transpose.fromSelection")}
            </button>
          )}
          {selection && !selection.rows && onChart && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                onChart(shownIn(bounds(selection)));
              }}
            >
              {t("chart.fromSelection")}
            </button>
          )}
          {menu.cell && (keyColumns.get(menu.cell.col) ?? []).map((key) => (
            <button key={`key:${key.schema}.${key.table}:${key.name}`} role="menuitem" onClick={() => menu.cell && void followKey(menu.cell, key)}>
              {t("references.openReferenced", { table: key.table })}
              <span className="menu-shortcut">{shortcutText(FOLLOW_SHORTCUT)}</span>
            </button>
          ))}
          {keyTable && keys.length > 0 && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                setViewerMode("references");
                setViewer(true);
              }}
            >
              {t("references.show")}
            </button>
          )}
          <hr />
          <Submenu label={t("results.copyMenu")}>
            {COPY_ITEMS.map((item) => (
              <button key={item.key} role="menuitem" onClick={() => void copy(item.format, item.header)}>
                {t(`results.${item.key}`, { shortcut: item.shortcut ? keyText(item.shortcut) : "" })}
              </button>
            ))}
            {displayFormats && (
              <button role="menuitem" onClick={() => void copy("tsv", false, true)}>
                {t("results.copyDisplayed")}
              </button>
            )}
            {updateSource && (
              <button role="menuitem" onClick={() => void copyUpdates()}>
                {t("results.copyUpdate")}
              </button>
            )}
            <button role="menuitem" onClick={() => { setMenu(null); setCopySpecial(selectedRange()); }}>
              {t("results.copySpecial")}
            </button>
          </Submenu>
          {selection && !selection.rows && ended(result) && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                const b = bounds(selection);
                useExport.getState().open({
                  kind: "selection",
                  session,
                  result,
                  range: { firstRow: b.r0, rows: b.r1 - b.r0 + 1, columns: shownIn(b) },
                });
              }}
            >
              {t("results.exportSelection")}
            </button>
          )}
          {selection && (
            <Submenu label={t("results.copyNamesMenu")}>
              {NAME_STYLES.filter((n) => n.style !== "qualified" || tableData).map((n) => (
                <button key={n.style} role="menuitem" onClick={() => void copyNames(shownIn(bounds(selection)), n.style)}>
                  {t(`results.${n.key}`)}
                </button>
              ))}
            </Submenu>
          )}
          {menu.cell && !(editing && (editing.inserts.length > 0 || editing.deleted.length > 0 || Object.keys(editing.cells).length > 0)) && (
            <>
              <hr />
              <FilterSubmenu
                cell={menu.cell}
                value={cache.get(menu.cell.row, menu.cell.col)}
                columns={result.columns}
                active={result.columnFilters ?? []}
                server={!!tableData}
                onAdd={(filter) => (tableData ? void addServerFilter(filter, menu.cell?.row ?? null) : addColumnFilter(filter))}
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
              <button role="menuitem" onClick={() => sortBy([{ column: selection.focus.col, descending: false }])}>
                {t("results.sortAsc")}
              </button>
              <button role="menuitem" onClick={() => sortBy([{ column: selection.focus.col, descending: true }])}>
                {t("results.sortDesc")}
              </button>
              {sortShown.length > 0 && (
                <button role="menuitem" onClick={() => sortBy([])}>
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
            left: cellEditor.col === frozenCol ? rowNumW : rowNumW + (offsets[cellEditor.col] ?? 0) - (scrollerRef.current?.scrollLeft ?? 0),
            top: HEADER_H + cellEditor.row * ROW_H - contentTop(),
            width: widths[cellEditor.col] ?? MIN_COL,
            height: ROW_H,
            // The values' size, the user's setting: what is typed matches the cell it replaces.
            fontSize: GRID_FONT_PX,
          }}
          value={cellEditor.value}
          onChange={(e) => setCellEditor({ ...cellEditor, value: e.target.value, dirty: true })}
          onFocus={(e) => { if (cellEditor.typed) e.currentTarget.setSelectionRange(e.currentTarget.value.length, e.currentTarget.value.length); else e.currentTarget.select(); }}
          onBlur={(e) => { if (!skipEditorBlur.current) commitEdit(e.currentTarget.value); skipEditorBlur.current = false; }}
          onKeyDown={(e) => {
            if (e.key === "Enter") { skipEditorBlur.current = true; commitEdit(e.currentTarget.value); }
            else if (e.key === "Escape") {
              skipEditorBlur.current = true;
              setCellEditor(null);
              scrollerRef.current?.focus();
            } else return;
            e.preventDefault();
            e.stopPropagation();
          }}
        />
      )}
      {rowJump && (
        <RowJump
          rows={rowOffset + result.rows}
          more={!!tableData || (!!result.paging?.hasMore && !pageViewChanged)}
          current={selection ? rowOffset + selection.focus.row + 1 : null}
          onGo={goToRow}
          onClose={() => {
            setRowJump(false);
            scrollerRef.current?.focus();
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
      {find && (
        <FindBar
          inputRef={findInput}
          text={find.text}
          matchCase={find.matchCase}
          status={findStatus()}
          canStep={found.kind === "ready" && found.value.count > 0}
          onText={(text) => setFind({ ...find, text })}
          onMatchCase={(matchCase) => setFind({ ...find, matchCase })}
          onNext={() => void goToMatch("next")}
          onPrevious={() => void goToMatch("previous")}
          onClose={closeFind}
        />
      )}
      {reveal && (
        <RevealPopover
          x={reveal.x}
          y={reveal.y}
          resultId={result.id}
          row={reveal.row}
          column={reveal.col}
          onClose={() => {
            setReveal(null);
            scrollerRef.current?.focus();
          }}
        />
      )}
      {columnStats && tableData && (
        <ColumnStatsPopover
          x={columnStats.x}
          y={columnStats.y}
          session={session}
          resultId={result.id}
          column={columnStats.column}
          name={result.columns[columnStats.column]?.name ?? ""}
          table={{ schema: tableData.schema, name: tableData.name, filter: tableData.filter }}
          onFilter={(filter) => {
            setColumnStats(null);
            tableData.onFilter(filter);
            scrollerRef.current?.focus();
          }}
          onClose={closeColumnStats}
        />
      )}
      {chooser && (
        <ColumnChooser
          x={chooser.x}
          y={chooser.y}
          names={result.columns.map((c) => c.name)}
          hidden={hidden}
          onChange={setHidden}
          onClose={closeChooser}
        />
      )}
      {notice && (
        <div className={`grid-notice ${notice.error ? "error" : ""}`} role="status">
          {notice.text}
        </div>
      )}
      {copySpecial && (
        <CopySpecialDialog
          load={loadCopyDefaults}
          copy={async (options) => {
            const text = await call(commands.copySpecial(result.id, copySpecial, options));
            await writeClipboard(text);
            copiedNotice(copySpecial);
            setCopySpecial(null);
            scrollerRef.current?.focus();
          }}
          onClose={() => {
            setCopySpecial(null);
            scrollerRef.current?.focus();
          }}
        />
      )}
      {customFilter && (
        <CustomFilterDialog
          start={customFilter}
          columns={result.columns}
          topValues={tableData ? { session, resultId: result.id, table: { schema: tableData.schema, name: tableData.name, filter: tableData.filter } } : undefined}
          onApply={(filter) => (tableData ? void addServerFilter(filter, null) : addColumnFilter(filter))}
          onCancel={() => {
            setCustomFilter(null);
            scrollerRef.current?.focus();
          }}
        />
      )}
    </div>
    {/* Always there, even empty: a footer that appeared with the first selection would shrink
        the grid under the pointer, and a right-click on the last rows would miss its cell. */}
    <div className="grid-footer">
      {hidden.size > 0 && (
        <button
          type="button"
          className="ghost small grid-hidden"
          title={t("results.hiddenHint")}
          onMouseDown={(e) => e.stopPropagation()}
          onClick={(e) => {
            const box = e.currentTarget.getBoundingClientRect();
            setChooser({ x: box.left, y: box.top - 8 });
          }}
        >
          {t("results.hiddenColumns", { count: hidden.size, formattedCount: numbers.format(hidden.size) })}
        </button>
      )}
      <span className="grid-selection">{selectionText}</span>
    </div>
    {(editing || paging) && (
      <div className="result-editing">
        {editing && editing.inserts.length > 0 && <div className="pending-rows" aria-label={t("edit.newRows")}>
          {editing.inserts.map((values, index) => <div className="pending-row" key={index}>
            <Icon name="plus" className="pending-row-mark" />
            <button className="ghost small pending-row-values" title={t("edit.editNewRow")} disabled={editing.busy || sorting} onClick={() => setInserting({ values, index })}>{values.length ? values.map(({ column, value }) => `${result.columns[column]?.name ?? column}: ${value === null ? "NULL" : value}`).join(" · ") : t("edit.defaultRow")}</button>
            <button className="ghost small" aria-label={t("edit.removeNewRow", { number: index + 1 })} title={t("edit.removeNewRow", { number: index + 1 })} disabled={editing.busy || sorting} onClick={() => editing.onRemoveInsert(index)}><Icon name="x" /></button>
          </div>)}
        </div>}
        <div className="result-edit-toolbar" role="toolbar" aria-label={t(editing ? "edit.toolbar" : "queryPaging.label")}>
          {editing && <>
            <div className="result-row-actions" role="group" aria-label={t("edit.toolbar")}>
              <button className="ghost" title={t("edit.shortcutAdd", { shortcut: keyText("addRow") })} aria-label={t("edit.addRow")} disabled={editing.busy || sorting || !ended(result)} onClick={() => setInserting({ values: [] })}><Icon name="addRow" /><span>{t("edit.addRow")}</span></button>
              <button className="ghost" title={t("edit.shortcutDuplicate", { shortcut: keyText("duplicateRow") })} aria-label={t("edit.duplicateRow")} disabled={editing.busy || sorting || !selection || editing.deleted.includes(selection.focus.row)} onClick={() => void duplicateRow()}><Icon name="duplicateRow" /><span>{t("edit.duplicateAction")}</span></button>
              <button className="ghost" title={t("edit.shortcutDelete", { shortcut: keyText("deleteSelectedRows") })} aria-label={t("edit.deleteSelected")} disabled={editing.busy || sorting || !selection} onClick={() => editing.onDelete(selectedRows())}><Icon name="delete" /><span>{t("edit.deleteAction")}</span></button>
              <button className="ghost" title={`${t("edit.revertSelected")} (${REVERT_SHORTCUT})`} aria-label={t("edit.revertSelected")} disabled={editing.busy || sorting || !selection} onClick={() => editing.onRevert(selectedRows())}><Icon name="revert" /><span>{t("edit.revertAction")}</span></button>
            </div>
            <div className="result-commit-actions" role="group" aria-label={t("edit.pendingActions")}>
              <button className="ghost" title={withShortcut(t("edit.undo"), commandShortcut("edits.undo"))} aria-label={t("edit.undo")} disabled={editing.busy || sorting || !editing.canUndo} onClick={() => { scrollerRef.current?.focus(); editing.onUndo(); }}><Icon name="undo" /></button>
              <button className="ghost" title={withShortcut(t("edit.redo"), commandShortcut("edits.redo"))} aria-label={t("edit.redo")} disabled={editing.busy || sorting || !editing.canRedo} onClick={() => { scrollerRef.current?.focus(); editing.onRedo(); }}><Icon name="redo" /></button>
              <button className={hasPendingEdits ? "primary" : ""} title={t("edit.shortcutSave", { shortcut: keyText("saveEdits") })} aria-label={t("edit.saveChanges")} disabled={editing.busy || sorting || !hasPendingEdits} onClick={() => { scrollerRef.current?.focus(); editing.onApply(); }}><Icon name="save" /><span>{t("edit.saveChanges")}</span></button>
              <button className="ghost" title={t("edit.shortcutDiscard", { shortcut: keyText("discardEdits") })} aria-label={t("edit.discard")} disabled={editing.busy || sorting || !hasPendingEdits} onClick={editing.onDiscard}><Icon name="discard" /><span>{t("edit.discard")}</span></button>
            </div>
            <span className={`small result-edit-count${hasPendingEdits ? " has-pending" : " muted"}`} role="status">
              {hasPendingEdits ? t("edit.counts", { inserted: editing.inserts.length, deleted: editing.deleted.length, updated: updatedRows }) : t("edit.noPending")}
            </span>
          </>}
          {paging && <>
            <div className="spacer" />
            <div className="query-paging" role="group" aria-label={t("queryPaging.label")} title={pageViewChanged ? t("queryPaging.clearView") : t("queryPaging.hint")}>
              <select aria-label={t("table.pageSize")} value={paging.pageSize} disabled={pageBusy} onChange={(e) => onQueryPageSize(Number(e.target.value))}>
                {PAGE_SIZES.map((size) => <option key={size} value={size}>{t("table.perPage", { count: size, formattedCount: numbers.format(size) })}</option>)}
              </select>
              {paging.progress !== undefined ? (
                <span className="load-rows small" role="status">
                  {t("pages.loadingRows", { rows: numbers.format(paging.progress) })}
                  <button type="button" onClick={() => onCancelLoad?.()}>{t("pages.cancelLoad")}</button>
                </span>
              ) : (
                <PageControls
                  page={queryPage}
                  pages={paging.total !== undefined && !pageViewChanged ? Math.max(1, Math.ceil(paging.total / paging.pageSize)) : !pageCanLoad ? Math.max(1, Math.ceil(result.rows / paging.pageSize)) : undefined}
                  locked={pageBusy}
                  canNext={!(pageStart + paging.pageSize >= result.rows && (!pageCanLoad || pageViewChanged))}
                  canLast={!(pageCanLoad && pageViewChanged)}
                  hint={pageViewChanged ? t("queryPaging.clearView") : t("pages.queryHint")}
                  lastHint={pageCanLoad ? t("pages.lastQueryHint") : undefined}
                  onPage={(page) => {
                    if (page === queryPage + 1) void nextQueryPage();
                    else void queryPageAt(page);
                  }}
                  onLast={() => void lastQueryPage()}
                  range={`${result.rows === 0 ? t("table.noRows") : t("table.range", { from: numbers.format(pageStart + 1), to: numbers.format(Math.min(result.rows, pageStart + paging.pageSize)) })}${paging.total !== undefined && !pageViewChanged ? ` ${t("table.ofTotal", { total: numbers.format(paging.total) })}` : ""}`}
                />
              )}
              <button type="button" className="ghost query-count" disabled={pageBusy || pageViewChanged} aria-label={t("table.count")} aria-busy={paging.loading} title={t(paging.loading ? "queryPaging.loading" : "queryPaging.countHint")} onClick={onCountQuery}>
                <span className={paging.loading ? "query-count-label busy" : "query-count-label"}>{t("table.count")}</span>
                {paging.loading && paging.progress === undefined && <span className="query-count-spinner" aria-hidden="true" />}
              </button>
              {pageCanLoad && onLoadRows && (
                <button type="button" className="ghost query-load-all" disabled={pageBusy || pageViewChanged} title={pageViewChanged ? t("queryPaging.clearView") : t("pages.loadAllHint")} onClick={() => void onLoadRows(null)}>
                  {t("pages.loadAll")}
                </button>
              )}
            </div>
          </>}
        </div>
        {editing && inserting && <InsertRowDialog columns={result.columns} initial={inserting.values} onAdd={(values) => { if (inserting.index === undefined) editing.onInsert(values); else editing.onUpdateInsert(inserting.index, values); setInserting(null); }} onCancel={() => setInserting(null)} />}
      </div>
    )}
    </div>
    {viewer && <>
    <div
      className="viewer-resizer"
      role="separator"
      tabIndex={0}
      aria-label={t("viewer.resize")}
      aria-orientation="vertical"
      aria-controls={viewerId}
      aria-valuemin={viewerMin}
      aria-valuemax={viewerMax}
      aria-valuenow={shownViewerWidth}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        viewerDrag.current = { startX: e.clientX, startWidth: shownViewerWidth };
        e.currentTarget.setPointerCapture(e.pointerId);
        e.preventDefault();
      }}
      onPointerMove={(e) => {
        const drag = viewerDrag.current;
        if (drag) resizeViewer(drag.startWidth + drag.startX - e.clientX);
      }}
      onPointerUp={() => { viewerDrag.current = null; }}
      onPointerCancel={() => { viewerDrag.current = null; }}
      onLostPointerCapture={() => { viewerDrag.current = null; }}
      onKeyDown={(e) => {
        if (e.key === "ArrowLeft") resizeViewer(shownViewerWidth + 16);
        else if (e.key === "ArrowRight") resizeViewer(shownViewerWidth - 16);
        else if (e.key === "Home") resizeViewer(viewerMin);
        else if (e.key === "End") resizeViewer(viewerMax);
        else return;
        e.preventDefault();
      }}
    />
    <div className="viewer-panel" id={viewerId} style={{ width: shownViewerWidth }}>
    {groupShown ? (
      <GroupPanel
        resultId={result.id}
        columns={result.columns}
        grouped={grouped.filter((column) => column < result.columns.length)}
        onGrouped={setGrouped}
        view={String(viewChanges.current.count)}
        waiting={!ended(result)}
        scope={serverSort ? "page" : result.paging?.hasMore ? "loaded" : undefined}
        dropping={headerGhost !== null}
        locked={!!result.paging?.loading || hasPendingEdits}
        onFiltered={(filters, rows, total, mode) => onColumnFiltered(filters, rows, total, mode)}
        modes={<ViewerModes modes={viewerModes} mode={shownMode} onMode={(mode) => { setViewerMode(mode); scrollerRef.current?.focus(); }} />}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    ) : calcShown ? (
      <CalcPanel
        resultId={result.id}
        range={calcRange}
        waiting={!!selection && !ended(result)}
        partial={partialText(result.end, result.total ?? result.rows, t, numbers)}
        view={`${view}|${result.rows}`}
        modes={<ViewerModes modes={viewerModes} mode={shownMode} onMode={(mode) => { setViewerMode(mode); scrollerRef.current?.focus(); }} />}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    ) : referencesShown && keyTable && result.rows > 0 ? (
      <ReferencesPanel
        session={session}
        resultId={result.id}
        row={Math.min(selection?.focus.row ?? 0, result.rows - 1)}
        schema={keyTable.schema}
        table={keyTable.name}
        modes={<ViewerModes modes={viewerModes} mode={shownMode} onMode={(mode) => { setViewerMode(mode); scrollerRef.current?.focus(); }} />}
        onOpen={openTarget}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    ) : selection && focusMeta ? (
      <CellViewer
        key={view}
        resultId={result.id}
        row={selection.focus.row}
        column={selection.focus.col}
        meta={focusMeta}
        columns={result.columns}
        mode={shownMode}
        modes={viewerModes}
        edits={editing?.cells}
        defaults={editing?.defaults}
        editable={!!editing && !editing.busy && !sorting && !editing.deleted.includes(selection.focus.row)}
        onEdit={editing?.onEdit}
        onType={editing?.onType}
        pendingChanges={hasPendingEdits}
        onLoadFile={editing?.onLoadFile}
        pickerSource={editing ? { session, schema: editing.table.schema, table: editing.table.name } : undefined}
        onMode={(mode) => {
          setViewerMode(mode);
          // Arrow keys keep moving through the grid.
          scrollerRef.current?.focus();
        }}
        onPickColumn={(col) => {
          showColumn(col);
          const cell = { row: selection.focus.row, col };
          setSelection({ anchor: cell, focus: cell });
          scrollIntoView(cell);
          setViewerMode("cell");
          scrollerRef.current?.focus();
        }}
        rowCount={result.rows}
        onPickRow={(row) => {
          const cell = { row, col: selection.focus.col };
          setSelection({ anchor: cell, focus: cell });
          scrollIntoView(cell);
        }}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    ) : (
      <aside className="cell-viewer" aria-label={t("viewer.title")}>
        <header>
          <div className="cell-viewer-title"><strong>{t("viewer.title")}</strong></div>
          <button className="ghost" aria-label={t("viewer.close", { shortcut: keyText("close") })} title={t("viewer.close", { shortcut: keyText("close") })} onClick={() => setViewer(false)}><Icon name="x" /></button>
        </header>
        <p className="muted small">{t("viewer.selectCell")}</p>
      </aside>
    )}
    </div>
    </>}
    <div className="result-side-actions" role="toolbar" aria-label={t("viewer.toolbar")}>
      <button
        type="button"
        className={`result-side-action ${viewer && !referencesShown && !calcShown && !groupShown ? "active" : ""}`}
        aria-label={t("viewer.toggle")}
        aria-pressed={viewer && !referencesShown && !calcShown && !groupShown}
        aria-controls={viewer ? viewerId : undefined}
        title={t(viewer && !referencesShown && !calcShown && !groupShown ? "viewer.hide" : "viewer.show")}
        onClick={() => {
          if (referencesShown || calcShown || groupShown) setViewerMode("cell");
          else setViewer(!viewer);
        }}
      >
        <Icon name="sidePanel" />
      </button>
      {keyTable && keys.length > 0 && (
        <button
          type="button"
          className={`result-side-action ${referencesShown ? "active" : ""}`}
          aria-label={t("references.toggle")}
          aria-pressed={referencesShown}
          aria-controls={referencesShown ? viewerId : undefined}
          title={t(referencesShown ? "references.hide" : "references.showPanel")}
          onClick={() => {
            if (referencesShown) setViewer(false);
            else {
              setViewerMode("references");
              setViewer(true);
            }
          }}
        >
          <Icon name="link" />
        </button>
      )}
      <button
        type="button"
        className={`result-side-action ${calcShown ? "active" : ""}`}
        aria-label={t("calc.toggle")}
        aria-pressed={calcShown}
        aria-controls={calcShown ? viewerId : undefined}
        title={t(calcShown ? "calc.hide" : "calc.show")}
        onClick={() => {
          if (calcShown) setViewer(false);
          else openCalc();
        }}
      >
        <Icon name="sigma" />
      </button>
      <button
        type="button"
        className={`result-side-action ${groupShown ? "active" : ""}`}
        aria-label={t("group.toggle")}
        aria-pressed={groupShown}
        aria-controls={groupShown ? viewerId : undefined}
        title={t(groupShown ? "group.hide" : "group.show")}
        onClick={() => {
          if (groupShown) setViewer(false);
          else openGroup();
        }}
      >
        <Icon name="group" />
      </button>
    </div>
    {headerGhost && (
      <div className="group-ghost" style={{ left: headerGhost.x + 12, top: headerGhost.y + 12 }} aria-hidden="true">
        {result.columns[headerGhost.col]?.name}
      </div>
    )}
    </div>
    </>
  );
}

/** "Go to row" (Ctrl+G, SDB-386): a row number, from 1 as the grid numbers rows; Enter goes.
 *  `rows` is what the grid numbers now, and `more` says rows past it can still be brought. */
function RowJump(props: { rows: number; more: boolean; current: number | null; onGo(row: number): void; onClose(): void }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [text, setText] = useState(props.current === null ? "" : String(props.current));
  return (
    <div className="row-jump" role="dialog" aria-label={t("goToRow.title")}>
      <input
        autoFocus
        inputMode="numeric"
        value={text}
        placeholder={t("goToRow.placeholder")}
        aria-label={t("goToRow.title")}
        onFocus={(e) => e.currentTarget.select()}
        onChange={(e) => setText(e.target.value.replace(/[^0-9]/g, ""))}
        onBlur={props.onClose}
        onKeyDown={(e) => {
          if (e.key === "Escape") props.onClose();
          else if (e.key === "Enter") {
            const row = Number.parseInt(text, 10);
            if (Number.isFinite(row) && row >= 1) props.onGo(row);
          } else return;
          e.preventDefault();
          e.stopPropagation();
        }}
      />
      <span className="small muted">{props.more ? "" : t("goToRow.of", { total: numbers.format(props.rows) })}</span>
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

/** The menu of a column header: select, fit, hide or freeze columns, and sort. */
function HeaderMenuItems(props: {
  column: number;
  names: string[];
  hiddenCount: number;
  canHide: boolean;
  frozen: boolean;
  /** The sort in force, the most significant key first. */
  sort: SortState[];
  onSelect(): void;
  onFit(): void;
  onHide(): void;
  onChooser(): void;
  onShowAll(): void;
  onFreeze(): void;
  onSort(sort: SortState[]): void;
  /** The column is masked (ADR-051): it cannot be sorted by. */
  masked?: boolean;
  /** Column-wide copies, after the hide and freeze entries. */
  children?: React.ReactNode;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = new Intl.NumberFormat(i18n.language);
  const noSortHint = props.masked ? t("masking.noSortHint") : undefined;
  // "Then sort": this column as the next key of the sort in force (what Shift+click does), or
  // its direction changed in place when it is already a key.
  const others = props.sort.filter((key) => key.column !== props.column);
  const then = (descending: boolean) => {
    const key = { column: props.column, descending };
    return props.sort.some((k) => k.column === props.column)
      ? props.sort.map((k) => (k.column === props.column ? key : k))
      : [...props.sort, key];
  };
  return (
    <>
      <button role="menuitem" onClick={props.onSelect}>{t("results.selectColumn", { gesture: keyText("selectColumn") })}</button>
      <button role="menuitem" onClick={props.onFit}>{t("results.fitColumn")}</button>
      <hr />
      <button role="menuitem" disabled={!props.canHide} onClick={props.onHide}>
        {t("results.hideColumn", { column: props.names[props.column] ?? "" })}
      </button>
      <button role="menuitem" onClick={props.onChooser}>{t("results.columnsMenu")}</button>
      {props.hiddenCount > 0 && (
        <button role="menuitem" onClick={props.onShowAll}>
          {t("results.showAllColumns", { count: props.hiddenCount, formattedCount: numbers.format(props.hiddenCount) })}
        </button>
      )}
      <button role="menuitemcheckbox" aria-checked={props.frozen} onClick={props.onFreeze}>
        {props.frozen ? "\u2713 " : ""}
        {t("results.freezeFirst")}
      </button>
      {props.children && <hr />}
      {props.children}
      <hr />
      <button role="menuitem" disabled={props.masked} title={noSortHint} onClick={() => props.onSort([{ column: props.column, descending: false }])}>{t("results.sortAsc")}</button>
      <button role="menuitem" disabled={props.masked} title={noSortHint} onClick={() => props.onSort([{ column: props.column, descending: true }])}>{t("results.sortDesc")}</button>
      {others.length > 0 && (
        <>
          <button role="menuitem" disabled={props.masked} title={noSortHint ?? t("results.sortThenHint", { gesture: keyText("sortThen") })} onClick={() => props.onSort(then(false))}>{t("results.sortThenAsc")}</button>
          <button role="menuitem" disabled={props.masked} title={noSortHint ?? t("results.sortThenHint", { gesture: keyText("sortThen") })} onClick={() => props.onSort(then(true))}>{t("results.sortThenDesc")}</button>
        </>
      )}
      {props.sort.length > 0 && <button role="menuitem" onClick={() => props.onSort([])}>{t("results.sortClear")}</button>}
    </>
  );
}

/** Which columns the grid shows: one checkbox each, a search, and "show all". At least one
 *  column stays shown. */
function ColumnChooser(props: {
  x: number;
  y: number;
  names: string[];
  hidden: ReadonlySet<number>;
  onChange(hidden: ReadonlySet<number>): void;
  onClose(): void;
}) {
  const { t } = useTranslation("editor");
  const [search, setSearch] = useState("");
  const box = useRef<HTMLDivElement>(null);
  const { onClose } = props;
  useLayoutEffect(() => {
    if (box.current) placeMenu(box.current, props.x, props.y);
  }, [props.x, props.y]);
  useEffect(() => {
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
  const query = search.trim().toLowerCase();
  const shown = props.names.length - props.hidden.size;
  return (
    <div ref={box} className="grid-menu column-chooser" role="dialog" aria-label={t("results.columnsTitle")} style={{ left: props.x, top: props.y }}>
      <input
        autoFocus
        type="search"
        value={search}
        placeholder={t("results.columnsSearch")}
        aria-label={t("results.columnsSearch")}
        spellCheck={false}
        onChange={(e) => setSearch(e.target.value)}
      />
      <div className="column-chooser-list">
        {props.names.map((name, col) => ({ name, col })).filter((c) => c.name.toLowerCase().includes(query)).map(({ name, col }) => {
          const visible = !props.hidden.has(col);
          return (
            <label key={col} className="check">
              <input
                type="checkbox"
                checked={visible}
                disabled={visible && shown <= 1}
                onChange={() => {
                  const next = new Set(props.hidden);
                  if (visible) next.add(col);
                  else next.delete(col);
                  props.onChange(next);
                }}
              />
              {name}
            </label>
          );
        })}
      </div>
      <div className="column-chooser-actions">
        <button className="ghost small" disabled={props.hidden.size === 0} onClick={() => props.onChange(new Set())}>
          {t("results.showAll")}
        </button>
      </div>
    </div>
  );
}

/** Filters for the cell on which the context menu was opened. */
function FilterSubmenu(props: {
  cell: Cell;
  value: string | null | undefined;
  columns: ResultMeta["columns"];
  active: ColumnFilterItem[];
  /** Conditions go to the table's WHERE (table data tab), not to the loaded rows. */
  server: boolean;
  onAdd(filter: ColumnFilterItem): void;
  onCustom(start: ColumnFilterItem): void;
  onReset(): void;
}) {
  const { t } = useTranslation("editor");
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
            { column, op: "contains" as const, value },
            { column, op: "notContains" as const, value },
            { column, op: "isNull" as const, value: "" },
            { column, op: "isNotNull" as const, value: "" },
          ];
  return (
    <Submenu label={t("results.filterMenu")}>
        {props.server && <p className="grid-menu-note muted small">{t("results.filterInServer")}</p>}
        {items.map((filter) => (
          <button key={filter.op} role="menuitem" onClick={() => props.onAdd(filter)}>
            <code>{filterLabel(filter, props.columns)}</code>
          </button>
        ))}
        <hr />
        <button role="menuitem" onClick={() => props.onCustom({ column, op: value === null ? "isNull" : "eq", value: value ?? "" })}>
          {t("results.customFilter")}
        </button>
        {props.active.length > 0 && !props.server && (
          <button role="menuitem" onClick={props.onReset}>
            {t("results.resetFilters")}
          </button>
        )}
    </Submenu>
  );
}

/** Ids of the "most frequent values" requests, apart from the column statistics' own. */

/** A column's most frequent values, as far as the dialog has asked for them. */
type TopValuesState =
  | { column: number; kind: "loading"; job: Job }
  | { column: number; kind: "done"; values: { value: string | null; count: number | null }[]; sampled: boolean; sampleRows: number | null }
  | { column: number; kind: "masked" }
  | { column: number; kind: "failed"; message: string };

/** One condition chosen by hand: column, operator and value. */
function CustomFilterDialog(props: {
  start: ColumnFilterItem;
  columns: ResultMeta["columns"];
  /** A table data tab: the server counts the column's most frequent values on request, to
   *  pick the value from (SDB-381). The core never offers a masked column's (ADR-051). */
  topValues?: { session: string; resultId: number; table: { schema: string; name: string; filter: string } };
  onApply(filter: ColumnFilterItem): void;
  onCancel(): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [filter, setFilter] = useState(props.start);
  const [top, setTop] = useState<TopValuesState | null>(null);
  const needsValue = takesValue(filter.op);
  const apply = () => props.onApply(needsValue ? filter : { ...filter, value: "" });
  // Only the answer for the column on screen counts; leaving the dialog or the column stops
  // the count.
  const running = useRef<Job | null>(null);
  const stop = () => {
    running.current?.cancel();
    running.current = null;
  };
  useEffect(() => stop, []);
  const masked = props.columns[filter.column]?.masked ?? false;
  const shownTop = top?.column === filter.column ? top : null;
  function loadTop() {
    const source = props.topValues;
    if (!source) return;
    stop();
    const column = filter.column;
    const job = startJob();
    running.current = job;
    setTop({ column, kind: "loading", job });
    call(commands.columnTopValues(source.session, job.channel, source.table, source.resultId, column))
      .then((outcome) => {
        if (running.current !== job) return;
        running.current = null;
        if (outcome.kind === "done") setTop({ column, kind: "done", values: outcome.values, sampled: outcome.sampled, sampleRows: outcome.sampleRows });
        else if (outcome.kind === "masked") setTop({ column, kind: "masked" });
        else setTop(null);
      })
      .catch((e: unknown) => {
        if (running.current !== job) return;
        running.current = null;
        setTop({ column, kind: "failed", message: e instanceof Error ? e.message : String(e) });
      });
  }
  return (
    <div className="modal-backdrop">
      <div className="modal custom-filter" role="dialog" aria-modal="true" aria-label={t("results.customFilterTitle")} onKeyDown={(e) => {
        if (e.key === "Escape") { e.stopPropagation(); props.onCancel(); }
      }}>
        <h2>{t("results.customFilterTitle")}</h2>
        <div
          className="custom-filter-row"
          onKeyDown={(e) => {
            if (e.key === "Enter") apply();
          }}
        >
          <select
            aria-label={t("results.filterColumn")}
            value={filter.column}
            onChange={(e) => { stop(); setTop(null); setFilter({ ...filter, column: Number(e.target.value) }); }}
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
        {props.topValues && (
          <div className="filter-top-values">
            {masked || shownTop?.kind === "masked" ? (
              <p className="muted small">{t("results.topValuesMasked")}</p>
            ) : shownTop === null ? (
              <button type="button" className="small" onClick={loadTop}>{t("results.topValuesShow")}</button>
            ) : shownTop.kind === "loading" ? (
              <p className="muted small" role="status">
                {t("results.topValuesLoading")}{" "}
                <button type="button" className="ghost small" onClick={() => { stop(); setTop(null); }}>{t("results.cancel")}</button>
              </p>
            ) : shownTop.kind === "failed" ? (
              <p className="small" role="alert">{t("results.topValuesFailed", { reason: shownTop.message })}</p>
            ) : (
              <>
                <p className="muted small">
                  {shownTop.values.length === 0 ? t("results.topValuesNone") : t("results.topValuesPick")}
                  {shownTop.sampled ? ` ${t("results.topValuesSampled", { rows: numbers.format(shownTop.sampleRows ?? 0) })}` : ""}
                </p>
                <ul className="filter-top-list" aria-label={t("results.topValues")}>
                  {shownTop.values.map((item, index) => (
                    <li key={index}>
                      <button
                        type="button"
                        className="ghost"
                        onClick={() =>
                          setFilter(item.value === null
                            ? { ...filter, op: "isNull", value: "" }
                            : { ...filter, op: takesValue(filter.op) ? filter.op : "eq", value: item.value })}
                      >
                        <code>{item.value === null ? "NULL" : item.value}</code>
                        <span className="muted small">{numbers.format(item.count ?? 0)}</span>
                      </button>
                    </li>
                  ))}
                </ul>
              </>
            )}
          </div>
        )}
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
