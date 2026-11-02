import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  DataEditor,
  GridCellKind,
  type DataEditorRef,
  type GridCell,
  type GridColumn,
  type Item,
  type Rectangle,
} from "@glideapps/glide-data-grid";
import * as api from "./api";
import { BLOCK, RowCache } from "./rowCache";
import { runScrollBench, type ScrollBenchResult } from "./bench";

interface Current {
  id: number;
  columns: string[];
  rows: number;
  loading: boolean;
}

type Report = Record<string, unknown>;

const DEFAULT_PG = "host=127.0.0.1 port=15432 user=spelunk password=spelunk dbname=spelunk";
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export default function App() {
  const gridRef = useRef<DataEditorRef>(null);
  const cacheRef = useRef<RowCache | null>(null);
  const visibleRef = useRef<Rectangle>({ x: 0, y: 0, width: 0, height: 0 });
  const counters = useRef({ requested: 0, blank: 0 });
  const firstBlockRef = useRef<((t: number) => void) | null>(null);
  const loadedRowsRef = useRef(0);
  const startedRef = useRef(false);
  const reportRef = useRef<Report>({});

  const [current, setCurrent] = useState<Current | null>(null);
  const [info, setInfo] = useState<api.SystemInfo | null>(null);
  const [busy, setBusy] = useState(false);
  const [log, setLog] = useState<string[]>([]);
  const [pgConn, setPgConn] = useState(DEFAULT_PG);
  const [pgSql, setPgSql] = useState("select * from big");

  const addLog = (line: string) => setLog((l) => [...l, line]);

  // When a block arrives, repaint only the visible cells of that block.
  const onBlockLoaded = useCallback((b: number) => {
    const r = visibleRef.current;
    const from = Math.max(r.y, b * BLOCK);
    const to = Math.min(r.y + r.height, (b + 1) * BLOCK);
    const cells: { cell: Item }[] = [];
    for (let row = from; row < to; row++) {
      for (let col = r.x; col < r.x + r.width; col++) cells.push({ cell: [col, row] });
    }
    if (cells.length > 0) gridRef.current?.updateCells(cells);
    if (b === 0 && firstBlockRef.current) {
      const resolve = firstBlockRef.current;
      firstBlockRef.current = null;
      requestAnimationFrame(() => resolve(performance.now()));
    }
  }, []);

  const waitFirstBlock = () => new Promise<number>((resolve) => (firstBlockRef.current = resolve));


  const getCellContent = useCallback(([col, row]: Item): GridCell => {
    counters.current.requested++;
    const text = cacheRef.current?.get(row, col);
    if (text === undefined) {
      counters.current.blank++;
      return { kind: GridCellKind.Loading, allowOverlay: false };
    }
    return { kind: GridCellKind.Text, data: text, displayData: text, allowOverlay: false, readonly: true };
    // The result id changes the cache: force a new callback so the grid repaints.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [current?.id]);

  const columns = useMemo<GridColumn[]>(
    () => (current?.columns ?? []).map((title) => ({ title, id: title, width: 170 })),
    [current?.columns],
  );

  async function memory() {
    const m = await api.memoryStats();
    const heap = (performance as unknown as { memory?: { usedJSHeapSize: number } }).memory;
    return {
      processTreeMb: Math.round(m.treeMb),
      backendMb: Math.round(m.ownMb),
      processes: m.processes,
      jsHeapMb: heap ? Math.round(heap.usedJSHeapSize / 1_048_576) : null,
    };
  }

  async function loadSynthetic(rows: number) {
    const t0 = performance.now();
    const r = await api.openSynthetic(rows);
    cacheRef.current = new RowCache(r.id, onBlockLoaded);
    const firstPaint = waitFirstBlock();
    setCurrent({ id: r.id, columns: r.columns, rows: r.rows, loading: false });
    const result = {
      rows: r.rows,
      backendBuildMs: Math.round(r.elapsedMs),
      firstRowUiMs: Math.round((await firstPaint) - t0),
    };
    addLog(`Synthetic: ${r.rows} rows generated in ${result.backendBuildMs} ms; first row on screen after ${result.firstRowUiMs} ms`);
    return result;
  }

  function loadPostgres(conn: string, sql: string) {
    return new Promise<Report>((resolve, reject) => {
      const t0 = performance.now();
      let firstPaint: Promise<number> | null = null;
      let firstProgressMs = 0;
      let firstFetchMs = 0;
      loadedRowsRef.current = 0;
      api
        .openPostgres(conn, sql, (e) => {
          switch (e.kind) {
            case "started":
              cacheRef.current = new RowCache(e.id, onBlockLoaded);
                        firstPaint = waitFirstBlock();
              setCurrent({ id: e.id, columns: e.columns, rows: 0, loading: true });
              break;
            case "progress":
            case "done": {
              if (firstProgressMs === 0) {
                firstProgressMs = performance.now() - t0;
                const cache = cacheRef.current;
                const n = cache?.fetchLatenciesMs.length ?? 0;
                const check = () => {
                  if ((cache?.fetchLatenciesMs.length ?? 0) > n) firstFetchMs = performance.now() - t0;
                  else requestAnimationFrame(check);
                };
                requestAnimationFrame(check);
              }
              // The last requested block may have arrived incomplete: request it again.
              cacheRef.current?.invalidateFrom(loadedRowsRef.current);
              loadedRowsRef.current = e.rows;
              setCurrent((c) => c && { ...c, rows: e.rows, loading: e.kind === "progress" });
              if (e.kind === "done") {
                (firstPaint ?? Promise.resolve(performance.now())).then((t) => {
                  const result = {
                    rows: e.rows,
                    backendFirstBatchMs: Math.round(e.firstBatchMs),
                    backendTotalMs: Math.round(e.elapsedMs),
                    firstRowUiMs: Math.round(t - t0),
                    firstProgressUiMs: Math.round(firstProgressMs),
                    firstFetchDoneUiMs: Math.round(firstFetchMs),
                  };
                  addLog(`Postgres: ${e.rows} rows; first batch after ${result.backendFirstBatchMs} ms, first row on screen after ${result.firstRowUiMs} ms, all loaded after ${result.backendTotalMs} ms`);
                  resolve(result);
                });
              }
              break;
            }
            case "failed":
              reject(new Error(e.message));
              break;
          }
        })
        .catch(reject);
    });
  }

  async function bench(label: string): Promise<ScrollBenchResult> {
    await sleep(500);
    const scroller = document.querySelector<HTMLElement>(".dvn-scroller");
    if (!scroller || !cacheRef.current) throw new Error("no grid to measure");
    scroller.scrollTop = 0;
    await sleep(300);
    const r = await runScrollBench({
      scroller,
      durationMs: 8000,
      cellCounters: counters.current,
      fetchLatenciesMs: cacheRef.current.fetchLatenciesMs,
    });
    addLog(
      `Scroll ${label}: ${r.avgFps} fps on average, p95 ${r.p95FrameMs} ms/frame, ${r.slowFramePct}% slow frames, ${r.blankCellPct}% empty cells, fetch p95 ${r.fetchP95Ms} ms`,
    );
    return r;
  }

  async function runAll(i: api.SystemInfo, pg: string | null, sql: string) {
    setBusy(true);
    const report: Report = {
      system: { os: i.os, arch: i.arch, webview: i.webview, userAgent: navigator.userAgent },
      startedAt: new Date().toISOString(),
    };
    reportRef.current = report;
    try {
      report.memoryIdle = await memory();
      const synthetic: Report = await loadSynthetic(i.rows);
      report.synthetic = synthetic;
      synthetic.scroll = await bench("synthetic");
      synthetic.memory = await memory();
      if (pg) {
        const postgres = await loadPostgres(pg, sql);
        report.postgres = postgres;
        postgres.scroll = await bench("postgres");
        postgres.memory = await memory();
      }
    } catch (e) {
      report.error = String(e);
      addLog(`ERROR: ${e}`);
    }
    try {
      addLog(`Report saved to ${await api.saveReport(report)}`);
    } catch (e) {
      addLog(`Could not save the report: ${e}`);
    }
    setBusy(false);
    if (i.autorun) await api.exitApp();
  }

  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    api.systemInfo().then((i) => {
      setInfo(i);
      if (i.pg) setPgConn(i.pg);
      setPgSql(i.pgSql);
      if (i.autorun) runAll(i, i.pg, i.pgSql);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const run = (f: () => Promise<unknown>) => async () => {
    setBusy(true);
    try {
      await f();
    } catch (e) {
      addLog(`ERROR: ${e}`);
    }
    setBusy(false);
  };

  return (
    <div className="app">
      <header className="toolbar">
        <strong>Phase 0 · grid + streaming</strong>
        <span className="muted">
          {info ? `${info.os}/${info.arch} · WebView ${info.webview}` : "…"}
        </span>
        <button disabled={busy || !info} onClick={run(() => loadSynthetic(info!.rows))}>
          Synthetic {info ? (info.rows / 1e6).toFixed(1) : ""} M
        </button>
        <input className="conn" value={pgConn} onChange={(e) => setPgConn(e.target.value)} />
        <input className="sql" value={pgSql} onChange={(e) => setPgSql(e.target.value)} />
        <button disabled={busy} onClick={run(() => loadPostgres(pgConn, pgSql))}>
          Postgres
        </button>
        <button disabled={busy || !current} onClick={run(() => bench("manual"))}>
          Measure scrolling
        </button>
        <button disabled={busy || !info} onClick={() => runAll(info!, pgConn, pgSql)}>
          Run all + report
        </button>
      </header>
      <main className="body">
        <section className="grid">
          {current && (
            <DataEditor
              // A new grid per result: it starts at the top without the previous one's state.
              key={current.id}
              ref={gridRef}
              columns={columns}
              rows={current.rows}
              getCellContent={getCellContent}
              onVisibleRegionChanged={(r) => (visibleRef.current = r)}
              rowMarkers="number"
              smoothScrollY
              width="100%"
              height="100%"
            />
          )}
        </section>
        <aside className="log">
          <div className="muted">
            {current ? `${current.rows.toLocaleString("en-US")} rows${current.loading ? " (loading…)" : ""}` : "No result"}
          </div>
          {log.map((line, i) => (
            <div key={i}>{line}</div>
          ))}
        </aside>
      </main>
    </div>
  );
}
