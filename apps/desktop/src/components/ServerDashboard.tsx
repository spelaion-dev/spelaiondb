// The process monitor's live dashboard (SDB-192). The sampler is the core's: it decides when to
// read, reads only while this view is shown, and keeps the samples in memory; this view starts
// it, says whether it is shown, and draws the state it sends (`DashboardStateItem`) as plain SVG
// lines with the theme's chart palette (ADR-040). Nothing is stored on disk.

import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { DashboardSampleItem_Serialize as DashboardSampleItem, DashboardStateItem_Serialize as DashboardStateItem, DashboardStateItem_Deserialize } from "../bindings";
import { formatBytes } from "./ServerObservation";

type Value = (sample: DashboardSampleItem) => number | null;
type Series = { key: string; label: string; value: Value };
type Format = (n: number) => string;

/** The chart's height in CSS pixels; its width is its box's, measured. */
const HEIGHT = 150;
const RIGHT = 8;
const TOP = 8;
const BOTTOM = 20;

function message(error: unknown) { return error instanceof Error ? error.message : String(error); }

/** A round top for the axis: 1, 2 or 5 times a power of ten, at least `max`. */
function niceTop(max: number): number {
  if (!(max > 0)) return 1;
  const power = 10 ** Math.floor(Math.log10(max));
  for (const step of [1, 2, 5, 10]) if (step * power >= max) return step * power;
  return 10 * power;
}

/**
 * One line's runs of samples, broken before a sample that starts a new line and around missing
 * values. A run of one sample (a rate right after a start or a pause) has nothing to join: it
 * is drawn as a dot, so a value the chart has is never shown as text only.
 */
function runs(samples: DashboardSampleItem[], value: Value): number[][] {
  const out: number[][] = [];
  let current: number[] | null = null;
  samples.forEach((sample, i) => {
    const v = value(sample);
    if (v === null || sample.gap) current = null;
    if (v === null) return;
    if (current === null) { current = []; out.push(current); }
    current.push(i);
  });
  return out;
}

/** Approximate width of a character of the axis text (`--text-xs`), for the left margin. */
const CHAR_PX = 6.5;

/** One chart: its title, the values of the sample pointed at (or the latest), and its lines. */
function DashboardChart({ title, hint, series, samples, format, clock }: {
  title: string; hint?: string; series: Series[]; samples: DashboardSampleItem[]; format: Format; clock(sample: DashboardSampleItem): string;
}) {
  const { t } = useTranslation("shell");
  const titleId = useId();
  const [pointed, setPointed] = useState<number | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(480);
  useLayoutEffect(() => {
    const element = box.current;
    if (!element) return;
    const measure = () => setWidth(Math.max(160, element.clientWidth));
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  const index = pointed !== null && pointed < samples.length ? pointed : samples.length - 1;
  const shown = index >= 0 ? samples[index] : null;
  const max = Math.max(0, ...samples.flatMap((s) => series.map((line) => line.value(s) ?? 0)));
  const top = niceTop(max);
  // Room on the left for the widest axis label, whatever its unit ("38.3 KB/s").
  const left = Math.ceil(Math.max(format(0).length, format(top).length) * CHAR_PX) + 12;
  const plotWidth = Math.max(1, width - left - RIGHT);
  const plotHeight = HEIGHT - TOP - BOTTOM;
  const x = (i: number) => left + (samples.length <= 1 ? plotWidth : (i / (samples.length - 1)) * plotWidth);
  const y = (v: number) => TOP + plotHeight - (v / top) * plotHeight;
  const ticks = [0, top / 2, top];

  function point(clientX: number, rect: DOMRect) {
    if (samples.length === 0) return;
    const ratio = (clientX - rect.left - left) / plotWidth;
    setPointed(Math.max(0, Math.min(samples.length - 1, Math.round(ratio * (samples.length - 1)))));
  }

  return <section className="dashboard-chart" aria-labelledby={titleId}>
    <header className="dashboard-chart-header">
      <h2 id={titleId}>{title}</h2>
      {shown && <span className="dashboard-chart-time">{clock(shown)}</span>}
    </header>
    <dl className="dashboard-values">
      {series.map((line, i) => {
        const v = shown ? line.value(shown) : null;
        return <div key={line.key} className="dashboard-value">
          {series.length > 1 && <span className="dashboard-swatch" style={{ background: `var(--chart-${i + 1})` }} aria-hidden="true" />}
          <dt>{line.label}</dt><dd>{v === null ? "—" : format(v)}</dd>
        </div>;
      })}
    </dl>
    <div ref={box} className="dashboard-plot"><svg className="dashboard-svg" width={width} height={HEIGHT} viewBox={`0 0 ${width} ${HEIGHT}`} role="img" tabIndex={0}
      aria-label={t("serverDashboard.chartLabel", { title, count: samples.length })}
      onMouseMove={(e) => point(e.clientX, e.currentTarget.getBoundingClientRect())} onMouseLeave={() => setPointed(null)} onBlur={() => setPointed(null)}
      onKeyDown={(e) => {
        if (samples.length === 0) return;
        const from = pointed ?? samples.length - 1;
        if (e.key === "ArrowLeft") { e.preventDefault(); setPointed(Math.max(0, from - 1)); }
        else if (e.key === "ArrowRight") { e.preventDefault(); setPointed(Math.min(samples.length - 1, from + 1)); }
        else if (e.key === "Home") { e.preventDefault(); setPointed(0); }
        else if (e.key === "End" || e.key === "Escape") { e.preventDefault(); setPointed(null); }
      }}>
      {ticks.map((tick) => <g key={tick}>
        <line className={tick === 0 ? "chart-baseline" : "chart-grid"} x1={left} x2={width - RIGHT} y1={y(tick)} y2={y(tick)} />
        {/* The middle line is unlabelled: half of a whole count's top need not be whole. */}
        {tick !== top / 2 && <text className="chart-axis-text" x={left - 6} y={y(tick) + 4} textAnchor="end">{format(tick)}</text>}
      </g>)}
      {samples.length > 0 && <>
        <text className="chart-axis-text" x={left} y={HEIGHT - 4}>{clock(samples[0]!)}</text>
        <text className="chart-axis-text" x={width - RIGHT} y={HEIGHT - 4} textAnchor="end">{clock(samples[samples.length - 1]!)}</text>
      </>}
      {pointed !== null && index >= 0 && <line className="chart-guide" x1={x(index)} x2={x(index)} y1={TOP} y2={TOP + plotHeight} />}
      {series.map((line, i) => {
        const color = `var(--chart-${i + 1})`;
        const at = (n: number) => ({ x: x(n), y: y(line.value(samples[n]!) ?? 0) });
        const lines = runs(samples, line.value);
        const path = lines.filter((run) => run.length > 1)
          .map((run) => run.map((n, k) => `${k === 0 ? "M" : "L"}${at(n).x.toFixed(1)},${at(n).y.toFixed(1)}`).join("")).join("");
        return <g key={line.key} data-series={line.key}>
          <path className="chart-line" style={{ stroke: color }} d={path} />
          {lines.filter((run) => run.length === 1).map(([n]) => <circle key={n} className="chart-dot" cx={at(n!).x} cy={at(n!).y} r={3.5} style={{ fill: color }} />)}
        </g>;
      })}
    </svg></div>
    {hint && <p className="dashboard-hint">{hint}</p>}
  </section>;
}

/** The dashboard of the monitor session `session` (null while it connects). */
export function ServerDashboard({ session, engine }: { session: string | null; engine: string }) {
  const { t, i18n } = useTranslation("shell");
  const [state, setState] = useState<{ value: DashboardStateItem; received: number } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  // Start (or watch again) the core's sampler while this view is mounted; hidden, it pauses.
  useEffect(() => {
    if (!session) return;
    let live = true;
    // The command's channel type is the union of both serde forms; the core only ever sends the
    // serialized one.
    const channel = new Channel<DashboardStateItem_Deserialize>();
    channel.onmessage = (value) => { if (live) setState({ value: value as DashboardStateItem, received: Date.now() }); };
    setError(null);
    call(commands.startDashboard(session, channel)).catch((e) => { if (live) setError(message(e)); });
    const visibility = () => { void call(commands.setDashboardVisible(session, document.visibilityState === "visible")).catch(() => undefined); };
    document.addEventListener("visibilitychange", visibility);
    return () => {
      live = false;
      document.removeEventListener("visibilitychange", visibility);
      void call(commands.setDashboardVisible(session, false)).catch(() => undefined);
    };
  }, [session, attempt]);

  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }), [i18n.language]);
  const whole = useMemo(() => new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 0 }), [i18n.language]);
  const samples = state?.value.samples ?? [];
  const latest = samples.length ? samples[samples.length - 1] : null;
  // Samples carry the time since the sampler started; the latest one is about when it arrived.
  const clock = (sample: DashboardSampleItem) => {
    const at = (state?.received ?? Date.now()) - ((latest?.atMs ?? 0) - sample.atMs);
    return new Date(at).toLocaleTimeString(i18n.language);
  };
  const phase = state?.value.phase;
  const seconds = Math.round((state?.value.intervalMs ?? 5000) / 1000);
  let status = t("serverDashboard.starting");
  if (phase) {
    switch (phase.kind) {
      case "sampling": status = t("serverDashboard.sampling", { seconds }); break;
      case "paused": status = t("serverDashboard.paused"); break;
      case "failed": status = t("serverDashboard.failedStatus"); break;
      case "stopped": status = t("serverDashboard.stopped"); break;
      default: { const never: never = phase; return never; }
    }
  }
  const failure = error ?? (phase?.kind === "failed" ? phase.message : null);
  const waitingHint = ["postgres", "mysql", "mariadb", "sqlserver"].includes(engine) ? t(`serverDashboard.waitingHint.${engine}`) : undefined;

  return <div className="dashboard" aria-busy={!latest}>
    <div className="process-toolbar dashboard-toolbar">
      <span className="muted small" role="status">{status}</span>
      <span className="muted small">{t("serverDashboard.memoryOnly")}</span>
    </div>
    {failure && <div className="banner banner-warn" role="alert">
      {t("serverDashboard.failed", { detail: failure })}
      <button onClick={() => { setState(null); setAttempt((n) => n + 1); }}>{t("serverDashboard.tryAgain")}</button>
    </div>}
    {!latest && !failure && <div className="empty">{t("serverDashboard.firstSample")}</div>}
    {latest && <div className="dashboard-grid">
      <DashboardChart title={t("serverDashboard.sessions")} hint={waitingHint} samples={samples} clock={clock} format={(n) => whole.format(n)} series={[
        { key: "sessions", label: t("serverDashboard.connected"), value: (s) => s.sessions },
        { key: "active", label: t("serverDashboard.active"), value: (s) => s.active },
        { key: "waiting", label: t("serverDashboard.waiting"), value: (s) => s.waiting },
      ]} />
      <DashboardChart title={t("serverDashboard.transactions")} samples={samples} clock={clock} format={(n) => numbers.format(n)} series={[
        { key: "transactions", label: t("serverDashboard.perSecond"), value: (s) => s.transactionsPerSecond },
      ]} />
      <DashboardChart title={t("serverDashboard.reads")} hint={t("serverDashboard.readsHint")} samples={samples} clock={clock} format={(n) => numbers.format(n)} series={[
        { key: "reads", label: t("serverDashboard.pagesPerSecond"), value: (s) => s.readsPerSecond },
      ]} />
      <DashboardChart title={t("serverDashboard.log")} hint={t("serverDashboard.logHint")} samples={samples} clock={clock} format={(n) => `${formatBytes(n, i18n.language)}/s`} series={[
        { key: "log", label: t("serverDashboard.written"), value: (s) => s.logBytesPerSecond },
      ]} />
    </div>}
  </div>;
}
