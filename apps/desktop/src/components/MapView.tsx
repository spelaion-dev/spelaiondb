// A map of geometries (ADR-045), drawn as plain SVG: the core turns every engine's values into
// GeoJSON, and this view only projects and draws them. Without tiles (the default) the shapes
// sit on a plain background and nothing is asked of the network; with tiles turned on in
// Settings and coordinates in longitude and latitude (or Web Mercator), images from the user's
// own tile server go behind them, sent without a referrer.
//
// Pan by dragging or with the arrow keys, zoom with the wheel or + and −, 0 fits the shapes.
// Paths are built once per "base" view and moved with a transform while the user pans or
// zooms (strokes keep their width), then rebuilt when the gesture ends.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent as ReactKeyboardEvent, type PointerEvent as ReactPointerEvent } from "react";
import { fitted as fittedAt, moved, shouldFit, sizeKey, type FitMemory } from "../mapFit";
import { useTranslation } from "react-i18next";
import type { Geometry, MapCrs, MapPreferencesItem } from "../bindings";
import { Icon } from "./Icons";

export interface MapFeature {
  geometry: Geometry;
  /** The row of the current view it comes from, when the map shows a whole column. */
  row?: number;
}

type XY = [number, number];

/** A feature's parts in projected coordinates. */
interface Projected {
  points: XY[];
  lines: XY[][];
  /** Each polygon is its rings. */
  polygons: XY[][][];
  row?: number;
}

const EARTH_RADIUS = 6378137;
const WORLD = 2 * Math.PI * EARTH_RADIUS;
const HALF_WORLD = WORLD / 2;
const MAX_LAT = 85.05112878;
const TILE_SIZE = 256;
const MAX_ZOOM = 19;
const MAX_TILES = 96;
/** Pixels kept free around the shapes when fitting them. */
const FIT_PADDING = 24;

/** Whether the coordinates are longitude and latitude in degrees. */
function isLonLat(crs: MapCrs): boolean {
  return crs === "lonLat" || crs === "assumedLonLat";
}

/** Coordinates whose tiles exist: longitude and latitude, or Web Mercator. */
export function canShowTiles(crs: MapCrs): boolean {
  return crs !== "plane";
}

function project(crs: MapCrs, x: number | null, y: number | null): XY | null {
  if (x == null || y == null || !Number.isFinite(x) || !Number.isFinite(y)) return null;
  if (!isLonLat(crs)) return [x, y];
  const lat = Math.max(-MAX_LAT, Math.min(MAX_LAT, y));
  return [(EARTH_RADIUS * x * Math.PI) / 180, EARTH_RADIUS * Math.log(Math.tan(Math.PI / 4 + (lat * Math.PI) / 360))];
}

function unproject(crs: MapCrs, [x, y]: XY): XY {
  if (!isLonLat(crs)) return [x, y];
  return [(x / EARTH_RADIUS) * (180 / Math.PI), (2 * Math.atan(Math.exp(y / EARTH_RADIUS)) - Math.PI / 2) * (180 / Math.PI)];
}

function projectFeature(crs: MapCrs, feature: MapFeature): Projected {
  const out: Projected = { points: [], lines: [], polygons: [], row: feature.row };
  const line = (positions: [number | null, number | null][]): XY[] =>
    positions.map(([x, y]) => project(crs, x, y)).filter((p): p is XY => p !== null);
  const walk = (g: Geometry) => {
    switch (g.type) {
      case "Point": {
        const p = g.coordinates.length >= 2 ? project(crs, g.coordinates[0] ?? null, g.coordinates[1] ?? null) : null;
        if (p) out.points.push(p);
        break;
      }
      case "MultiPoint":
        out.points.push(...line(g.coordinates));
        break;
      case "LineString":
        out.lines.push(line(g.coordinates));
        break;
      case "MultiLineString":
        for (const l of g.coordinates) out.lines.push(line(l));
        break;
      case "Polygon":
        out.polygons.push(g.coordinates.map(line));
        break;
      case "MultiPolygon":
        for (const polygon of g.coordinates) out.polygons.push(polygon.map(line));
        break;
      case "GeometryCollection":
        for (const part of g.geometries) walk(part);
        break;
    }
  };
  walk(feature.geometry);
  return out;
}

interface Box {
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
}

function boxOf(features: Projected[]): Box | null {
  let box: Box | null = null;
  const add = ([x, y]: XY) => {
    box = box ? { minX: Math.min(box.minX, x), minY: Math.min(box.minY, y), maxX: Math.max(box.maxX, x), maxY: Math.max(box.maxY, y) } : { minX: x, minY: y, maxX: x, maxY: y };
  };
  for (const f of features) {
    f.points.forEach(add);
    f.lines.forEach((l) => l.forEach(add));
    f.polygons.forEach((p) => p.forEach((r) => r.forEach(add)));
  }
  return box;
}

/** The centre of the map in projected units, and pixels per unit. */
interface View {
  cx: number;
  cy: number;
  scale: number;
}

function maxScale(crs: MapCrs): number {
  // Tile zoom 19 in metres; any other system is free to zoom in far.
  return canShowTiles(crs) ? (TILE_SIZE * 2 ** MAX_ZOOM) / WORLD : Number.MAX_VALUE;
}

function fitView(box: Box | null, width: number, height: number, crs: MapCrs): View {
  if (!box) return { cx: 0, cy: 0, scale: canShowTiles(crs) ? width / WORLD : 1 };
  const w = Math.max(width - 2 * FIT_PADDING, 1);
  const h = Math.max(height - 2 * FIT_PADDING, 1);
  const spanX = box.maxX - box.minX;
  const spanY = box.maxY - box.minY;
  let scale: number;
  if (spanX === 0 && spanY === 0) {
    // One point: about two kilometres across on a web map, ten units otherwise.
    scale = canShowTiles(crs) ? w / 2000 : w / 10;
  } else {
    scale = Math.min(spanX > 0 ? w / spanX : Infinity, spanY > 0 ? h / spanY : Infinity);
  }
  return { cx: (box.minX + box.maxX) / 2, cy: (box.minY + box.maxY) / 2, scale: Math.min(scale, maxScale(crs)) };
}

/** SVG path data of a feature's parts, in the screen coordinates of `view`. */
function paths(f: Projected, view: View, width: number, height: number): { area: string; line: string; point: string } {
  const sx = (x: number) => ((x - view.cx) * view.scale + width / 2).toFixed(2);
  const sy = (y: number) => (height / 2 - (y - view.cy) * view.scale).toFixed(2);
  const run = (points: XY[], close: boolean) =>
    points.length === 0 ? "" : points.map(([x, y], i) => `${i === 0 ? "M" : "L"}${sx(x)} ${sy(y)}`).join("") + (close ? "Z" : "");
  return {
    area: f.polygons.map((p) => p.map((r) => run(r, true)).join("")).join(""),
    line: f.lines.map((l) => run(l, false)).join(""),
    // A point is a line of no length with round caps: its size stays the same at any zoom.
    point: f.points.map(([x, y]) => `M${sx(x)} ${sy(y)}h0`).join(""),
  };
}

interface Tile {
  key: string;
  url: string;
  left: number;
  top: number;
  size: number;
}

function tilesFor(view: View, width: number, height: number, template: string): Tile[] {
  const worldPx = view.scale * WORLD;
  const z = Math.max(0, Math.min(MAX_ZOOM, Math.round(Math.log2(worldPx / TILE_SIZE))));
  const n = 2 ** z;
  const size = worldPx / n;
  // The screen position of the world's top left corner.
  const ox = (-HALF_WORLD - view.cx) * view.scale + width / 2;
  const oy = height / 2 - (HALF_WORLD - view.cy) * view.scale;
  const firstX = Math.floor(-ox / size);
  const lastX = Math.floor((width - ox) / size);
  const firstY = Math.max(0, Math.floor(-oy / size));
  const lastY = Math.min(n - 1, Math.floor((height - oy) / size));
  const tiles: Tile[] = [];
  for (let ty = firstY; ty <= lastY; ty++) {
    for (let tx = firstX; tx <= lastX; tx++) {
      if (tiles.length >= MAX_TILES) return tiles;
      const x = ((tx % n) + n) % n;
      tiles.push({
        key: `${z}/${tx}/${ty}`,
        url: template.replaceAll("{z}", String(z)).replaceAll("{x}", String(x)).replaceAll("{y}", String(ty)),
        left: ox + tx * size,
        top: oy + ty * size,
        size,
      });
    }
  }
  return tiles;
}

export function MapView(props: {
  features: MapFeature[];
  crs: MapCrs;
  /** What the map shows, for its accessible name. */
  label: string;
  preferences: MapPreferencesItem;
  /** A shape of a column's map was clicked: show its row. */
  onPickRow?(row: number): void;
}) {
  const { features, crs, preferences } = props;
  const { t, i18n } = useTranslation("editor");
  const host = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState({ width: 0, height: 0 });
  const projected = useMemo(() => features.map((f) => projectFeature(crs, f)), [features, crs]);
  const box = useMemo(() => boxOf(projected), [projected]);
  const [view, setView] = useState<View | null>(null);
  /** The view the paths were built for; the drawn paths move with a transform from it. */
  const [base, setBase] = useState<View | null>(null);
  const [hovered, setHovered] = useState<number | null>(null);
  const [pointer, setPointer] = useState<XY | null>(null);
  const [tilesShown, setTilesShown] = useState(true);
  const [tileFailures, setTileFailures] = useState(0);
  const drag = useRef<{ x: number; y: number; view: View; moved: boolean } | null>(null);

  useLayoutEffect(() => {
    const element = host.current;
    if (!element) return;
    const observer = new ResizeObserver(([entry]) => {
      if (entry) setSize({ width: Math.round(entry.contentRect.width), height: Math.round(entry.contentRect.height) });
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, []);

  // The last automatic fit (src/mapFit.ts). `autoFit` is its size while the view is still it,
  // shown on the map as `data-fit` (the smoke test waits for it to match `data-size`).
  const fitted = useRef<FitMemory | null>(null);
  const [autoFit, setAutoFit] = useState<string | null>(null);
  const fit = useCallback(() => {
    if (size.width === 0 || size.height === 0) return;
    const next = fitView(box, size.width, size.height, crs);
    setView(next);
    setBase(next);
    fitted.current = fittedAt(box, size.width, size.height);
    setAutoFit(sizeKey(size.width, size.height));
  }, [box, size.width, size.height, crs]);
  /** The user moved the view: it is theirs, and a size change keeps it. */
  const leaveFit = useCallback(() => {
    fitted.current = moved(fitted.current);
    setAutoFit(null);
  }, []);

  // New shapes, the first size, or a new size while the view is still the automatic fit.
  useEffect(() => {
    if (shouldFit(fitted.current, box, size.width, size.height)) fit();
  }, [box, fit, size.width, size.height]);

  // The paths are rebuilt for the current view a moment after a gesture ends.
  useEffect(() => {
    if (!view || view === base) return;
    const timer = window.setTimeout(() => {
      if (!drag.current) setBase(view);
    }, 150);
    return () => window.clearTimeout(timer);
  }, [view, base]);

  const drawn = useMemo(
    () => (base ? projected.map((f) => paths(f, base, size.width, size.height)) : []),
    [projected, base, size.width, size.height],
  );

  const zoomAt = useCallback(
    (factor: number, at?: XY) => {
      leaveFit();
      setView((current) => {
        if (!current) return current;
        const scale = Math.max(1e-12, Math.min(maxScale(crs), current.scale * factor));
        const applied = scale / current.scale;
        const [px, py] = at ?? [size.width / 2, size.height / 2];
        // The point under the cursor stays where it is.
        const ux = current.cx + (px - size.width / 2) / current.scale;
        const uy = current.cy - (py - size.height / 2) / current.scale;
        return { scale, cx: ux - (ux - current.cx) / applied, cy: uy - (uy - current.cy) / applied };
      });
    },
    [crs, size.width, size.height, leaveFit],
  );

  // The wheel needs a listener that may prevent the page from scrolling.
  useEffect(() => {
    const element = host.current;
    if (!element) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      const rect = element.getBoundingClientRect();
      zoomAt(Math.pow(1.2, -e.deltaY / 100), [e.clientX - rect.left, e.clientY - rect.top]);
    };
    element.addEventListener("wheel", onWheel, { passive: false });
    return () => element.removeEventListener("wheel", onWheel);
  }, [zoomAt]);

  const pan = (dx: number, dy: number) => {
    leaveFit();
    setView((v) => (v ? { ...v, cx: v.cx - dx / v.scale, cy: v.cy + dy / v.scale } : v));
  };

  function onKeyDown(e: ReactKeyboardEvent) {
    const step = Math.min(size.width, size.height) / 8;
    const handled = (() => {
      switch (e.key) {
        case "ArrowLeft": pan(step, 0); return true;
        case "ArrowRight": pan(-step, 0); return true;
        case "ArrowUp": pan(0, step); return true;
        case "ArrowDown": pan(0, -step); return true;
        case "+":
        case "=": zoomAt(1.5); return true;
        case "-": zoomAt(1 / 1.5); return true;
        case "0": fit(); return true;
        default: return false;
      }
    })();
    if (handled) {
      e.preventDefault();
      e.stopPropagation();
    }
  }

  function local(e: ReactPointerEvent): XY {
    const rect = host.current!.getBoundingClientRect();
    return [e.clientX - rect.left, e.clientY - rect.top];
  }

  function onPointerDown(e: ReactPointerEvent) {
    if (e.button !== 0 || !view) return;
    const [x, y] = local(e);
    drag.current = { x, y, view, moved: false };
    e.currentTarget.setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: ReactPointerEvent) {
    const [x, y] = local(e);
    if (view) {
      const unit: XY = [view.cx + (x - size.width / 2) / view.scale, view.cy - (y - size.height / 2) / view.scale];
      setPointer(unproject(crs, unit));
    }
    const d = drag.current;
    if (d) {
      if (!d.moved && Math.abs(x - d.x) + Math.abs(y - d.y) > 3) {
        d.moved = true;
        leaveFit();
      }
      setView({ ...d.view, cx: d.view.cx - (x - d.x) / d.view.scale, cy: d.view.cy + (y - d.y) / d.view.scale });
      return;
    }
    const feature = (e.target as Element).closest?.("[data-feature]");
    setHovered(feature ? Number(feature.getAttribute("data-feature")) : null);
  }

  function onPointerUp(e: ReactPointerEvent) {
    const d = drag.current;
    drag.current = null;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
    if (d && !d.moved && hovered !== null) {
      const row = projected[hovered]?.row;
      if (row !== undefined) props.onPickRow?.(row);
    }
    if (view) setBase(view);
  }

  const tileServer = preferences.tiles && preferences.tileUrl !== "" && canShowTiles(crs);
  const showTiles = tileServer && tilesShown && view !== null && size.width > 0;
  const tiles = showTiles && view ? tilesFor(view, size.width, size.height, preferences.tileUrl) : [];
  useEffect(() => setTileFailures(0), [preferences.tileUrl]);

  // The transform from the base view to the current one.
  const transform = (() => {
    if (!view || !base) return undefined;
    const k = view.scale / base.scale;
    const tx = size.width / 2 - k * (size.width / 2 + (view.cx - base.cx) * base.scale);
    const ty = size.height / 2 - k * (size.height / 2 - (view.cy - base.cy) * base.scale);
    return `matrix(${k} 0 0 ${k} ${tx} ${ty})`;
  })();

  const fmt = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: isLonLat(crs) ? 5 : 2 });
  const hoveredRow = hovered === null ? undefined : projected[hovered]?.row;

  return (
    <div className="map-view">
      <div
        ref={host}
        className={`map-host ${showTiles ? "with-tiles" : ""}`}
        tabIndex={0}
        role="group"
        aria-label={props.label}
        aria-roledescription={t("viewer.map.roledescription")}
        data-size={sizeKey(size.width, size.height)}
        data-fit={autoFit ?? undefined}
        onKeyDown={onKeyDown}
        onPointerDown={onPointerDown}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerLeave={() => { setHovered(null); setPointer(null); }}
      >
        {tiles.length > 0 && (
          <div className="map-tiles" aria-hidden="true">
            {tiles.map((tile) => (
              <img
                key={tile.key}
                src={tile.url}
                alt=""
                draggable={false}
                referrerPolicy="no-referrer"
                style={{ left: tile.left, top: tile.top, width: tile.size, height: tile.size }}
                onError={(e) => { e.currentTarget.style.visibility = "hidden"; setTileFailures((n) => n + 1); }}
              />
            ))}
          </div>
        )}
        <svg className="map-svg" width={size.width} height={size.height} aria-hidden="true">
          <g transform={transform}>
            {drawn.map((d, i) => (
              <g key={i} className={`map-feature ${hovered === i ? "hovered" : ""}`} data-feature={i}>
                {d.area && <path className="map-area" d={d.area} vectorEffect="non-scaling-stroke" fillRule="evenodd" />}
                {d.line && <path className="map-line" d={d.line} vectorEffect="non-scaling-stroke" />}
                {d.point && <path className="map-point-ring" d={d.point} vectorEffect="non-scaling-stroke" />}
                {d.point && <path className="map-point" d={d.point} vectorEffect="non-scaling-stroke" />}
              </g>
            ))}
          </g>
        </svg>
        <div className="map-controls" onPointerDown={(e) => e.stopPropagation()}>
          <button className="ghost map-control" title={`${t("viewer.map.zoomIn")} (+)`} aria-label={t("viewer.map.zoomIn")} onClick={() => zoomAt(1.5)}><Icon name="plus" /></button>
          <button className="ghost map-control" title={`${t("viewer.map.zoomOut")} (−)`} aria-label={t("viewer.map.zoomOut")} onClick={() => zoomAt(1 / 1.5)}><Icon name="minus" /></button>
          <button className="ghost map-control" title={`${t("viewer.map.fit")} (0)`} aria-label={t("viewer.map.fit")} onClick={fit}><Icon name="scan" /></button>
          {tileServer && (
            <button className="ghost small map-control map-tiles-toggle" aria-pressed={tilesShown} title={t("viewer.map.tilesHint", { name: preferences.tileName })} onClick={() => setTilesShown((v) => !v)}>
              {t("viewer.map.tiles")}
            </button>
          )}
        </div>
        {showTiles && (
          <div className="map-attribution">{preferences.tileAttribution ? `${preferences.tileName} · ${preferences.tileAttribution}` : preferences.tileName}</div>
        )}
        {hoveredRow !== undefined && (
          <div className="map-hover" role="status">{t("viewer.map.row", { number: (hoveredRow + 1).toLocaleString(i18n.language) })}</div>
        )}
      </div>
      <p className="muted small map-caption">
        {pointer ? (isLonLat(crs) ? t("viewer.map.lonLat", { lon: fmt.format(pointer[0]), lat: fmt.format(pointer[1]) }) : t("viewer.map.xy", { x: fmt.format(pointer[0]), y: fmt.format(pointer[1]) })) : " "}
      </p>
      {tileFailures > 0 && showTiles && <p className="muted small">{t("viewer.map.tileFailures", { name: preferences.tileName })}</p>}
    </div>
  );
}
