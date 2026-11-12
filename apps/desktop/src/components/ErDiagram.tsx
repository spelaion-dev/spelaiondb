// An ER diagram tab (ADR-046). The core reads the tables from the catalog on this tab's own
// session, lays them out and writes the Mermaid and DBML exports; this component only draws the
// layout it gets, as plain SVG (like charts, ADR-040). Panning and zooming change one transform
// and never ask for the layout again: a new layout comes only with new display options or a
// refresh (`data-layouts` counts them, for the smoke test).

import { Channel } from "@tauri-apps/api/core";
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type {
  CardinalityItem,
  DiagramColumnsItem,
  DiagramEdgeItem,
  DiagramLayoutItem_Serialize as DiagramLayout,
  DiagramNodeItem_Serialize as DiagramNode,
  DiagramProgressItem,
  DiagramSideItem,
  DiagramSummaryItem_Serialize as DiagramSummary,
} from "../bindings";
import { connectTabFromExplorer, explorerDatabase, openSession, openTableData } from "../connect";
import { chooseSavePath } from "../dialogs";
import { openDiagram } from "../diagramTabs";
import { useApp, type Tab } from "../store";
import { Icon } from "./Icons";
import { ObjectMenu } from "./ObjectMenu";

/** Ids the core keeps each tab's model under. */
let nextDiagram = 1;

const MIN_ZOOM = 0.05;
const MAX_ZOOM = 4;
/** Keys of the focused diagram (docs/ui-guidelines.md, "ER diagram" in the shortcut map). */
const ZOOM_IN_KEYS = ["+", "="];
const ZOOM_OUT_KEYS = ["-", "_"];
const FIT_KEY = "0";
const ACTUAL_SIZE_KEY = "1";
const PAN_STEP = 48;
/** Rows of a box are this tall; the core lays them out at this height. */
const ROW_HEIGHT = 18;
/** Largest PNG export, in pixels (WebView2 refuses bigger canvases). */
const MAX_PNG_PIXELS = 48_000_000;
const MAX_PNG_SIDE = 16_384;

interface View {
  x: number;
  y: number;
  k: number;
}

interface Options {
  columns: DiagramColumnsItem;
  types: boolean;
  comments: boolean;
}

type Load =
  | { kind: "idle" }
  | { kind: "reading"; progress: DiagramProgressItem | null }
  | { kind: "done"; summary: DiagramSummary }
  | { kind: "error"; message: string };

function reason(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The width of one character and the row height of the diagram's monospace font. */
function measureFont(host: Element): { charWidth: number; lineHeight: number } {
  const style = getComputedStyle(host);
  const size = style.getPropertyValue("--text-sm").trim() || "12px";
  const family = style.getPropertyValue("--font-code").trim() || "monospace";
  const canvas = document.createElement("canvas");
  const context = canvas.getContext("2d");
  if (!context) return { charWidth: 7.2, lineHeight: ROW_HEIGHT };
  context.font = `600 ${size} ${family}`;
  const sample = "0123456789abcdefghijklmnopqrstuvwxyz".repeat(3);
  return { charWidth: context.measureText(sample).width / sample.length, lineHeight: ROW_HEIGHT };
}

/** The view that shows the whole diagram in a `width` × `height` area. */
function fitted(layout: DiagramLayout, width: number, height: number): View {
  const k = Math.min(1, Math.max(MIN_ZOOM, Math.min(width / layout.width, height / layout.height)));
  return { k, x: (width - layout.width * k) / 2, y: Math.max(0, (height - layout.height * k) / 2) };
}

function edgePath(edge: DiagramEdgeItem): string {
  const [p0, p1, p2, p3, p4, p5] = edge.points;
  if (!p0 || !p1 || !p2 || !p3 || !p4 || !p5) return "";
  return `M${p0.x} ${p0.y}L${p1.x} ${p1.y}C${p2.x} ${p2.y} ${p3.x} ${p3.y} ${p4.x} ${p4.y}L${p5.x} ${p5.y}`;
}

/** Crow's foot marks at one end of a relation: `x`, `y` on the box, going out to `side`. */
function endMarks(x: number, y: number, side: DiagramSideItem, cardinality: CardinalityItem): ReactNode {
  const d = side === "left" ? -1 : 1;
  const bar = (at: number) => <path d={`M${x + d * at} ${y - 5}V${y + 5}`} />;
  const ring = (at: number) => <circle className="er-ring" cx={x + d * at} cy={y} r={3.5} />;
  switch (cardinality) {
    case "one":
      return <>{bar(6)}{bar(10)}</>;
    case "zeroOrOne":
      return <>{bar(6)}{ring(13)}</>;
    case "zeroOrMany":
      return (
        <>
          <path d={`M${x} ${y - 6}L${x + d * 9} ${y}L${x} ${y + 6}M${x} ${y}H${x + d * 9}`} />
          {ring(13)}
        </>
      );
  }
}

/** The drawing of a layout, memoized so a pan or zoom re-renders nothing inside it. */
const DiagramContent = memo(function DiagramContent({
  layout,
  selected,
  relationLabel,
}: {
  layout: DiagramLayout;
  selected: number | null;
  relationLabel: (edge: DiagramEdgeItem) => string;
}) {
  const related = useMemo(() => {
    if (selected === null) return null;
    const near = new Set<number>([selected]);
    for (const edge of layout.edges) {
      if (edge.from === selected) near.add(edge.to);
      if (edge.to === selected) near.add(edge.from);
    }
    return near;
  }, [layout, selected]);
  return (
    <>
      <g className="er-edges">
        {layout.edges.map((edge, i) => {
          const [start, , , , , end] = edge.points;
          const lit = selected !== null && (edge.from === selected || edge.to === selected);
          const dim = selected !== null && !lit;
          return (
            <g key={i} className={`er-edge${lit ? " lit" : ""}${dim ? " dim" : ""}`}>
              <title>{relationLabel(edge)}</title>
              <path className="er-line" d={edgePath(edge)} />
              {start && endMarks(start.x, start.y, edge.fromSide, edge.fromCardinality)}
              {end && endMarks(end.x, end.y, edge.toSide, edge.toCardinality)}
            </g>
          );
        })}
      </g>
      <g className="er-nodes">
        {layout.nodes.map((node, i) => (
          <DiagramBox key={i} index={i} node={node} lineHeight={layout.lineHeight} selected={selected === i} dim={related !== null && !related.has(i)} />
        ))}
      </g>
    </>
  );
});

function DiagramBox({ index, node, lineHeight, selected, dim }: { index: number; node: DiagramNode; lineHeight: number; selected: boolean; dim: boolean }) {
  const titleY = 6 + lineHeight * 0.72;
  const header = node.rows.length > 0 ? node.rowsTop : node.height;
  const baseline = lineHeight * 0.72;
  const tooltip = [`${node.schema}.${node.name}`, node.fullComment].filter(Boolean).join("\n");
  return (
    <g
      className={`er-node${node.focus ? " focus" : ""}${selected ? " selected" : ""}${dim ? " dim" : ""}`}
      transform={`translate(${node.x} ${node.y})`}
      data-node={index}
    >
      <rect className="er-box" width={node.width} height={node.height} rx={6} />
      {node.rows.length > 0 && <path className="er-divider" d={`M0 ${header}H${node.width}`} />}
      <g className="er-heading">
        <title>{tooltip}</title>
        <text className="er-title" x={10} y={titleY}>{node.title}</text>
      </g>
      {node.comment && (
        <text className="er-comment" x={10} y={titleY + lineHeight}>{node.comment}</text>
      )}
      {node.rows.map((row, r) => {
        const y = node.rowsTop + r * lineHeight + baseline;
        const tip = `${row.fullName} ${row.fullType}${row.nullable ? "" : " NOT NULL"}${row.fullComment ? `\n${row.fullComment}` : ""}`;
        return (
          <g key={r} className="er-row">
            <title>{tip}</title>
            {row.key && <text className={`er-key${row.primary ? " primary" : ""}`} x={node.keyX} y={y}>{row.key}</text>}
            <text className="er-name" x={node.nameX} y={y}>{row.name}</text>
            {row.nativeType && <text className="er-type" x={node.typeX} y={y}>{row.nativeType}</text>}
            {row.comment && <text className="er-comment" x={node.commentX} y={y}>{row.comment}</text>}
          </g>
        );
      })}
    </g>
  );
}

/** The diagram as a standalone SVG document: the theme's colors and fonts written on the
 *  elements (a file has no stylesheet), the whole layout at its own size, on its background. */
function standaloneSvg(content: SVGGElement, layout: DiagramLayout, background: string): string {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("xmlns", "http://www.w3.org/2000/svg");
  svg.setAttribute("width", String(layout.width));
  svg.setAttribute("height", String(layout.height));
  svg.setAttribute("viewBox", `0 0 ${layout.width} ${layout.height}`);
  const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
  rect.setAttribute("width", "100%");
  rect.setAttribute("height", "100%");
  rect.setAttribute("style", `fill:${background}`);
  svg.appendChild(rect);
  const clone = content.cloneNode(true) as SVGGElement;
  clone.removeAttribute("transform");
  const properties = ["fill", "stroke", "stroke-width", "stroke-dasharray", "opacity", "font-family", "font-size", "font-weight"];
  const source = content.querySelectorAll<SVGElement>("*");
  const target = clone.querySelectorAll<SVGElement>("*");
  source.forEach((element, i) => {
    const style = getComputedStyle(element);
    target[i]?.setAttribute("style", properties.map((p) => `${p}:${style.getPropertyValue(p)}`).join(";"));
    target[i]?.removeAttribute("class");
  });
  // Selection and dimming are the screen's, not the file's.
  clone.querySelectorAll("[style]").forEach((element) => {
    const style = element.getAttribute("style") ?? "";
    element.setAttribute("style", style.replace(/opacity:[^;]*/, "opacity:1"));
  });
  svg.appendChild(clone);
  return new XMLSerializer().serializeToString(svg);
}

async function svgToPng(text: string, width: number, height: number): Promise<string> {
  const image = new Image();
  image.src = `data:image/svg+xml;charset=utf-8,${encodeURIComponent(text)}`;
  await image.decode();
  const scale = Math.min(2, Math.sqrt(MAX_PNG_PIXELS / (width * height)), MAX_PNG_SIDE / Math.max(width, height));
  const canvas = document.createElement("canvas");
  canvas.width = Math.max(1, Math.floor(width * scale));
  canvas.height = Math.max(1, Math.floor(height * scale));
  const context = canvas.getContext("2d");
  if (!context) throw new Error("no canvas");
  context.scale(scale, scale);
  context.drawImage(image, 0, 0, width, height);
  return canvas.toDataURL("image/png");
}

/** A tab's diagram: the catalog read on its own session, then laid out by the core. */
export function ErDiagramView({ tab }: { tab: Tab }) {
  const { t, i18n } = useTranslation("shell");
  const scope = tab.diagram!;
  const diagram = useRef(0);
  const [load, setLoad] = useState<Load>({ kind: "idle" });
  const [revision, setRevision] = useState(0);
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const [options, setOptions] = useState<Options>({ columns: "all", types: true, comments: false });
  const [layout, setLayout] = useState<DiagramLayout | null>(null);
  const [layoutError, setLayoutError] = useState<string | null>(null);
  const layouts = useRef(0);
  const [view, setView] = useState<View>({ x: 0, y: 0, k: 1 });
  const [selected, setSelected] = useState<number | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; node: number } | null>(null);
  const [exportMenu, setExportMenu] = useState(false);
  const [note, setNote] = useState<{ error: boolean; text: string } | null>(null);
  const host = useRef<HTMLDivElement>(null);
  const content = useRef<SVGGElement>(null);
  const drag = useRef<{ x: number; y: number; view: View; moved: boolean; node: number | null } | null>(null);
  const fitOnLayout = useRef(true);

  // The catalog, read once per tab (and on Refresh); nothing is read again after a reconnect
  // until the user asks.
  useEffect(() => {
    if (tab.status !== "connected" || needsRefresh) return;
    let current = true;
    const id = nextDiagram++;
    const previous = diagram.current;
    diagram.current = id;
    if (previous) void call(commands.closeDiagram(previous)).catch(() => undefined);
    setLoad({ kind: "reading", progress: null });
    setLayout(null);
    setSelected(null);
    const channel = new Channel<DiagramProgressItem>();
    channel.onmessage = (progress) => {
      if (current) setLoad({ kind: "reading", progress });
    };
    call(commands.readDiagram(tab.id, id, scope, channel))
      .then((summary) => {
        if (current) {
          fitOnLayout.current = true;
          setLoad({ kind: "done", summary });
        }
      })
      .catch((e: unknown) => {
        if (current) setLoad({ kind: "error", message: reason(e) });
      });
    return () => {
      current = false;
    };
  }, [tab.id, tab.status, scope, revision, needsRefresh]);

  useEffect(() => () => {
    if (diagram.current) void call(commands.closeDiagram(diagram.current)).catch(() => undefined);
  }, []);

  // A layout per model and display options: pan and zoom never come here.
  useEffect(() => {
    if (load.kind !== "done" || !host.current) return;
    let current = true;
    setLayoutError(null);
    call(commands.diagramLayout(diagram.current, options, measureFont(host.current)))
      .then((next) => {
        if (!current) return;
        layouts.current += 1;
        setLayout(next);
      })
      .catch((e: unknown) => {
        if (current) setLayoutError(reason(e));
      });
    return () => {
      current = false;
    };
  }, [load, options]);

  const fit = useCallback(() => {
    const element = host.current;
    if (!layout || !element) return;
    setView(fitted(layout, element.clientWidth, element.clientHeight));
  }, [layout]);

  useLayoutEffect(() => {
    if (layout && fitOnLayout.current) {
      fitOnLayout.current = false;
      fit();
    }
  }, [layout, fit]);

  const zoomAt = useCallback((factor: number, cx?: number, cy?: number) => {
    const element = host.current;
    if (!element) return;
    const x = cx ?? element.clientWidth / 2;
    const y = cy ?? element.clientHeight / 2;
    setView((v) => {
      const k = Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, v.k * factor));
      return { k, x: x - ((x - v.x) * k) / v.k, y: y - ((y - v.y) * k) / v.k };
    });
  }, []);

  // A wheel listener that may prevent scrolling (React's is passive).
  useEffect(() => {
    const element = host.current;
    if (!element) return;
    const wheel = (e: WheelEvent) => {
      e.preventDefault();
      const box = element.getBoundingClientRect();
      zoomAt(Math.exp(-e.deltaY * 0.0015), e.clientX - box.left, e.clientY - box.top);
    };
    element.addEventListener("wheel", wheel, { passive: false });
    return () => element.removeEventListener("wheel", wheel);
  }, [zoomAt]);

  useEffect(() => {
    if (!menu && !exportMenu) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && (e.target as Element | null)?.closest(".object-menu")) return;
      setMenu(null);
      setExportMenu(false);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [menu, exportMenu]);

  const relationLabel = useCallback(
    (edge: DiagramEdgeItem) => {
      const name = (i: number) => layout?.nodes[i]?.name ?? "?";
      return t("diagram.relation", {
        name: edge.name,
        from: `${name(edge.from)} (${edge.fromColumns.join(", ")})`,
        to: `${name(edge.to)} (${edge.toColumns.join(", ")})`,
      });
    },
    [layout, t],
  );

  const nodeAt = (target: EventTarget | null): number | null => {
    const element = (target as Element | null)?.closest("[data-node]");
    const index = element ? Number(element.getAttribute("data-node")) : NaN;
    return Number.isInteger(index) ? index : null;
  };

  async function reconnect() {
    if (!tab.connectionId) return;
    setNeedsRefresh(true);
    useApp.getState().updateTab(tab.id, { status: "connecting" });
    try {
      const version = await openSession(tab.id, tab.connectionId);
      useApp.getState().updateTab(tab.id, { status: version === null ? "disconnected" : "connected", serverVersion: version, ...(version === null ? {} : { lost: undefined }) });
    } catch (e) {
      useApp.getState().updateTab(tab.id, { status: "disconnected" });
      setLoad({ kind: "error", message: reason(e) });
    }
  }

  function refresh() {
    setNeedsRefresh(false);
    setRevision((r) => r + 1);
  }

  async function exportAs(format: "png" | "svg" | "mermaid" | "dbml") {
    setExportMenu(false);
    setNote(null);
    const base = scope.kind === "schema" ? scope.schema : scope.kind === "focus" ? scope.table : "diagram";
    const extension = format === "mermaid" ? "mmd" : format;
    const names: Record<typeof format, string> = { png: "PNG", svg: "SVG", mermaid: "Mermaid", dbml: "DBML" };
    try {
      const path = await chooseSavePath({
        title: t("diagram.export"),
        defaultPath: `${base.replace(/[\\/:*?"<>|]/g, "_")}.${extension}`,
        filters: [{ name: names[format], extensions: [extension] }],
      });
      if (!path) return;
      if (format === "mermaid" || format === "dbml") {
        await call(commands.saveDiagramText(diagram.current, format, path));
      } else {
        if (!content.current || !layout || !host.current) return;
        const text = standaloneSvg(content.current, layout, getComputedStyle(host.current).backgroundColor);
        const data = format === "svg" ? text : await svgToPng(text, layout.width, layout.height);
        await call(commands.saveChartImage(path, format, data));
      }
      setNote({ error: false, text: t("diagram.saved", { name: path.split(/[\\/]/).pop() ?? path }) });
    } catch (e) {
      setNote({ error: true, text: t("diagram.saveFailed", { detail: reason(e) }) });
    }
  }

  function openFocus(node: DiagramNode) {
    if (!tab.connectionId) return;
    openDiagram(tab.connectionId, { kind: "focus", schema: node.schema, table: node.name }, tab.id);
  }

  function openData(node: DiagramNode) {
    if (!tab.connectionId) return;
    openTableData(tab.connectionId, tab.id, node.schema, node.name);
  }

  function openStructure(node: DiagramNode) {
    if (!tab.connectionId) return;
    const app = useApp.getState();
    const existing = app.tabs.some((other) => other.connectionId === tab.connectionId && other.tableDetails?.schema === node.schema && other.tableDetails.name === node.name && other.tableDetails.kind === "columns");
    const id = app.addTableDetailsTab(tab.connectionId, node.schema, node.name, "columns");
    if (!existing) connectTabFromExplorer(id, tab.id, explorerDatabase(tab.connectionId, node.schema));
  }

  const number = (n: number) => n.toLocaleString(i18n.language);
  const summary = load.kind === "done" ? load.summary : null;
  const title =
    scope.kind === "schema"
      ? t("diagram.titleSchema", { schema: scope.schema })
      : scope.kind === "focus"
        ? t("diagram.titleFocus", { table: scope.table })
        : t("diagram.titleTables", { count: scope.tables.length });
  const countText = (s: DiagramSummary) =>
    `${t("diagram.tableCount", { count: s.tables, formattedCount: number(s.tables) })}, ${t("diagram.relationCount", { count: s.relations, formattedCount: number(s.relations) })}`;
  const menuNode = menu ? layout?.nodes[menu.node] : undefined;
  const busy = tab.status !== "connected" || load.kind === "reading";

  return (
    <div className="er-view">
      <div className="er-toolbar" role="toolbar" aria-label={t("diagram.toolbar")}>
        <div className="segmented" role="radiogroup" aria-label={t("diagram.columns")}>
          {(["all", "keys", "none"] as const).map((columns) => (
            <button
              key={columns}
              type="button"
              role="radio"
              aria-checked={options.columns === columns}
              className={options.columns === columns ? "active" : ""}
              onClick={() => setOptions((o) => ({ ...o, columns }))}
            >
              {t(`diagram.show.${columns}`)}
            </button>
          ))}
        </div>
        <label className="check">
          <input type="checkbox" checked={options.types} disabled={options.columns === "none"} onChange={(e) => setOptions((o) => ({ ...o, types: e.target.checked }))} />
          {t("diagram.types")}
        </label>
        <label className="check">
          <input type="checkbox" checked={options.comments} onChange={(e) => setOptions((o) => ({ ...o, comments: e.target.checked }))} />
          {t("diagram.comments")}
        </label>
        <span className="er-toolbar-gap" />
        <button type="button" className="ghost icon-button" onClick={() => zoomAt(1 / 1.25)} disabled={!layout} title={`${t("diagram.zoomOut")} (-)`} aria-label={t("diagram.zoomOut")}>
          <Icon name="minus" />
        </button>
        <span className="er-zoom muted small" aria-live="polite">{Math.round(view.k * 100)}%</span>
        <button type="button" className="ghost icon-button" onClick={() => zoomAt(1.25)} disabled={!layout} title={`${t("diagram.zoomIn")} (+)`} aria-label={t("diagram.zoomIn")}>
          <Icon name="plus" />
        </button>
        <button type="button" className="ghost icon-button" onClick={fit} disabled={!layout} title={`${t("diagram.fit")} (0)`} aria-label={t("diagram.fit")}>
          <Icon name="scan" />
        </button>
        <button type="button" className="ghost icon-button" onClick={refresh} disabled={busy} title={t("diagram.refresh")} aria-label={t("diagram.refresh")}>
          <Icon name="refresh" />
        </button>
        <div className="er-export">
          <button type="button" onClick={() => setExportMenu((open) => !open)} disabled={!layout} aria-haspopup="menu" aria-expanded={exportMenu}>
            {t("diagram.export")}
          </button>
          {exportMenu && (
            <div className="grid-menu object-menu er-export-menu" role="menu">
              <button role="menuitem" onClick={() => void exportAs("png")}>{t("diagram.exportPng")}</button>
              <button role="menuitem" onClick={() => void exportAs("svg")}>{t("diagram.exportSvg")}</button>
              <hr />
              <button role="menuitem" onClick={() => void exportAs("mermaid")}>{t("diagram.exportMermaid")}</button>
              <button role="menuitem" onClick={() => void exportAs("dbml")}>{t("diagram.exportDbml")}</button>
            </div>
          )}
        </div>
      </div>
      <div className="er-status" role="status">
        {tab.status === "connecting" && <span className="muted">{t("diagram.connecting")}</span>}
        {load.kind === "reading" && (
          <span className="muted">
            {load.progress && load.progress.total > 0
              ? t("diagram.reading", { read: number(load.progress.read), total: number(load.progress.total) })
              : t("diagram.readingStart")}
          </span>
        )}
        {summary && (
          <span>
            {title}: {countText(summary)}
          </span>
        )}
        {summary && summary.leftOut > 0 && <span className="er-warning">{t("diagram.leftOut", { max: number(summary.maxTables), count: summary.leftOut, formattedCount: number(summary.leftOut) })}</span>}
        {summary && summary.outsideKeys > 0 && <span className="muted">{t("diagram.outsideKeys", { count: summary.outsideKeys, formattedCount: number(summary.outsideKeys) })}</span>}
        {note && <span className={note.error ? "error" : "muted"} role={note.error ? "alert" : undefined}>{note.text}</span>}
      </div>
      {summary && summary.unreadable > 0 && (
        <p className="banner" role="alert">{t("diagram.unreadable", { count: summary.unreadable, formattedCount: number(summary.unreadable), detail: summary.firstError ?? "" })}</p>
      )}
      {(load.kind === "error" || layoutError || (tab.status === "disconnected" && tab.lost)) && (
        <p className="banner error" role="alert">
          {load.kind === "error" ? t("diagram.readFailed", { detail: load.message }) : layoutError ? t("diagram.layoutFailed", { detail: layoutError }) : tab.lost}
        </p>
      )}
      {tab.status === "disconnected" && (
        <div className="er-actions">
          <button type="button" onClick={() => void reconnect()} disabled={!tab.connectionId}>{t("tableDetails.reconnect")}</button>
        </div>
      )}
      {tab.status === "connected" && needsRefresh && (
        <div className="er-actions">
          <span className="muted">{t("diagram.refreshAfterReconnect")}</span>
          <button type="button" onClick={refresh}>{t("diagram.refresh")}</button>
        </div>
      )}
      <div
        ref={host}
        className={`er-canvas${drag.current?.moved ? " panning" : ""}`}
        tabIndex={0}
        role="img"
        aria-label={summary ? `${title}: ${countText(summary)}` : title}
        data-layouts={layouts.current}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          drag.current = { x: e.clientX, y: e.clientY, view, moved: false, node: nodeAt(e.target) };
          e.currentTarget.setPointerCapture(e.pointerId);
        }}
        onPointerMove={(e) => {
          const start = drag.current;
          if (!start) return;
          const dx = e.clientX - start.x;
          const dy = e.clientY - start.y;
          if (!start.moved && Math.hypot(dx, dy) < 3) return;
          start.moved = true;
          setView({ ...start.view, x: start.view.x + dx, y: start.view.y + dy });
        }}
        onPointerUp={() => {
          const start = drag.current;
          drag.current = null;
          if (start && !start.moved) setSelected(start.node);
        }}
        onContextMenu={(e) => {
          const node = nodeAt(e.target);
          if (node === null) return;
          e.preventDefault();
          setSelected(node);
          setMenu({ x: e.clientX, y: e.clientY, node });
        }}
        onKeyDown={(e) => {
          if (!layout || e.ctrlKey || e.metaKey || e.altKey) return;
          const step = e.shiftKey ? PAN_STEP * 4 : PAN_STEP;
          const pan = { ArrowLeft: [step, 0], ArrowRight: [-step, 0], ArrowUp: [0, step], ArrowDown: [0, -step] }[e.key];
          if (pan) {
            e.preventDefault();
            const [dx = 0, dy = 0] = pan;
            setView((v) => ({ ...v, x: v.x + dx, y: v.y + dy }));
          } else if (ZOOM_IN_KEYS.includes(e.key)) {
            e.preventDefault();
            zoomAt(1.25);
          } else if (ZOOM_OUT_KEYS.includes(e.key)) {
            e.preventDefault();
            zoomAt(1 / 1.25);
          } else if (e.key === FIT_KEY) {
            e.preventDefault();
            fit();
          } else if (e.key === ACTUAL_SIZE_KEY) {
            e.preventDefault();
            zoomAt(1 / view.k);
          } else if (e.key === "Escape") {
            setSelected(null);
          }
        }}
      >
        {layout && (
          <svg className="er-svg">
            <g ref={content} transform={`translate(${view.x} ${view.y}) scale(${view.k})`}>
              <DiagramContent layout={layout} selected={selected} relationLabel={relationLabel} />
            </g>
          </svg>
        )}
        {summary && summary.tables === 0 && (
          <div className="empty">{scope.kind === "schema" ? t("diagram.emptySchema") : t("diagram.empty")}</div>
        )}
      </div>
      {menu && menuNode && (
        // A table near the bottom or the right edge opens it inside the window too (SDB-255).
        <ObjectMenu at={menu}>
          <button role="menuitem" onClick={() => { setMenu(null); openFocus(menuNode); }}>{t("diagram.focus")}</button>
          <button role="menuitem" onClick={() => { setMenu(null); openData(menuNode); }}>{t("diagram.openData")}</button>
          <button role="menuitem" onClick={() => { setMenu(null); openStructure(menuNode); }}>{t("diagram.openStructure")}</button>
        </ObjectMenu>
      )}
    </div>
  );
}
