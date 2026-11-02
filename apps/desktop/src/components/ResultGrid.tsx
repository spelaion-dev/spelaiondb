// Canvas result grid (ADR-010). Only what is visible is painted; scrolling is native
// (a container with a "spacer" of the full size) and the canvas is redrawn on top.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type CellRange, type TextFormat } from "../api";
import { THEME_CHANGE_EVENT } from "../preferences";
import { RowCache } from "../grid/rowCache";
import type { ResultMeta } from "../store";
import { CellViewer } from "./CellViewer";

const ROW_H = 26;
const HEADER_H = 36;
const PAD = 8;
const MIN_COL = 72;
const MAX_COL = 420;
const DATA_FONT = '12.5px ui-monospace, "Cascadia Mono", Consolas, monospace';
const HEADER_FONT = '600 12.5px system-ui, "Segoe UI", sans-serif';
const TYPE_FONT = '11px system-ui, "Segoe UI", sans-serif';
const NUMERIC = new Set(["int", "float", "decimal"]);

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

export function ResultGrid({ result, session }: { result: ResultMeta; session: string }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const frame = useRef<number | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [selection, setSelection] = useState<Selection | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  const dragging = useRef(false);
  const [viewer, setViewer] = useState(false);

  const redraw = useCallback(() => {
    if (frame.current !== null) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = null;
      draw();
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const cache = useMemo(() => new RowCache(result.id, () => redraw()), [result.id, redraw]);
  cache.grewTo(result.rows);

  // Widths: header and type, bounded. (Fitting to content: next iteration.)
  const widths = useMemo(() => {
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
  const offsets = useMemo(() => {
    const out = [0];
    for (const w of widths) out.push((out[out.length - 1] ?? 0) + w);
    return out;
  }, [widths]);
  const rowNumW = Math.max(52, numbers.format(result.rows).length * 8 + PAD * 2);
  const totalW = rowNumW + (offsets[offsets.length - 1] ?? 0);
  const totalH = HEADER_H + result.rows * ROW_H;

  // What `draw` needs, always up to date (draw is called outside the React cycle).
  const live = useRef({ result, cache, widths, offsets, rowNumW, selection, size, numbers });
  live.current = { result, cache, widths, offsets, rowNumW, selection, size, numbers };

  function draw() {
    const canvas = canvasRef.current;
    const scroller = scrollerRef.current;
    if (!canvas || !scroller) return;
    const { result, cache, widths, offsets, rowNumW, selection, size, numbers } = live.current;
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
        const value = cache.get(row, col);
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
      ctx.fillText(fit(ctx, meta.name, w - PAD * 2), x + PAD, 13);
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
    // New result: scroll to the top and clear the selection.
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
        text: t("results.copied", { count: range.rows, formattedCount: numbers.format(range.rows), columns: range.columns }),
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
      if (dragging.current && e.buttons === 1) selectAt(e, true);
    };
    const up = () => {
      dragging.current = false;
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
    <div className="grid-host">
      <canvas ref={canvasRef} className="grid-canvas" style={{ width: size.w, height: size.h }} />
      <div
        ref={scrollerRef}
        className="grid-scroller"
        tabIndex={0}
        onScroll={redraw}
        onMouseDown={(e) => {
          if (e.button !== 0) return;
          dragging.current = true;
          selectAt(e, e.shiftKey);
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          const cell = cellAt(e.clientX, e.clientY);
          if (!cell) return;
          // Right-click outside the selection selects that cell first.
          if (!selection || (!cell.gutter && !inside(bounds(selection), cell.row, cell.col))) selectAt(e, false);
          const host = e.currentTarget.getBoundingClientRect();
          setMenu({ x: e.clientX - host.left, y: e.clientY - host.top });
        }}
        onDoubleClick={(e) => {
          const cell = cellAt(e.clientX, e.clientY);
          if (cell && !cell.gutter) {
            setSelection({ anchor: cell, focus: cell });
            setViewer(true);
          }
        }}
        onKeyDown={onKeyDown}
        aria-label={t("results.gridLabel", { count: result.rows, formattedCount: numbers.format(result.rows) })}
      >
        <div style={{ width: totalW, height: totalH }} />
      </div>
      {menu && (
        <div className="grid-menu" role="menu" style={{ left: menu.x, top: menu.y }}>
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
        </div>
      )}
      {notice && (
        <div className={`grid-notice ${notice.error ? "error" : ""}`} role="status">
          {notice.text}
        </div>
      )}
    </div>
    {viewer && selection && focusMeta && (
      <CellViewer
        resultId={result.id}
        row={selection.focus.row}
        column={selection.focus.col}
        meta={focusMeta}
        onClose={() => {
          setViewer(false);
          scrollerRef.current?.focus();
        }}
      />
    )}
    </div>
  );
}
