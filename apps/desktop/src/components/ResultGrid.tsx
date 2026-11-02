// Canvas result grid (ADR-010). Only what is visible is painted; scrolling is native
// (a container with a "spacer" of the full size) and the canvas is redrawn on top.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { RowCache } from "../grid/rowCache";
import type { ResultMeta } from "../store";

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

export function ResultGrid({ result }: { result: ResultMeta }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const frame = useRef<number | null>(null);
  const [size, setSize] = useState({ w: 0, h: 0 });
  const [selected, setSelected] = useState<Cell | null>(null);

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
  const live = useRef({ result, cache, widths, offsets, rowNumW, selected, size, numbers });
  live.current = { result, cache, widths, offsets, rowNumW, selected, size, numbers };

  function draw() {
    const canvas = canvasRef.current;
    const scroller = scrollerRef.current;
    if (!canvas || !scroller) return;
    const { result, cache, widths, offsets, rowNumW, selected, size, numbers } = live.current;
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
        if (selected && selected.row === row && selected.col === col) {
          ctx.fillStyle = c.select;
          ctx.fillRect(x, y, w, ROW_H);
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
    ctx.fillStyle = c.muted;
    ctx.textAlign = "right";
    for (let row = first; row < last; row++) {
      ctx.fillText(numbers.format(row + 1), rowNumW - PAD, HEADER_H + row * ROW_H - top + ROW_H / 2);
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
    const scheme = window.matchMedia("(prefers-color-scheme: dark)");
    scheme.addEventListener("change", redraw);
    return () => {
      observer.disconnect();
      scheme.removeEventListener("change", redraw);
    };
  }, [redraw]);

  useEffect(() => {
    redraw();
  });

  useEffect(() => {
    // New result: scroll to the top and clear the selection.
    setSelected(null);
    scrollerRef.current?.scrollTo(0, 0);
  }, [result.id]);

  function cellAt(clientX: number, clientY: number): Cell | null {
    const scroller = scrollerRef.current;
    if (!scroller) return null;
    const rect = scroller.getBoundingClientRect();
    const x = clientX - rect.left;
    const y = clientY - rect.top;
    if (x < rowNumW || y < HEADER_H || x > scroller.clientWidth || y > scroller.clientHeight) return null;
    const row = Math.floor((y - HEADER_H + scroller.scrollTop) / ROW_H);
    const dataX = x - rowNumW + scroller.scrollLeft;
    const col = offsets.findIndex((o, i) => i < widths.length && dataX >= o && dataX < (offsets[i + 1] ?? 0));
    if (row >= result.rows || col < 0) return null;
    return { row, col };
  }

  function onKeyDown(e: React.KeyboardEvent) {
    if (!selected) return;
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "c") {
      const value = cache.get(selected.row, selected.col);
      void navigator.clipboard.writeText(value ?? "NULL");
      e.preventDefault();
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
      row: Math.min(result.rows - 1, Math.max(0, selected.row + move[0])),
      col: Math.min(widths.length - 1, Math.max(0, selected.col + move[1])),
    };
    setSelected(next);
    const scroller = scrollerRef.current;
    if (!scroller) return;
    const y = next.row * ROW_H;
    if (y < scroller.scrollTop) scroller.scrollTop = y;
    else if (y + ROW_H > scroller.scrollTop + scroller.clientHeight - HEADER_H)
      scroller.scrollTop = y + ROW_H - scroller.clientHeight + HEADER_H;
    const x = offsets[next.col] ?? 0;
    const w = widths[next.col] ?? 0;
    if (x < scroller.scrollLeft) scroller.scrollLeft = x;
    else if (x + w > scroller.scrollLeft + scroller.clientWidth - rowNumW)
      scroller.scrollLeft = x + w - scroller.clientWidth + rowNumW;
  }

  return (
    <div className="grid-host">
      <canvas ref={canvasRef} className="grid-canvas" style={{ width: size.w, height: size.h }} />
      <div
        ref={scrollerRef}
        className="grid-scroller"
        tabIndex={0}
        onScroll={redraw}
        onMouseDown={(e) => {
          const cell = cellAt(e.clientX, e.clientY);
          if (cell) setSelected(cell);
        }}
        onKeyDown={onKeyDown}
        aria-label={t("results.gridLabel", { count: result.rows, formattedCount: numbers.format(result.rows) })}
      >
        <div style={{ width: totalW, height: totalH }} />
      </div>
    </div>
  );
}
