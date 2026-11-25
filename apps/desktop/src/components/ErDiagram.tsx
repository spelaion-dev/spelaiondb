// An ER diagram tab (ADR-046). The core reads the tables from the catalog on this tab's own
// session, lays them out and writes the Mermaid and DBML exports; this component only draws the
// layout it gets, as plain SVG (like charts, ADR-040). Panning and zooming change one transform
// and never ask for the layout again: a new layout comes only with new display options, a
// refresh or a change to the arrangement (`data-layouts` counts them, for the smoke test).
//
// A diagram can be saved and opened again (ADR-084): the page holds the arrangement it is
// editing (positions, notes, groups) and sends it with each layout; the core checks it, places
// the tables, wraps the notes and frames the groups, and keeps it in the local store when it is
// saved. Dragging a table, a note or a group's frame moves it here until the pointer is
// released, then the core lays the diagram out again with the new positions.

import { Channel } from "@tauri-apps/api/core";
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type {
  CardinalityItem,
  DiagramArrangementItem,
  DiagramColorItem,
  DiagramColumnsItem,
  DiagramEdgeItem,
  DiagramLayoutItem_Serialize as DiagramLayout,
  DiagramNodeItem_Serialize as DiagramNode,
  DiagramPlacementItem,
  DiagramProgressItem,
  DiagramSideItem,
  DiagramSummaryItem_Serialize as DiagramSummary,
  TableNameItem,
} from "../bindings";
import { connectTabFromExplorer, explorerDatabase, openTabSession, openTableData } from "../connect";
import { chooseSavePath } from "../dialogs";
import { SAVE_DIAGRAM_EVENT, openDiagram } from "../diagramTabs";
import { NOTE_WIDTH, placeNewNote } from "../erNotes";
import { useApp, useDialogs, type Tab } from "../store";
import { connectionKey } from "../connectionKey";
import { CONNECTING, DISCONNECTED, connectedWith, lostSession, statusName } from "../tabState";
import { ConfirmDelete } from "./ConfirmDelete";
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
/** The longest note and name the core keeps (`er/saved.rs`). */
const MAX_NOTE_CHARS = 2000;
const MAX_NAME_CHARS = 100;
const COLORS: DiagramColorItem[] = ["gray", "blue", "green", "amber", "red", "purple"];
const NO_ARRANGEMENT: DiagramArrangementItem = { positions: [], notes: [], groups: [] };
/** The palette colors a new note and a new group start with. */
const NEW_NOTE_COLOR: DiagramColorItem = "amber";
const NEW_GROUP_COLOR: DiagramColorItem = "blue";

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

/** The saved diagram this tab opened, read back from the core before the first layout. */
type SavedLoad =
  | { kind: "none" }
  | { kind: "loading" }
  | { kind: "ready" }
  | { kind: "error"; message: string };

/** What a pointer is on: a table, a gone table, a note or a group's frame (by their place in
 *  the layout's lists). */
type Target =
  | { kind: "node"; index: number }
  | { kind: "gone"; index: number }
  | { kind: "note"; index: number }
  | { kind: "group"; index: number };

/** What a drag moves. */
interface Items {
  nodes: number[];
  gone: number[];
  notes: number[];
  groups: number[];
}

/** A drag under way: `dx`, `dy` in the layout's pixels. */
interface Moving extends Items {
  dx: number;
  dy: number;
}

type Editor =
  | { kind: "note"; index: number | null; text: string; color: DiagramColorItem; x: number; y: number }
  | { kind: "group"; index: number | null; name: string; color: DiagramColorItem; tables: TableNameItem[] };

function reason(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function tableKey(schema: string, name: string): string {
  return `${schema}\u0000${name}`;
}

/** The width of one character and the row height of the diagram's monospace font. */
function measureFont(host: Element): { charWidth: number; lineHeight: number } {
  const style = getComputedStyle(host);
  // The stylesheet always defines the token: no size of its own here.
  const size = style.getPropertyValue("--text-sm").trim();
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
  return { k, x: (width - layout.width * k) / 2 - layout.x * k, y: Math.max(0, (height - layout.height * k) / 2) - layout.y * k };
}

function edgePath(edge: DiagramEdgeItem): string {
  const [p0, p1, p2, p3, p4, p5] = edge.points;
  if (!p0 || !p1 || !p2 || !p3 || !p4 || !p5) return "";
  return `M${p0.x} ${p0.y}L${p1.x} ${p1.y}C${p2.x} ${p2.y} ${p3.x} ${p3.y} ${p4.x} ${p4.y}L${p5.x} ${p5.y}`;
}

/** An edge with the ends of moving boxes moved along, while a drag is under way. */
function movedEdge(edge: DiagramEdgeItem, moving: Moving | null): DiagramEdgeItem {
  if (!moving) return edge;
  const from = moving.nodes.includes(edge.from);
  const to = moving.nodes.includes(edge.to);
  if (!from && !to) return edge;
  return {
    ...edge,
    points: edge.points.map((p, i) => ((i < 3 ? from : to) ? { x: p.x + moving.dx, y: p.y + moving.dy } : p)),
  };
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
  selection,
  newTables,
  moving,
  relationLabel,
  goneLabel,
  goneTip,
  newLabel,
  newTip,
}: {
  layout: DiagramLayout;
  selection: number[];
  newTables: ReadonlySet<string>;
  moving: Moving | null;
  relationLabel: (edge: DiagramEdgeItem) => string;
  goneLabel: string;
  goneTip: (name: string) => string;
  newLabel: string;
  newTip: string;
}) {
  const related = useMemo(() => {
    if (selection.length === 0) return null;
    const near = new Set<number>(selection);
    for (const edge of layout.edges) {
      if (selection.includes(edge.from)) near.add(edge.to);
      if (selection.includes(edge.to)) near.add(edge.from);
    }
    return near;
  }, [layout, selection]);
  const shift = (moved: boolean) => (moved && moving ? { dx: moving.dx, dy: moving.dy } : { dx: 0, dy: 0 });
  return (
    <>
      <g className="er-groups">
        {layout.groups.map((frame, i) => {
          const { dx, dy } = shift(moving?.groups.includes(i) ?? false);
          return (
            <g key={i} className={`er-group-frame er-color-${frame.color}`} data-group={i} transform={`translate(${frame.x + dx} ${frame.y + dy})`}>
              <rect width={frame.width} height={frame.height} rx={10} />
              <text x={12} y={6 + layout.lineHeight * 0.72}>{frame.label}</text>
            </g>
          );
        })}
      </g>
      <g className="er-edges">
        {layout.edges.map((original, i) => {
          const edge = movedEdge(original, moving);
          const [start, , , , , end] = edge.points;
          const lit = related !== null && (selection.includes(edge.from) || selection.includes(edge.to));
          const dim = related !== null && !lit;
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
        {layout.gone.map((gone, i) => {
          const { dx, dy } = shift(moving?.gone.includes(i) ?? false);
          return (
            <g key={`gone-${i}`} className="er-gone" data-gone={i} transform={`translate(${gone.x + dx} ${gone.y + dy})`}>
              <title>{goneTip(`${gone.schema}.${gone.name}`)}</title>
              <text className="er-mark" x={0} y={-5}>{goneLabel}</text>
              <rect className="er-box" width={gone.width} height={gone.height} rx={6} />
              <text className="er-title" x={10} y={6 + layout.lineHeight * 0.72}>{gone.title}</text>
            </g>
          );
        })}
        {layout.nodes.map((node, i) => (
          <DiagramBox
            key={i}
            index={i}
            node={node}
            lineHeight={layout.lineHeight}
            selected={selection.includes(i)}
            dim={related !== null && !related.has(i)}
            isNew={node.new || newTables.has(tableKey(node.schema, node.name))}
            offset={shift(moving?.nodes.includes(i) ?? false)}
            newLabel={newLabel}
            newTip={newTip}
          />
        ))}
      </g>
      <g className="er-notes">
        {layout.notes.map((note, i) => {
          const { dx, dy } = shift(moving?.notes.includes(i) ?? false);
          return (
            <g key={i} className={`er-note er-color-${note.color}`} data-note={i} transform={`translate(${note.x + dx} ${note.y + dy})`}>
              <rect width={note.width} height={note.height} rx={4} />
              {note.lines.map((line, l) => (
                <text key={l} x={10} y={6 + layout.lineHeight * (l + 0.72)} xmlSpace="preserve">{line}</text>
              ))}
            </g>
          );
        })}
      </g>
    </>
  );
});

function DiagramBox({ index, node, lineHeight, selected, dim, isNew, offset, newLabel, newTip }: {
  index: number;
  node: DiagramNode;
  lineHeight: number;
  selected: boolean;
  dim: boolean;
  isNew: boolean;
  offset: { dx: number; dy: number };
  newLabel: string;
  newTip: string;
}) {
  const titleY = 6 + lineHeight * 0.72;
  const header = node.rows.length > 0 ? node.rowsTop : node.height;
  const baseline = lineHeight * 0.72;
  const tooltip = [`${node.schema}.${node.name}`, node.fullComment].filter(Boolean).join("\n");
  return (
    <g
      className={`er-node${node.focus ? " focus" : ""}${selected ? " selected" : ""}${dim ? " dim" : ""}${isNew ? " new" : ""}`}
      transform={`translate(${node.x + offset.dx} ${node.y + offset.dy})`}
      data-node={index}
    >
      {isNew && (
        // Marked on screen until the diagram is saved; never in an exported image.
        <text className="er-mark new" x={0} y={-5} data-screen-only="">
          <title>{newTip}</title>
          {newLabel}
        </text>
      )}
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
  svg.setAttribute("viewBox", `${layout.x} ${layout.y} ${layout.width} ${layout.height}`);
  const rect = document.createElementNS("http://www.w3.org/2000/svg", "rect");
  rect.setAttribute("x", String(layout.x));
  rect.setAttribute("y", String(layout.y));
  rect.setAttribute("width", String(layout.width));
  rect.setAttribute("height", String(layout.height));
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
  // Selection and dimming are the screen's, not the file's; so are the marks of new tables.
  clone.querySelectorAll("[style]").forEach((element) => {
    const style = element.getAttribute("style") ?? "";
    element.setAttribute("style", style.replace(/opacity:[^;]*/, "opacity:1"));
  });
  clone.querySelectorAll("[data-screen-only]").forEach((element) => element.remove());
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

/** Every table and gone table where the layout draws it: the positions a first move fixes. */
function drawnPositions(layout: DiagramLayout): DiagramPlacementItem[] {
  return [
    ...layout.nodes.map((n) => ({ schema: n.schema, name: n.name, x: n.x, y: n.y })),
    ...layout.gone.map((g) => ({ schema: g.schema, name: g.name, x: g.x, y: g.y })),
  ];
}

/** The note and group form: a note's text or a group's name, and a color from the palette. */
function DiagramEditorDialog({ editor, onCancel, onDone }: { editor: Editor; onCancel(): void; onDone(editor: Editor): void }) {
  const { t } = useTranslation("shell");
  const [value, setValue] = useState(editor.kind === "note" ? editor.text : editor.name);
  const [color, setColor] = useState<DiagramColorItem>(editor.color);
  const field = useRef<HTMLTextAreaElement & HTMLInputElement>(null);
  useEffect(() => field.current?.focus(), []);
  const done = () => {
    if (!value.trim()) return;
    onDone(editor.kind === "note" ? { ...editor, text: value, color } : { ...editor, name: value.trim(), color });
  };
  const title = editor.kind === "note" ? t("diagram.noteTitle") : t("diagram.groupTitle");
  return (
    <div className="modal-backdrop">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            onCancel();
          }
        }}
      >
        <h2>{title}</h2>
        <label className="field">
          <span>{editor.kind === "note" ? t("diagram.noteText") : t("diagram.groupName")}</span>
          {editor.kind === "note" ? (
            <textarea
              ref={field}
              className="er-note-text"
              value={value}
              maxLength={MAX_NOTE_CHARS}
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) done();
              }}
            />
          ) : (
            <input
              ref={field}
              value={value}
              maxLength={MAX_NAME_CHARS}
              onChange={(e) => setValue(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") done();
              }}
            />
          )}
        </label>
        <div className="field">
          <span id="er-color-label">{t("diagram.color")}</span>
          <div className="er-swatches" role="radiogroup" aria-labelledby="er-color-label">
            {COLORS.map((c) => (
              <button
                key={c}
                type="button"
                role="radio"
                aria-checked={color === c}
                className={`er-swatch er-color-${c}`}
                onClick={() => setColor(c)}
              >
                {t(`diagram.colors.${c}`)}
              </button>
            ))}
          </div>
        </div>
        <div className="modal-actions">
          <button type="button" onClick={onCancel}>{t("diagram.cancel")}</button>
          <button type="button" className="primary" disabled={!value.trim()} onClick={done}>
            {editor.kind === "note" ? t("diagram.noteConfirm") : t("diagram.groupConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** A tab's diagram: the catalog read on its own session, then laid out by the core. */
export function ErDiagramView({ tab }: { tab: Tab }) {
  const status = statusName(tab);
  const lost = lostSession(tab);
  const { t, i18n } = useTranslation("shell");
  const scope = tab.diagram!;
  const savedDiagram = tab.savedDiagram;
  const diagram = useRef(0);
  const [load, setLoad] = useState<Load>({ kind: "idle" });
  const [revision, setRevision] = useState(0);
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const [options, setOptions] = useState<Options>({ columns: "all", types: true, comments: false });
  const [arrangement, setArrangement] = useState<DiagramArrangementItem>(NO_ARRANGEMENT);
  // The saved diagram's options and arrangement are read once, when the tab opens on it.
  const [savedLoad, setSavedLoad] = useState<SavedLoad>(() => (savedDiagram ? { kind: "loading" } : { kind: "none" }));
  const savedId = useRef(savedDiagram?.id ?? null);
  // Tables placed by the core below the saved ones, marked until the diagram is saved.
  const [newTables, setNewTables] = useState<ReadonlySet<string>>(new Set());
  const [layout, setLayout] = useState<DiagramLayout | null>(null);
  const [layoutError, setLayoutError] = useState<string | null>(null);
  const layouts = useRef(0);
  const [view, setView] = useState<View>({ x: 0, y: 0, k: 1 });
  const [selection, setSelection] = useState<number[]>([]);
  const [moving, setMoving] = useState<Moving | null>(null);
  const [menu, setMenu] = useState<{ x: number; y: number; target: Target } | null>(null);
  const [exportMenu, setExportMenu] = useState(false);
  const [moreMenu, setMoreMenu] = useState(false);
  const [editor, setEditor] = useState<Editor | null>(null);
  const [deleting, setDeleting] = useState(false);
  const [note, setNote] = useState<{ error: boolean; text: string } | null>(null);
  const host = useRef<HTMLDivElement>(null);
  const content = useRef<SVGGElement>(null);
  const drag = useRef<{ x: number; y: number; view: View; moved: boolean; target: Target | null; items: Items | null; additive: boolean } | null>(null);
  const fitOnLayout = useRef(true);
  const modified = tab.diagramModified === true;

  const markModified = useCallback(() => {
    if (!useApp.getState().tabs.find((t) => t.id === tab.id)?.diagramModified) {
      useApp.getState().updateTab(tab.id, { diagramModified: true });
    }
  }, [tab.id]);

  // A saved diagram's options and arrangement, checked by the core, before the first layout.
  useEffect(() => {
    if (savedId.current === null) return;
    let current = true;
    call(commands.openSavedDiagram(savedId.current))
      .then((detail) => {
        if (!current) return;
        setOptions(detail.options);
        setArrangement(detail.arrangement);
        setSavedLoad({ kind: "ready" });
      })
      .catch((e: unknown) => {
        if (current) setSavedLoad({ kind: "error", message: reason(e) });
      });
    return () => {
      current = false;
    };
  }, []);

  // The catalog, read once per tab (and on Refresh); nothing is read again after a reconnect
  // until the user asks.
  useEffect(() => {
    if (status !== "connected" || needsRefresh) return;
    let current = true;
    const id = nextDiagram++;
    const previous = diagram.current;
    diagram.current = id;
    if (previous) void call(commands.closeDiagram(previous)).catch(() => undefined);
    setLoad({ kind: "reading", progress: null });
    setLayout(null);
    setSelection([]);
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
  }, [tab.id, status, scope, revision, needsRefresh]);

  useEffect(() => () => {
    if (diagram.current) void call(commands.closeDiagram(diagram.current)).catch(() => undefined);
  }, []);

  // A layout per model, display options and arrangement: pan and zoom never come here.
  const savedReady = savedLoad.kind === "none" || savedLoad.kind === "ready";
  // The last arrangement the core laid out: one it refuses (a note it does not keep) is undone.
  const accepted = useRef<DiagramArrangementItem | null>(null);
  useEffect(() => {
    if (load.kind !== "done" || !savedReady || !host.current) return;
    let current = true;
    setLayoutError(null);
    call(commands.diagramLayout(diagram.current, options, measureFont(host.current), arrangement))
      .then((next) => {
        if (!current) return;
        accepted.current = arrangement;
        layouts.current += 1;
        setMoving(null);
        setLayout(next);
        const fresh = next.nodes.filter((n) => n.new);
        if (fresh.length > 0) {
          setNewTables((known) => {
            const all = new Set(known);
            for (const n of fresh) all.add(tableKey(n.schema, n.name));
            return all.size === known.size ? known : all;
          });
        }
      })
      .catch((e: unknown) => {
        if (!current) return;
        setMoving(null);
        const previous = accepted.current;
        if (previous && previous !== arrangement) {
          // Said in the status line: the banner clears with the layout of the one restored.
          setNote({ error: true, text: t("diagram.layoutFailed", { detail: reason(e) }) });
          setArrangement(previous);
        } else {
          setLayoutError(reason(e));
        }
      });
    return () => {
      current = false;
    };
  }, [load, options, arrangement, savedReady, t]);

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
    if (!menu && !exportMenu && !moreMenu) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && (e.target as Element | null)?.closest(".object-menu")) return;
      setMenu(null);
      setExportMenu(false);
      setMoreMenu(false);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [menu, exportMenu, moreMenu]);

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
  const goneTip = useCallback((name: string) => t("diagram.goneTip", { name }), [t]);

  const targetAt = (target: EventTarget | null): Target | null => {
    const element = (target as Element | null)?.closest("[data-node],[data-gone],[data-note],[data-group]");
    if (!element) return null;
    for (const kind of ["node", "gone", "note", "group"] as const) {
      const value = element.getAttribute(`data-${kind}`);
      if (value === null) continue;
      const index = Number(value);
      return Number.isInteger(index) ? { kind, index } : null;
    }
    return null;
  };

  /** A group's tables and gone tables, by their place in the layout. */
  const groupMembers = (groupIndex: number): { nodes: number[]; gone: number[] } => {
    const group = arrangement.groups[groupIndex];
    if (!layout || !group) return { nodes: [], gone: [] };
    const wanted = new Set(group.tables.map((tbl) => tableKey(tbl.schema, tbl.name)));
    return {
      nodes: layout.nodes.flatMap((n, i) => (wanted.has(tableKey(n.schema, n.name)) ? [i] : [])),
      gone: layout.gone.flatMap((g, i) => (wanted.has(tableKey(g.schema, g.name)) ? [i] : [])),
    };
  };

  /** What dragging `target` moves: a selected table moves the whole selection. */
  const itemsFor = (target: Target): Items => {
    const none: Items = { nodes: [], gone: [], notes: [], groups: [] };
    switch (target.kind) {
      case "node":
        return { ...none, nodes: selection.includes(target.index) ? selection : [target.index] };
      case "gone":
        return { ...none, gone: [target.index] };
      case "note":
        return { ...none, notes: [target.index] };
      case "group": {
        const frame = layout?.groups[target.index];
        if (!frame) return none;
        return { ...none, ...groupMembers(frame.group), groups: [target.index] };
      }
      default: {
        const unknown: never = target;
        return unknown;
      }
    }
  };

  /** Moves `items` by `dx`, `dy` in the arrangement: the first move fixes every table where it is. */
  function commitMove(items: Items, dx: number, dy: number) {
    if (!layout) return;
    const moveBy = (v: number | null, d: number) => Math.round((v ?? 0) + d);
    const movedTables = new Set([
      ...items.nodes.flatMap((i) => (layout.nodes[i] ? [tableKey(layout.nodes[i].schema, layout.nodes[i].name)] : [])),
      ...items.gone.flatMap((i) => (layout.gone[i] ? [tableKey(layout.gone[i].schema, layout.gone[i].name)] : [])),
    ]);
    const positions = drawnPositions(layout).map((p) =>
      movedTables.has(tableKey(p.schema, p.name)) ? { ...p, x: moveBy(p.x, dx), y: moveBy(p.y, dy) } : p,
    );
    const movedNotes = new Set(items.notes.map((i) => layout.notes[i]?.note).filter((n): n is number => n !== undefined));
    const notes = arrangement.notes.map((n, i) => (movedNotes.has(i) ? { ...n, x: moveBy(n.x, dx), y: moveBy(n.y, dy) } : n));
    setArrangement({ ...arrangement, positions, notes });
    markModified();
  }

  function changeArrangement(next: DiagramArrangementItem) {
    setArrangement(next);
    markModified();
  }

  async function reconnect() {
    if (!tab.connectionId) return;
    setNeedsRefresh(true);
    useApp.getState().setConnection(tab.id, CONNECTING);
    try {
      // On the database it was opened on (ADR-074, SDB-506), never quietly another.
      const session = await openTabSession(tab.id, connectionKey(tab.connectionId));
      useApp.getState().setConnection(tab.id, session === null ? DISCONNECTED : connectedWith(session));
    } catch (e) {
      useApp.getState().setConnection(tab.id, DISCONNECTED);
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
    const base = savedDiagram?.name ?? (scope.kind === "schema" ? scope.schema : scope.kind === "focus" ? scope.table : "diagram");
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

  /** Saves the diagram: over the saved one, or as a new one under a name asked for. */
  async function save(asNew = false) {
    setMoreMenu(false);
    if (!tab.connectionId) return;
    let name = savedDiagram?.name ?? null;
    if (!savedDiagram || asNew) {
      name = await useDialogs.getState().askText({
        title: asNew ? t("diagram.saveAsTitle") : t("diagram.saveTitle"),
        label: t("diagram.name"),
        initial: savedDiagram?.name ?? (scope.kind === "schema" ? scope.schema : scope.kind === "focus" ? scope.table : ""),
        confirm: t("diagram.saveConfirm"),
      });
      if (!name) return;
    }
    if (!name) return;
    setNote(null);
    // What is drawn is what is kept: once saved, every table stays where it is now.
    const positions = arrangement.positions.length > 0 || !layout ? arrangement.positions : drawnPositions(layout);
    const kept = { ...arrangement, positions };
    try {
      const saved = await call(commands.saveDiagram(
        connectionKey(tab.connectionId),
        asNew || !savedDiagram ? null : savedDiagram.id,
        name,
        scope,
        options,
        kept,
      ));
      savedId.current = saved.saved.id;
      useApp.getState().updateTab(tab.id, { savedDiagram: { id: saved.saved.id, name: saved.saved.name }, diagramModified: false });
      setArrangement(saved.arrangement);
      setNewTables(new Set());
      setNote({ error: false, text: t("diagram.savedDiagram", { name: saved.saved.name }) });
    } catch (e) {
      setNote({ error: true, text: t("diagram.saveDiagramFailed", { detail: reason(e) }) });
    }
  }

  async function rename() {
    setMoreMenu(false);
    if (!savedDiagram) return;
    const name = await useDialogs.getState().askText({
      title: t("diagram.renameTitle"),
      label: t("diagram.name"),
      initial: savedDiagram.name,
      confirm: t("diagram.renameConfirm"),
    });
    if (!name || name === savedDiagram.name) return;
    try {
      const renamed = await call(commands.renameSavedDiagram(savedDiagram.id, name));
      useApp.getState().updateTab(tab.id, { savedDiagram: { id: renamed.id, name: renamed.name } });
      setNote(null);
    } catch (e) {
      setNote({ error: true, text: t("diagram.saveDiagramFailed", { detail: reason(e) }) });
    }
  }

  // Ctrl+S, or the palette's Save, on this tab.
  const saveRef = useRef(save);
  saveRef.current = save;
  useEffect(() => {
    const onSave = (e: Event) => {
      if ((e as CustomEvent<string>).detail === tab.id) void saveRef.current();
    };
    window.addEventListener(SAVE_DIAGRAM_EVENT, onSave);
    return () => window.removeEventListener(SAVE_DIAGRAM_EVENT, onSave);
  }, [tab.id]);

  function layOutAgain() {
    setMoreMenu(false);
    if (arrangement.positions.length === 0) return;
    fitOnLayout.current = true;
    changeArrangement({ ...arrangement, positions: [] });
  }

  /** A new note in empty space next to the focused table, or near the middle of the screen. */
  function addNote() {
    const element = host.current;
    const middle = {
      x: element ? (element.clientWidth / 2 - view.x) / view.k : 0,
      y: element ? (element.clientHeight / 2 - view.y) / view.k : 0,
    };
    const { x, y } = layout ? placeNewNote(layout, middle) : { x: Math.round(middle.x - NOTE_WIDTH / 2), y: Math.round(middle.y) };
    setEditor({ kind: "note", index: null, text: "", color: NEW_NOTE_COLOR, x, y });
  }

  function editTarget(target: Target) {
    setMenu(null);
    if (!layout) return;
    if (target.kind === "note") {
      const box = layout.notes[target.index];
      const kept = box ? arrangement.notes[box.note] : undefined;
      if (box && kept) setEditor({ kind: "note", index: box.note, text: kept.text, color: kept.color, x: kept.x ?? box.x, y: kept.y ?? box.y });
    } else if (target.kind === "group") {
      const frame = layout.groups[target.index];
      const kept = frame ? arrangement.groups[frame.group] : undefined;
      if (frame && kept) setEditor({ kind: "group", index: frame.group, name: kept.name, color: kept.color, tables: kept.tables });
    }
  }

  /** A new group of the selected tables (or the one right-clicked): they leave their groups. */
  function groupSelection(node: number) {
    setMenu(null);
    if (!layout) return;
    const chosen = selection.includes(node) ? selection : [node];
    const tables = chosen.flatMap((i) => (layout.nodes[i] ? [{ schema: layout.nodes[i].schema, name: layout.nodes[i].name }] : []));
    setEditor({ kind: "group", index: null, name: "", color: NEW_GROUP_COLOR, tables });
  }

  function finishEditor(done: Editor) {
    setEditor(null);
    if (done.kind === "note") {
      const kept = { x: done.x, y: done.y, width: done.index === null ? NOTE_WIDTH : (arrangement.notes[done.index]?.width ?? NOTE_WIDTH), text: done.text, color: done.color };
      const notes = done.index === null ? [...arrangement.notes, kept] : arrangement.notes.map((n, i) => (i === done.index ? kept : n));
      changeArrangement({ ...arrangement, notes });
      return;
    }
    const members = new Set(done.tables.map((tbl) => tableKey(tbl.schema, tbl.name)));
    const group = { name: done.name, color: done.color, tables: done.tables };
    // A table is in one group at most: the new group takes its tables from the others.
    const others = arrangement.groups
      .map((g, i) => (i === done.index ? g : { ...g, tables: g.tables.filter((tbl) => !members.has(tableKey(tbl.schema, tbl.name))) }))
      .filter((g) => g.tables.length > 0);
    const groups = done.index === null ? [...others, group] : others.map((g) => (g === arrangement.groups[done.index!] ? group : g));
    changeArrangement({ ...arrangement, groups });
  }

  function deleteNote(target: Target) {
    setMenu(null);
    const box = target.kind === "note" ? layout?.notes[target.index] : undefined;
    if (!box) return;
    changeArrangement({ ...arrangement, notes: arrangement.notes.filter((_, i) => i !== box.note) });
  }

  function ungroup(target: Target) {
    setMenu(null);
    const frame = target.kind === "group" ? layout?.groups[target.index] : undefined;
    if (!frame) return;
    changeArrangement({ ...arrangement, groups: arrangement.groups.filter((_, i) => i !== frame.group) });
  }

  /** The group a table (or gone table) is in, by its place in the arrangement. */
  const groupOf = (schema: string, name: string): number =>
    arrangement.groups.findIndex((g) => g.tables.some((tbl) => tbl.schema === schema && tbl.name === name));

  function removeFromGroup(schema: string, name: string) {
    setMenu(null);
    const groups = arrangement.groups
      .map((g) => ({ ...g, tables: g.tables.filter((tbl) => !(tbl.schema === schema && tbl.name === name)) }))
      .filter((g) => g.tables.length > 0);
    changeArrangement({ ...arrangement, groups });
  }

  /** Takes a gone table out of the diagram: its position and its place in a group. */
  function removeGone(target: Target) {
    setMenu(null);
    const gone = target.kind === "gone" ? layout?.gone[target.index] : undefined;
    if (!gone) return;
    const same = (tbl: { schema: string; name: string }) => tbl.schema === gone.schema && tbl.name === gone.name;
    changeArrangement({
      ...arrangement,
      positions: arrangement.positions.filter((p) => !same(p)),
      groups: arrangement.groups.map((g) => ({ ...g, tables: g.tables.filter((tbl) => !same(tbl)) })).filter((g) => g.tables.length > 0),
    });
  }

  async function deleteSaved() {
    if (!savedDiagram) return;
    await call(commands.deleteSavedDiagram(savedDiagram.id));
    setDeleting(false);
    savedId.current = null;
    // The tab stays, as a diagram of the same scope and arrangement that is not saved; closing
    // it does not ask again about what was just deleted.
    useApp.getState().updateTab(tab.id, { savedDiagram: undefined, diagramModified: false });
    setNote({ error: false, text: t("diagram.deleted", { name: savedDiagram.name }) });
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
    if (!existing) connectTabFromExplorer(id, tab.id, explorerDatabase(connectionKey(tab.connectionId), node.schema));
  }

  const number = (n: number) => n.toLocaleString(i18n.language);
  const summary = load.kind === "done" ? load.summary : null;
  const title = savedDiagram
    ? savedDiagram.name
    : scope.kind === "schema"
      ? t("diagram.titleSchema", { schema: scope.schema })
      : scope.kind === "focus"
        ? t("diagram.titleFocus", { table: scope.table })
        : t("diagram.titleTables", { count: scope.tables.length });
  const countText = (s: DiagramSummary) =>
    `${t("diagram.tableCount", { count: s.tables, formattedCount: number(s.tables) })}, ${t("diagram.relationCount", { count: s.relations, formattedCount: number(s.relations) })}`;
  const busy = status !== "connected" || load.kind === "reading";
  const newCount = layout ? layout.nodes.filter((n) => n.new || newTables.has(tableKey(n.schema, n.name))).length : 0;
  const goneCount = layout?.gone.length ?? 0;

  function menuItems(target: Target): ReactNode {
    if (!layout) return null;
    switch (target.kind) {
      case "node": {
        const node = layout.nodes[target.index];
        if (!node) return null;
        const inGroup = groupOf(node.schema, node.name) >= 0;
        return (
          <>
            <button role="menuitem" onClick={() => { setMenu(null); openFocus(node); }}>{t("diagram.focus")}</button>
            <button role="menuitem" onClick={() => { setMenu(null); openData(node); }}>{t("diagram.openData")}</button>
            <button role="menuitem" onClick={() => { setMenu(null); openStructure(node); }}>{t("diagram.openStructure")}</button>
            <hr />
            <button role="menuitem" onClick={() => groupSelection(target.index)}>{t("diagram.group")}</button>
            {inGroup && <button role="menuitem" onClick={() => removeFromGroup(node.schema, node.name)}>{t("diagram.removeFromGroup")}</button>}
          </>
        );
      }
      case "gone": {
        const gone = layout.gone[target.index];
        if (!gone) return null;
        return (
          <>
            {groupOf(gone.schema, gone.name) >= 0 && <button role="menuitem" onClick={() => removeFromGroup(gone.schema, gone.name)}>{t("diagram.removeFromGroup")}</button>}
            <button role="menuitem" onClick={() => removeGone(target)}>{t("diagram.removeTable")}</button>
          </>
        );
      }
      case "note":
        return (
          <>
            <button role="menuitem" onClick={() => editTarget(target)}>{t("diagram.editNote")}</button>
            <button role="menuitem" className="danger" onClick={() => deleteNote(target)}>{t("diagram.deleteNote")}</button>
          </>
        );
      case "group":
        return (
          <>
            <button role="menuitem" onClick={() => editTarget(target)}>{t("diagram.editGroup")}</button>
            <button role="menuitem" onClick={() => ungroup(target)}>{t("diagram.ungroup")}</button>
          </>
        );
      default: {
        const unknown: never = target;
        return unknown;
      }
    }
  }

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
              onClick={() => { setOptions((o) => ({ ...o, columns })); if (savedDiagram) markModified(); }}
            >
              {t(`diagram.show.${columns}`)}
            </button>
          ))}
        </div>
        <label className="check">
          <input type="checkbox" checked={options.types} disabled={options.columns === "none"} onChange={(e) => { setOptions((o) => ({ ...o, types: e.target.checked })); if (savedDiagram) markModified(); }} />
          {t("diagram.types")}
        </label>
        <label className="check">
          <input type="checkbox" checked={options.comments} onChange={(e) => { setOptions((o) => ({ ...o, comments: e.target.checked })); if (savedDiagram) markModified(); }} />
          {t("diagram.comments")}
        </label>
        <button type="button" onClick={addNote} disabled={!layout}>{t("diagram.addNote")}</button>
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
        <button type="button" className="primary" onClick={() => void save()} disabled={!layout} title={t("diagram.save")}>
          {t("diagram.save")}
        </button>
        <div className="er-export">
          <button type="button" className="ghost icon-button" onClick={() => setMoreMenu((open) => !open)} disabled={!layout} aria-haspopup="menu" aria-expanded={moreMenu} title={t("diagram.more")} aria-label={t("diagram.more")}>
            <Icon name="chevronDown" />
          </button>
          {moreMenu && (
            <div className="grid-menu object-menu er-export-menu" role="menu">
              <button role="menuitem" onClick={() => void save(true)}>{t("diagram.saveAs")}</button>
              <button role="menuitem" onClick={() => void rename()} disabled={!savedDiagram}>{t("diagram.rename")}</button>
              <button role="menuitem" onClick={layOutAgain} disabled={arrangement.positions.length === 0}>{t("diagram.layOutAgain")}</button>
              <hr />
              <button role="menuitem" className="danger" onClick={() => { setMoreMenu(false); setDeleting(true); }} disabled={!savedDiagram}>{t("diagram.delete")}</button>
            </div>
          )}
        </div>
      </div>
      <div className="er-status" role="status">
        {status === "connecting" && <span className="muted">{t("diagram.connecting")}</span>}
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
        {modified && <span className="er-unsaved">{t("diagram.unsaved")}</span>}
        {summary && summary.leftOut > 0 && <span className="er-warning">{t("diagram.leftOut", { max: number(summary.maxTables), count: summary.leftOut, formattedCount: number(summary.leftOut) })}</span>}
        {summary && summary.outsideKeys > 0 && <span className="muted">{t("diagram.outsideKeys", { count: summary.outsideKeys, formattedCount: number(summary.outsideKeys) })}</span>}
        {newCount > 0 && <span className="muted">{t("diagram.newCount", { count: newCount, formattedCount: number(newCount) })}</span>}
        {goneCount > 0 && <span className="er-warning">{t("diagram.goneCount", { count: goneCount, formattedCount: number(goneCount) })}</span>}
        {note && <span className={note.error ? "error" : "muted"} role={note.error ? "alert" : undefined}>{note.text}</span>}
      </div>
      {summary && summary.unreadable > 0 && (
        <p className="banner" role="alert">{t("diagram.unreadable", { count: summary.unreadable, formattedCount: number(summary.unreadable), detail: summary.firstError ?? "" })}</p>
      )}
      {(load.kind === "error" || layoutError || lost || savedLoad.kind === "error") && (
        <p className="banner error" role="alert">
          {savedLoad.kind === "error"
            ? t("diagram.openFailed", { detail: savedLoad.message })
            : load.kind === "error"
              ? t("diagram.readFailed", { detail: load.message })
              : layoutError
                ? t("diagram.layoutFailed", { detail: layoutError })
                : lost?.reason}
        </p>
      )}
      {status === "disconnected" && (
        <div className="er-actions">
          <button type="button" onClick={() => void reconnect()} disabled={!tab.connectionId}>{t("tableDetails.reconnect")}</button>
        </div>
      )}
      {status === "connected" && needsRefresh && (
        <div className="er-actions">
          <span className="muted">{t("diagram.refreshAfterReconnect")}</span>
          <button type="button" onClick={refresh}>{t("diagram.refresh")}</button>
        </div>
      )}
      <div
        ref={host}
        className={`er-canvas${drag.current?.moved && !drag.current.items ? " panning" : ""}`}
        tabIndex={0}
        role="img"
        aria-label={summary ? `${title}: ${countText(summary)}` : title}
        data-layouts={layouts.current}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          const target = targetAt(e.target);
          drag.current = {
            x: e.clientX,
            y: e.clientY,
            view,
            moved: false,
            target,
            items: target ? itemsFor(target) : null,
            additive: e.ctrlKey || e.metaKey || e.shiftKey,
          };
          e.currentTarget.setPointerCapture(e.pointerId);
        }}
        onPointerMove={(e) => {
          const start = drag.current;
          if (!start) return;
          const dx = e.clientX - start.x;
          const dy = e.clientY - start.y;
          if (!start.moved && Math.hypot(dx, dy) < 3) return;
          start.moved = true;
          if (start.items) {
            setMoving({ ...start.items, dx: dx / view.k, dy: dy / view.k });
          } else {
            setView({ ...start.view, x: start.view.x + dx, y: start.view.y + dy });
          }
        }}
        onPointerUp={(e) => {
          const start = drag.current;
          drag.current = null;
          if (!start) return;
          if (start.moved && start.items) {
            const dx = (e.clientX - start.x) / view.k;
            const dy = (e.clientY - start.y) / view.k;
            // Kept on screen where it was dropped until the core's layout comes back.
            setMoving({ ...start.items, dx, dy });
            commitMove(start.items, dx, dy);
            return;
          }
          if (start.moved) return;
          // A click: a table is selected (Ctrl, Shift or Cmd adds or takes it out).
          const node = start.target?.kind === "node" ? start.target.index : null;
          if (node === null) {
            if (!start.additive) setSelection([]);
          } else if (start.additive) {
            setSelection((s) => (s.includes(node) ? s.filter((i) => i !== node) : [...s, node]));
          } else {
            setSelection([node]);
          }
        }}
        onDoubleClick={(e) => {
          const target = targetAt(e.target);
          if (target && (target.kind === "note" || target.kind === "group")) editTarget(target);
        }}
        onContextMenu={(e) => {
          const target = targetAt(e.target);
          if (target === null) return;
          e.preventDefault();
          if (target.kind === "node" && !selection.includes(target.index)) setSelection([target.index]);
          setMenu({ x: e.clientX, y: e.clientY, target });
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
            setSelection([]);
          }
        }}
      >
        {layout && (
          <svg className="er-svg">
            <g ref={content} transform={`translate(${view.x} ${view.y}) scale(${view.k})`}>
              <DiagramContent
                layout={layout}
                selection={selection}
                newTables={newTables}
                moving={moving}
                relationLabel={relationLabel}
                goneLabel={t("diagram.gone")}
                goneTip={goneTip}
                newLabel={t("diagram.new")}
                newTip={t("diagram.newTip")}
              />
            </g>
          </svg>
        )}
        {summary && summary.tables === 0 && layout && layout.gone.length === 0 && (
          <div className="empty">{scope.kind === "schema" ? t("diagram.emptySchema") : t("diagram.empty")}</div>
        )}
      </div>
      {menu && (
        // A table near the bottom or the right edge opens it inside the window too (SDB-255).
        <ObjectMenu at={menu}>{menuItems(menu.target)}</ObjectMenu>
      )}
      {editor && <DiagramEditorDialog editor={editor} onCancel={() => setEditor(null)} onDone={finishEditor} />}
      {deleting && savedDiagram && (
        <ConfirmDelete
          title={t("diagram.deleteTitle")}
          action={t("diagram.deleteAction")}
          onConfirm={deleteSaved}
          onCancel={() => setDeleting(false)}
        >
          <p>{t("diagram.deleteBody", { name: savedDiagram.name })}</p>
        </ConfirmDelete>
      )}
    </div>
  );
}
