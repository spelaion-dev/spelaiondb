// Charts from a result (ADR-040). The core prepares the points from the stored result
// (`result_chart`: at most MAX_CHART_POINTS of them, combined as it says) and this draws them as
// plain SVG with the theme's chart palette (`--chart-1` … `--chart-8`). No row reaches React.
// "Save image…" writes what is on screen, as SVG or PNG, through the core (`save_chart_image`).

import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { TFunction } from "i18next";
import { useTranslation } from "react-i18next";
import {
  call,
  commands,
  type ChartAggregateItem,
  type ChartKindItem,
  type ChartLabelItem,
  type ChartPointsItem,
  type ColumnItem,
} from "../api";
import { chooseSavePath } from "../dialogs";
import type { ResultMeta } from "../store";
import { CHART_SERIES, numericColumns, type ChartSettings } from "../chartDefaults";

export { CHART_SERIES, defaultChart, numericColumns, type ChartSettings } from "../chartDefaults";

const PIE_AGGREGATES: ChartAggregateItem[] = ["sum", "count"];
const AGGREGATES: ChartAggregateItem[] = ["sum", "avg", "min", "max", "count"];
const KINDS: ChartKindItem[] = ["bar", "line", "pie"];
/** Approximate width of a character of the chart's small text, for layout. */
const CHAR_PX = 6.5;

/** Settings a pie accepts: one series, added up. */
function fitKind(settings: ChartSettings, kind: ChartKindItem): ChartSettings {
  if (kind !== "pie") return { ...settings, kind };
  return {
    kind,
    x: settings.x,
    y: settings.y.slice(0, 1),
    aggregate: PIE_AGGREGATES.includes(settings.aggregate) ? settings.aggregate : "sum",
  };
}

type Load =
  | { kind: "idle" }
  | { kind: "loading"; previous: ChartPointsItem | null }
  | { kind: "done"; points: ChartPointsItem }
  | { kind: "error"; message: string };

export function ResultChart({
  result,
  settings,
  onSettings,
  scope,
}: {
  result: ResultMeta;
  settings: ChartSettings | null;
  onSettings(settings: ChartSettings): void;
  /** The rows are one page of a table, or only the loaded part of a query. */
  scope?: "page" | "loaded";
}) {
  const { t, i18n } = useTranslation("editor");
  const [load, setLoad] = useState<Load>({ kind: "idle" });
  const [slow, setSlow] = useState(false);
  const [saveNote, setSaveNote] = useState<{ error: boolean; text: string } | null>(null);
  const svgRef = useRef<SVGSVGElement>(null);
  const columns = result.columns;
  const numeric = useMemo(() => numericColumns(columns), [columns]);
  const request = settings ? JSON.stringify(settings) : null;
  const view = JSON.stringify([result.rows, result.sort, result.filter, result.columnFilters]);

  useEffect(() => {
    if (!settings || !result.complete) {
      setLoad({ kind: "idle" });
      return;
    }
    let alive = true;
    setSlow(false);
    const timer = window.setTimeout(() => alive && setSlow(true), 300);
    setLoad((current) => ({
      kind: "loading",
      previous: current.kind === "done" ? current.points : current.kind === "loading" ? current.previous : null,
    }));
    call(commands.resultChart(result.id, settings))
      .then((points) => {
        if (alive && points) setLoad({ kind: "done", points });
      })
      .catch((e: unknown) => {
        if (alive) setLoad({ kind: "error", message: e instanceof Error ? e.message : String(e) });
      });
    return () => {
      alive = false;
      window.clearTimeout(timer);
    };
    // `request` and `view` stand for `settings` and the result's view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [result.id, result.complete, request, view]);

  useEffect(() => () => void call(commands.cancelChart(result.id)).catch(() => undefined), [result.id]);

  if (!settings) {
    return <div className="empty chart-empty">{t("chart.noNumbers")}</div>;
  }
  const points = load.kind === "done" ? load.points : load.kind === "loading" ? load.previous : null;
  const xName = columns[settings.x]?.name ?? "?";
  const yNames = settings.y.map((c) => columns[c]?.name ?? "?");
  const aggregates = settings.kind === "pie" ? PIE_AGGREGATES : AGGREGATES;

  const save = async () => {
    const svg = svgRef.current;
    if (!svg) return;
    setSaveNote(null);
    try {
      const path = await chooseSavePath({
        title: t("chart.save"),
        defaultPath: "chart.png",
        filters: [
          { name: "PNG", extensions: ["png"] },
          { name: "SVG", extensions: ["svg"] },
        ],
      });
      if (!path) return;
      const format = /\.svg$/i.test(path) ? "svg" : "png";
      const text = standaloneSvg(svg);
      const data = format === "svg" ? text : await svgToPng(text, svg.clientWidth, svg.clientHeight);
      await call(commands.saveChartImage(path, format, data));
      setSaveNote({ error: false, text: t("chart.saved", { name: path.split(/[\\/]/).pop() ?? path }) });
    } catch (e) {
      setSaveNote({ error: true, text: t("chart.saveFailed", { detail: e instanceof Error ? e.message : String(e) }) });
    }
  };

  return (
    <div className="result-chart">
      <div className="chart-controls">
        <div className="segmented" role="radiogroup" aria-label={t("chart.kind")}>
          {KINDS.map((kind) => (
            <button
              key={kind}
              type="button"
              role="radio"
              aria-checked={settings.kind === kind}
              className={settings.kind === kind ? "active" : ""}
              onClick={() => onSettings(fitKind(settings, kind))}
            >
              {t(`chart.kinds.${kind}`)}
            </button>
          ))}
        </div>
        <label className="chart-field">
          <span>{t(settings.kind === "pie" ? "chart.slices" : "chart.x")}</span>
          <select value={settings.x} onChange={(e) => onSettings({ ...settings, x: Number(e.target.value) })}>
            {columns.map((c, i) => (
              <option key={i} value={i}>{c.name}</option>
            ))}
          </select>
        </label>
        <ValuesPicker
          columns={columns}
          numeric={numeric}
          single={settings.kind === "pie"}
          selected={settings.y}
          onChange={(y) => onSettings({ ...settings, y })}
        />
        <label className="chart-field">
          <span>{t("chart.aggregate")}</span>
          <select
            value={settings.aggregate}
            onChange={(e) => onSettings({ ...settings, aggregate: e.target.value as ChartAggregateItem })}
          >
            {aggregates.map((a) => (
              <option key={a} value={a}>{t(`chart.aggregates.${a}`)}</option>
            ))}
          </select>
        </label>
        <div className="chart-save">
          {saveNote && (
            <span className={saveNote.error ? "status-error" : "muted"} role={saveNote.error ? "alert" : "status"}>
              {saveNote.text}
            </span>
          )}
          <button type="button" disabled={!points || points.labels.length === 0} onClick={() => void save()}>
            {t("chart.save")}
          </button>
        </div>
      </div>
      {points && <p className="chart-note" role="status">{describe(points, settings, xName, scope, t, i18n.language)}</p>}
      <div className="chart-plot">
        {!result.complete ? (
          <div className="empty">{t("chart.waiting")}</div>
        ) : load.kind === "error" ? (
          <div className="empty status-error" role="alert">{t("chart.failed", { detail: load.message })}</div>
        ) : points && points.labels.length === 0 ? (
          <div className="empty">{t("chart.noRows")}</div>
        ) : points ? (
          <ChartSvg
            ref={svgRef}
            points={points}
            settings={settings}
            title={chartTitle(points, settings, xName, yNames, t)}
            language={i18n.language}
          />
        ) : (
          slow && <div className="empty">{t("chart.drawing")}</div>
        )}
      </div>
    </div>
  );
}

/** The Y columns: a menu of the numeric columns (one only for a pie). */
function ValuesPicker({
  columns,
  numeric,
  single,
  selected,
  onChange,
}: {
  columns: ColumnItem[];
  numeric: number[];
  single: boolean;
  selected: number[];
  onChange(y: number[]): void;
}) {
  const { t } = useTranslation("editor");
  const [open, setOpen] = useState(false);
  const id = useId();
  const box = useRef<HTMLDivElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent) {
        if (e.key !== "Escape") return;
        e.stopPropagation();
        button.current?.focus();
      }
      if (e instanceof MouseEvent && box.current?.contains(e.target as Node)) return;
      setOpen(false);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close, true);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close, true);
    };
  }, [open]);
  const names = selected.map((c) => columns[c]?.name ?? "?").join(", ");
  return (
    <div className="chart-field chart-values" ref={box}>
      <span id={`${id}-label`}>{t("chart.values")}</span>
      <button
        ref={button}
        type="button"
        className="chart-values-button"
        aria-haspopup="true"
        aria-expanded={open}
        aria-labelledby={`${id}-label ${id}-names`}
        onClick={() => setOpen((o) => !o)}
      >
        <span id={`${id}-names`}>{names || t("chart.noValues")}</span>
      </button>
      {open && (
        <div className="chart-values-menu" role={single ? "radiogroup" : "group"} aria-label={t("chart.values")}>
          {numeric.map((c) => {
            const checked = selected.includes(c);
            return (
              <label key={c} className="check">
                <input
                  type={single ? "radio" : "checkbox"}
                  name={`${id}-values`}
                  checked={checked}
                  disabled={!single && !checked && selected.length >= CHART_SERIES}
                  onChange={() => {
                    if (single) onChange([c]);
                    else if (checked) onChange(selected.filter((s) => s !== c));
                    else onChange([...selected, c].sort((a, b) => a - b));
                  }}
                />
                {columns[c]?.name}
              </label>
            );
          })}
          {!single && <p className="muted small">{t("chart.valuesHint", { count: CHART_SERIES })}</p>}
        </div>
      )}
    </div>
  );
}

type T = TFunction<"editor">;

function labelText(label: ChartLabelItem, t: T): string {
  switch (label.kind) {
    case "value":
      return label.text;
    case "null":
      return "NULL";
    case "other":
      return t("chart.other");
  }
}

function chartTitle(points: ChartPointsItem, settings: ChartSettings, x: string, y: string[], t: T): string {
  const values = y.join(", ");
  if (points.reduction === "rows" && settings.aggregate !== "count") return t("chart.titlePlain", { y: values, x });
  return t("chart.title", { aggregate: t(`chart.aggregates.${settings.aggregate}`), y: values, x });
}

/** What the chart combined, left out and read: said under the controls (ADR-040). */
function describe(
  points: ChartPointsItem,
  settings: ChartSettings,
  x: string,
  scope: "page" | "loaded" | undefined,
  t: T,
  language: string,
): string {
  const n = new Intl.NumberFormat(language);
  const rows = n.format(points.rows);
  const count = points.labels.length;
  const aggregate = t(`chart.aggregates.${settings.aggregate}`).toLowerCase();
  const parts: string[] = [];
  switch (points.reduction) {
    case "rows":
      parts.push(t("chart.note.rows", { count: points.rows, rows }));
      break;
    case "grouped":
      parts.push(t("chart.note.grouped", { rows, points: n.format(count), x, aggregate }));
      break;
    case "ranges":
      parts.push(t("chart.note.ranges", { rows, points: n.format(count), x, aggregate }));
      break;
    case "other":
      parts.push(
        settings.kind === "pie"
          ? t("chart.note.largest", { rows, kept: n.format(count - 1), x, aggregate })
          : t("chart.note.first", { rows, kept: n.format(count - 1), x, aggregate }),
      );
      break;
  }
  if (points.skippedRows > 0) parts.push(t("chart.note.skipped", { count: points.skippedRows, formattedCount: n.format(points.skippedRows), x }));
  if (points.emptyValues > 0) parts.push(t("chart.note.empty", { count: points.emptyValues, formattedCount: n.format(points.emptyValues) }));
  if (points.hiddenSlices > 0) parts.push(t("chart.note.hidden", { count: points.hiddenSlices, formattedCount: n.format(points.hiddenSlices) }));
  if (scope) parts.push(t(scope === "page" ? "chart.note.page" : "chart.note.loaded"));
  return parts.join(" ");
}

/** Round steps for an axis from `lo` to `hi`. */
function niceTicks(lo: number, hi: number, count: number): { ticks: number[]; lo: number; hi: number; step: number } {
  if (lo === hi) {
    const pad = lo === 0 ? 1 : Math.abs(lo) * 0.1;
    lo -= pad;
    hi += pad;
  }
  const raw = (hi - lo) / count;
  const magnitude = 10 ** Math.floor(Math.log10(raw));
  const norm = raw / magnitude;
  const step = (norm < 1.5 ? 1 : norm < 3 ? 2 : norm < 7 ? 5 : 10) * magnitude;
  const start = Math.floor(lo / step) * step;
  const end = Math.ceil(hi / step) * step;
  const ticks: number[] = [];
  for (let v = start, i = 0; v <= end + step / 2 && i < 50; v += step, i++) ticks.push(Math.abs(v) < step / 1e6 ? 0 : v);
  return { ticks, lo: start, hi: end, step };
}

function shorten(text: string, chars: number): string {
  return text.length > chars ? `${text.slice(0, Math.max(1, chars - 1))}…` : text;
}

interface Hover {
  index: number;
  x: number;
  y: number;
}

function ChartSvg({
  ref,
  points,
  settings,
  title,
  language,
}: {
  ref: React.Ref<SVGSVGElement>;
  points: ChartPointsItem;
  settings: ChartSettings;
  title: string;
  language: string;
}) {
  const { t } = useTranslation("editor");
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const [hover, setHover] = useState<Hover | null>(null);
  useLayoutEffect(() => {
    const element = host.current;
    if (!element) return;
    const measure = () => setSize({ width: element.clientWidth, height: element.clientHeight });
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  useEffect(() => setHover(null), [points]);
  const full = useMemo(() => new Intl.NumberFormat(language, { maximumFractionDigits: 6 }), [language]);
  const { width, height } = size;
  const n = points.labels.length;
  const labels = points.labels.map((l) => labelText(l, t));
  const kindName = t(`chart.kinds.${settings.kind}`);
  const ariaLabel = t("chart.accessible", { kind: kindName, title, count: n });

  let body: React.ReactNode = null;
  let tooltip: { left: number; top: number; title: string; rows: { color: string; name: string; value: string }[] } | null = null;
  let onMove: ((x: number, y: number) => void) | null = null;

  if (width > 0 && height > 0 && settings.kind === "pie") {
    const series = points.series[0];
    const values = series?.values.map((v) => (v != null && v > 0 ? v : 0)) ?? [];
    const total = values.reduce((a, b) => a + b, 0) || 1;
    const share = (v: number) => Math.round((v / total) * 1000) / 10;
    const legendWidth = Math.min(Math.max(...labels.map((l) => shorten(l, 28).length), 8) * CHAR_PX + 96, width * 0.5);
    const top = 28;
    const r = Math.max(8, Math.min((height - top - 12) / 2, (width - legendWidth - 32) / 2));
    const cx = 16 + r;
    const cy = top + (height - top) / 2 - 4;
    const color = (i: number) => (points.labels[i]?.kind === "other" ? "var(--chart-other)" : `var(--chart-${(i % CHART_SERIES) + 1})`);
    let angle = -Math.PI / 2;
    const slices = values.map((v, i) => {
      const start = angle;
      const end = angle + (v / total) * Math.PI * 2;
      angle = end;
      return { i, start, end, v };
    });
    onMove = (x, y) => {
      const dx = x - cx;
      const dy = y - cy;
      if (dx * dx + dy * dy > r * r) return setHover(null);
      let a = Math.atan2(dy, dx);
      if (a < -Math.PI / 2) a += Math.PI * 2;
      const hit = slices.find((s) => a >= s.start && a < s.end);
      setHover(hit ? { index: hit.i, x, y } : null);
    };
    body = (
      <>
        {slices.map(({ i, start, end, v }) =>
          v <= 0 ? null : end - start >= Math.PI * 2 - 1e-9 ? (
            <circle key={i} cx={cx} cy={cy} r={r} className="chart-slice" style={{ fill: color(i) }} />
          ) : (
            <path
              key={i}
              className={`chart-slice${hover?.index === i ? " hovered" : ""}`}
              style={{ fill: color(i) }}
              d={`M${cx},${cy} L${cx + r * Math.cos(start)},${cy + r * Math.sin(start)} A${r},${r} 0 ${end - start > Math.PI ? 1 : 0} 1 ${cx + r * Math.cos(end)},${cy + r * Math.sin(end)} Z`}
            />
          ),
        )}
        {labels.map((label, i) => {
          const y = top + 8 + i * 22;
          const x = cx + r + 24;
          return (
            <g key={i}>
              <rect x={x} y={y} width={10} height={10} rx={2} style={{ fill: color(i) }} />
              <text x={x + 16} y={y + 9} className="chart-legend-text">
                {shorten(label, 28)}
              </text>
              <text x={x + 16} y={y + 9} dx={shorten(label, 28).length * CHAR_PX + 8} className="chart-axis-text">
                {`${full.format(values[i] ?? 0)} · ${share(values[i] ?? 0)}%`}
              </text>
            </g>
          );
        })}
      </>
    );
    if (hover && series) {
      tooltip = {
        left: hover.x,
        top: hover.y,
        title: labels[hover.index] ?? "",
        rows: [{ color: color(hover.index), name: series.name, value: `${full.format(values[hover.index] ?? 0)} (${share(values[hover.index] ?? 0)}%)` }],
      };
    }
  } else if (width > 0 && height > 0) {
    const all = points.series.flatMap((s) => s.values).filter((v): v is number => v != null);
    let lo = all.length ? Math.min(...all) : 0;
    let hi = all.length ? Math.max(...all) : 1;
    if (settings.kind === "bar") {
      lo = Math.min(lo, 0);
      hi = Math.max(hi, 0);
    }
    const axis = niceTicks(lo, hi, Math.max(2, Math.min(6, Math.floor(height / 60))));
    const compact = Math.max(Math.abs(axis.lo), Math.abs(axis.hi)) >= 10_000;
    const tickFormat = new Intl.NumberFormat(language, compact ? { notation: "compact", maximumFractionDigits: 1 } : { maximumFractionDigits: Math.max(0, -Math.floor(Math.log10(axis.step))) });
    const tickLabels = axis.ticks.map((v) => tickFormat.format(v));
    const legendRows = points.series.length > 1 ? 1 : 0;
    const top = 28 + legendRows * 20;
    const left = Math.max(32, Math.max(...tickLabels.map((l) => l.length)) * CHAR_PX + 12);
    const right = 16;
    const bottom = 28;
    const plotW = Math.max(1, width - left - right);
    const plotH = Math.max(1, height - top - bottom);
    const yOf = (v: number) => top + plotH - ((v - axis.lo) / (axis.hi - axis.lo)) * plotH;
    const band = plotW / Math.max(1, n);
    const positions = points.positions?.map((p) => p ?? 0) ?? null;
    const p0 = positions?.[0] ?? 0;
    const pEnd = positions?.[n - 1] ?? 0;
    let xs: number[];
    if (settings.kind === "line" && positions && n > 1 && pEnd > p0) {
      const span = pEnd - p0;
      xs = positions.map((p) => left + ((p - p0) / span) * plotW);
    } else if (settings.kind === "line") {
      xs = labels.map((_, i) => (n === 1 ? left + plotW / 2 : left + (i / (n - 1)) * plotW));
    } else {
      xs = labels.map((_, i) => left + band * (i + 0.5));
    }
    // X labels: as many as fit, evenly spread.
    const fit = Math.max(1, Math.floor(plotW / 96));
    const every = Math.ceil(n / fit);
    const labelChars = Math.max(4, Math.floor((Math.min(plotW / fit, 160) - 8) / CHAR_PX));
    const series = points.series;
    const color = (s: number) => `var(--chart-${(s % CHART_SERIES) + 1})`;
    const zero = yOf(Math.min(Math.max(0, axis.lo), axis.hi));
    onMove = (x) => {
      if (x < left - 4 || x > left + plotW + 4) return setHover(null);
      let index: number;
      if (settings.kind === "bar") index = Math.min(n - 1, Math.max(0, Math.floor((x - left) / band)));
      else {
        let lo2 = 0;
        let hi2 = n - 1;
        while (lo2 < hi2) {
          const mid = (lo2 + hi2) >> 1;
          if ((xs[mid] ?? 0) < x) lo2 = mid + 1;
          else hi2 = mid;
        }
        index = lo2 > 0 && Math.abs((xs[lo2 - 1] ?? 0) - x) < Math.abs((xs[lo2] ?? 0) - x) ? lo2 - 1 : lo2;
      }
      setHover({ index, x: xs[index] ?? 0, y: top });
    };
    let marks: React.ReactNode;
    if (settings.kind === "bar") {
      // Thin marks: a bar is at most 48px wide, however few the points.
      const barW = Math.min((band * (n === 1 ? 0.4 : 0.8)) / series.length, 48);
      const groupW = barW * series.length;
      const gap = barW > 6 ? 2 : 0;
      marks = series.map((s, si) => (
        <g key={s.column} style={{ fill: color(si) }}>
          {s.values.map((v, i) =>
            v == null ? null : (
              <rect
                key={i}
                x={(xs[i] ?? 0) - groupW / 2 + si * barW + gap / 2}
                width={Math.max(0.5, barW - gap)}
                y={Math.min(yOf(v), zero)}
                height={Math.max(v === 0 ? 0 : 1, Math.abs(yOf(v) - zero))}
              />
            ),
          )}
        </g>
      ));
    } else {
      marks = series.map((s, si) => {
        // A line breaks where a point has no value.
        const segments: string[] = [];
        let current = "";
        s.values.forEach((v, i) => {
          if (v == null) {
            if (current) segments.push(current);
            current = "";
          } else current += `${current ? "L" : "M"}${xs[i]},${yOf(v)}`;
        });
        if (current) segments.push(current);
        return (
          <g key={s.column}>
            {segments.map((d, k) => (
              <path key={k} d={d} className="chart-line" style={{ stroke: color(si) }} />
            ))}
            {n <= 60 &&
              s.values.map((v, i) =>
                v == null ? null : <circle key={i} cx={xs[i]} cy={yOf(v)} r={3} className="chart-dot" style={{ fill: color(si) }} />,
              )}
          </g>
        );
      });
    }
    let legendX = 4;
    body = (
      <>
        {legendRows > 0 &&
          series.map((s, si) => {
            const x = legendX;
            legendX += shorten(s.name, 24).length * CHAR_PX + 30;
            return (
              <g key={s.column}>
                <rect x={x} y={28} width={10} height={10} rx={2} style={{ fill: color(si) }} />
                <text x={x + 16} y={37} className="chart-legend-text">{shorten(s.name, 24)}</text>
              </g>
            );
          })}
        {axis.ticks.map((v, i) => (
          <g key={i}>
            <line x1={left} x2={left + plotW} y1={yOf(v)} y2={yOf(v)} className={v === 0 ? "chart-baseline" : "chart-grid"} />
            <text x={left - 6} y={yOf(v)} dy="0.32em" textAnchor="end" className="chart-axis-text">{tickLabels[i]}</text>
          </g>
        ))}
        {labels.map((label, i) =>
          i % every === 0 ? (
            <text key={i} x={xs[i]} y={top + plotH + 16} textAnchor="middle" className="chart-axis-text">
              {shorten(label, labelChars)}
            </text>
          ) : null,
        )}
        {marks}
        {hover && (
          <line data-export="skip" x1={xs[hover.index]} x2={xs[hover.index]} y1={top} y2={top + plotH} className="chart-guide" />
        )}
      </>
    );
    if (hover) {
      tooltip = {
        left: xs[hover.index] ?? 0,
        top: top,
        title: labels[hover.index] ?? "",
        rows: series.map((s, si) => ({
          color: color(si),
          name: s.name,
          value: s.values[hover.index] == null ? t("chart.noValue") : full.format(s.values[hover.index] as number),
        })),
      };
    }
  }

  const point = (e: React.MouseEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    onMove?.(e.clientX - rect.left, e.clientY - rect.top);
  };
  return (
    <div className="chart-host" ref={host}>
      <svg
        ref={ref}
        className="chart-svg"
        width={width}
        height={height}
        viewBox={`0 0 ${width} ${height}`}
        role="img"
        aria-label={ariaLabel}
        tabIndex={0}
        onMouseMove={point}
        onMouseLeave={() => setHover(null)}
        onBlur={() => setHover(null)}
        onKeyDown={(e) => {
          if (n === 0 || !["ArrowLeft", "ArrowRight", "Home", "End"].includes(e.key)) return;
          e.preventDefault();
          const current = hover?.index ?? -1;
          const index =
            e.key === "Home" ? 0 : e.key === "End" ? n - 1 : e.key === "ArrowLeft" ? Math.max(0, current - 1) : Math.min(n - 1, current + 1);
          setHover({ index, x: 0, y: 0 });
        }}
      >
        <text x={4} y={14} className="chart-title">{shorten(title, Math.max(8, Math.floor(width / 7)))}</text>
        {body}
      </svg>
      {tooltip && (
        <div
          className="chart-tooltip"
          role="status"
          style={{
            left: Math.min(Math.max(0, tooltip.left + 12), Math.max(0, width - 220)),
            top: tooltip.top,
          }}
        >
          <div className="chart-tooltip-title">{tooltip.title}</div>
          {tooltip.rows.map((row, i) => (
            <div key={i} className="chart-tooltip-row">
              <span className="chart-swatch" style={{ background: row.color }} />
              <span>{row.name}</span>
              <span className="chart-tooltip-value">{row.value}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** The chart as a standalone SVG document: every color and font the theme gives it written on
 *  the elements (a file has no stylesheet), on the chart's own background. */
function standaloneSvg(svg: SVGSVGElement): string {
  const clone = svg.cloneNode(true) as SVGSVGElement;
  const source = svg.querySelectorAll<SVGElement>("*");
  const target = clone.querySelectorAll<SVGElement>("*");
  const properties = ["fill", "stroke", "stroke-width", "stroke-dasharray", "stroke-linecap", "stroke-linejoin", "opacity", "font-family", "font-size", "font-weight"];
  source.forEach((element, i) => {
    const style = getComputedStyle(element);
    target[i]?.setAttribute("style", properties.map((p) => `${p}:${style.getPropertyValue(p)}`).join(";"));
    target[i]?.removeAttribute("class");
  });
  clone.querySelectorAll('[data-export="skip"]').forEach((element) => element.remove());
  clone.removeAttribute("class");
  clone.removeAttribute("tabindex");
  clone.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  const width = svg.clientWidth;
  const height = svg.clientHeight;
  clone.setAttribute("width", String(width));
  clone.setAttribute("height", String(height));
  const background = document.createElementNS("http://www.w3.org/2000/svg", "rect");
  background.setAttribute("width", "100%");
  background.setAttribute("height", "100%");
  background.setAttribute("style", `fill:${getComputedStyle(svg).backgroundColor}`);
  clone.insertBefore(background, clone.firstChild);
  return new XMLSerializer().serializeToString(clone);
}

/** A PNG (as a data URL) of a standalone SVG, at twice its size. */
async function svgToPng(text: string, width: number, height: number): Promise<string> {
  const image = new Image();
  image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(text)}`;
  await image.decode();
  const scale = 2;
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.round(width * scale));
  canvas.height = Math.max(1, Math.round(height * scale));
  const context = canvas.getContext("2d");
  if (!context) throw new Error("no canvas");
  context.scale(scale, scale);
  context.drawImage(image, 0, 0, width, height);
  return canvas.toDataURL("image/png");
}
