// Bounded core projections only: parsing, ordering, metrics, findings and file data stay Rust-side.
import { useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { PlanSummary_Serialize as PlanSummary, PlanPage_Serialize as PlanPage, PlanNode_Serialize as PlanNode, PlanView, PlanSort, PlanStatKind, PlanStatsPage_Serialize as PlanStatsPage, RawWindow } from "../bindings";
import { chooseSavePath } from "../dialogs";
import { openSavedPlan } from "../planFiles";
import { type Tab } from "../store";
import { Icon } from "./Icons";

const PAGE_SIZE = 50;
const defaultView = (): PlanView => ({ sort: "tree", descending: false, collapsed: [], findingsOnly: false });
const reason = (e: unknown) => e instanceof Error ? e.message : String(e);
let exportSequence = 3_000_000;

type Metric = { key: PlanSort; label: string; value(node: PlanNode): number | null };
const METRICS: Metric[] = [
  { key: "cost", label: "cost", value: n => n.metrics.cost },
  { key: "selfCost", label: "selfCost", value: n => n.metrics.selfCost },
  { key: "estimatedRows", label: "estimatedRows", value: n => n.metrics.estimatedRows },
  { key: "actualRowsPerLoop", label: "actualRowsPerLoop", value: n => n.metrics.actualRowsPerLoop },
  { key: "actualRows", label: "actualRows", value: n => n.metrics.actualRows },
  { key: "time", label: "time", value: n => n.metrics.timeMs },
  { key: "selfTime", label: "selfTime", value: n => n.metrics.selfTimeMs },
  { key: "timePerLoop", label: "timePerLoop", value: n => n.metrics.timePerLoopMs },
  { key: "loops", label: "loops", value: n => n.metrics.loops },
  { key: "buffersHit", label: "buffersHit", value: n => n.metrics.buffersHit },
  { key: "buffersRead", label: "buffersRead", value: n => n.metrics.buffersRead },
  { key: "rowsRemoved", label: "rowsRemoved", value: n => n.metrics.rowsRemoved },
  { key: "heapFetches", label: "heapFetches", value: n => n.metrics.heapFetches },
  { key: "workersPlanned", label: "workersPlanned", value: n => n.metrics.workersPlanned },
  { key: "workersLaunched", label: "workersLaunched", value: n => n.metrics.workersLaunched },
  { key: "temporaryRead", label: "temporaryRead", value: n => n.metrics.temporaryRead },
  { key: "temporaryWritten", label: "temporaryWritten", value: n => n.metrics.temporaryWritten },
  { key: "sortSpace", label: "sortSpace", value: n => n.metrics.sortSpaceKb },
];

export function PlanFileTab({ tab }: { tab: Tab }) {
  const { t } = useTranslation("editor");
  const [summary, setSummary] = useState<PlanSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const path = tab.plan?.path ?? "";
  useEffect(() => {
    let active = true;
    setError(null); setSummary(null);
    void call(commands.openPlanFile(path)).then((result) => {
      if (active) setSummary(result);
      else void commands.releasePlan(result.id);
    }).catch((e: unknown) => { if (active) setError(t("plan.openFailed", { reason: reason(e) })); });
    return () => { active = false; };
  }, [path, revision, t]);
  if (summary) return <PlanViewer summary={summary} />;
  return <div className="plan-viewer"><div className="plan-heading"><h2>{t("plan.title")}</h2></div>{error
    ? <div className="banner plan-error" role="alert"><p>{error}</p><button onClick={() => setRevision(r => r + 1)}>{t("history.retry")}</button></div>
    : <div className="empty" role="status">{t("plan.opening")}</div>}</div>;
}

export function PlanViewer({ summary, controls, onBack }: { summary: PlanSummary; controls?: ReactNode; onBack?(): void }) {
  const { t, i18n } = useTranslation("editor");
  const number = useMemo(() => new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 3 }), [i18n.language]);
  const fmt = (value: number | null | undefined) => value == null ? t("plan.unavailable") : number.format(value);
  const [mode, setMode] = useState<"overview" | "tree" | "graph" | "raw">("overview");
  const [view, setView] = useState(defaultView);
  const [offset, setOffset] = useState(0);
  const [page, setPage] = useState<PlanPage | null>(null);
  const [selected, setSelected] = useState<PlanNode | null>(null);
  const [pendingSelection, setPendingSelection] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [refresh, setRefresh] = useState(0);
  const [rawOffsets, setRawOffsets] = useState([0]);
  const [raw, setRaw] = useState<RawWindow | null>(null);
  const [rawBusy, setRawBusy] = useState(false);
  const [exportId, setExportId] = useState<number | null>(null);
  const exportRef = useRef<number | null>(null);
  const generation = useRef(0);

  useEffect(() => { if (pendingSelection === null) { setOffset(0); setSelected(null); } }, [view, summary.id]);
  useEffect(() => {
    const current = ++generation.current;
    setPage(null); setError(null);
    void call(commands.planPage(summary.id, view, offset, PAGE_SIZE)).then(result => {
      if (current !== generation.current) return;
      setPage(result);
      if (pendingSelection !== null) { setSelected(result.nodes.find(n => n.id === pendingSelection) ?? null); setPendingSelection(null); }
    }).catch((e: unknown) => { if (current === generation.current) setError(t("plan.loadFailed", { reason: reason(e) })); });
    return () => { generation.current += 1; };
  }, [summary.id, view, offset, refresh, t]);
  useEffect(() => () => {
    if (exportRef.current !== null) void call(commands.cancelExport(exportRef.current)).catch(() => undefined);
    void commands.releasePlan(summary.id).catch(() => undefined);
  }, [summary.id]);
  useEffect(() => {
    if (mode !== "raw") return;
    let active = true; setRawBusy(true); setError(null);
    void call(commands.planRawWindow(summary.id, rawOffsets.at(-1) ?? 0)).then(result => { if (active) setRaw(result); })
      .catch((e: unknown) => { if (active) setError(t("plan.loadFailed", { reason: reason(e) })); })
      .finally(() => { if (active) setRawBusy(false); });
    return () => { active = false; };
  }, [summary.id, mode, rawOffsets, refresh, t]);

  function jump(id: number) {
    setView(defaultView()); setOffset(Math.floor(id / PAGE_SIZE) * PAGE_SIZE); setPendingSelection(id); setMode("tree");
  }
  function sort(key: PlanSort) { setView(v => ({ ...v, sort: key, descending: v.sort === key ? !v.descending : true })); }
  function collapse(id: number) { setView(v => ({ ...v, collapsed: v.collapsed.includes(id) ? v.collapsed.filter(n => n !== id) : [...v.collapsed, id] })); }
  async function save(csv: boolean) {
    setError(null); setNotice(null);
    try {
      const path = await chooseSavePath({ title: t(csv ? "plan.exportCsv" : "plan.saveRaw"), defaultPath: csv ? "execution-plan.csv" : "execution-plan.json", filters: [{ name: t(csv ? "plan.csvFiles" : "plan.files"), extensions: csv ? ["csv"] : ["json"] }] });
      if (!path) return;
      const id = ++exportSequence; exportRef.current = id; setExportId(id);
      await call(commands.exportPlan(id, summary.id, view, path, csv));
      if (exportRef.current === id) setNotice(t("plan.saved", { path }));
    } catch (e) { setError(t("plan.saveFailed", { reason: reason(e) })); }
    finally { exportRef.current = null; setExportId(null); }
  }
  const paging = <div className="plan-paging"><button disabled={!page || offset === 0} onClick={() => setOffset(o => Math.max(0, o - PAGE_SIZE))}>{t("history.previous")}</button><span className="muted small" role="status">{page ? t("plan.range", { from: page.total ? page.offset + 1 : 0, to: page.offset + page.nodes.length, total: page.total }) : t("plan.loading")}</span><button disabled={!page || page.nextOffset === null} onClick={() => { if (page?.nextOffset !== null && page?.nextOffset !== undefined) setOffset(page.nextOffset); }}>{t("history.next")}</button></div>;
  return <section className="plan-viewer" aria-label={t("plan.title")}>
    <header className="plan-heading"><div><h2>{t("plan.title")}</h2><span className="muted small">{summary.name} · {t(summary.analyzed ? "plan.measured" : "plan.estimated")}</span></div><div className="spacer" />
      {onBack && <button className="ghost" onClick={onBack}>{t("plan.back")}</button>}
      <button className="ghost" onClick={() => void openSavedPlan()}>{t("plan.open")}</button>
      {exportId === null ? <><button onClick={() => void save(false)}>{t("plan.saveRaw")}</button><button onClick={() => void save(true)}>{t("plan.exportCsv")}</button></> : <button onClick={() => void call(commands.cancelExport(exportId))}>{t("export.cancel")}</button>}
    </header>
    {controls}
    <nav className="plan-tabs" aria-label={t("plan.viewsLabel")}>{(["overview", "tree", "graph", "raw"] as const).map(tab => <button key={tab} className={mode === tab ? "active" : ""} aria-pressed={mode === tab} onClick={() => setMode(tab)}>{t(`plan.views.${tab}`)}</button>)}</nav>
    {error && <div className="banner plan-error" role="alert"><p>{error}</p><button onClick={() => setRefresh(r => r + 1)}>{t("history.retry")}</button></div>}
    {notice && <div className="banner" role="status">{notice}</div>}
    {mode === "overview" ? <div className="plan-overview">
      <dl className="plan-overview-numbers">{[["totalCost", summary.totalCost], ["planningTime", summary.planningMs], ["executionTime", summary.executionMs], ["nodes", summary.nodeCount], ["depth", summary.depth], ["findings", summary.findingCount], ["sequentialScans", summary.sequentialScans], ["sorts", summary.sorts], ["temporaryOperations", summary.temporaryOperations]].map(([key, value]) => <div key={key}><dt>{t(`plan.${key}`)}</dt><dd>{fmt(value as number | null)}</dd></div>)}</dl>
      <div className="plan-overview-steps">{[["highestCost", summary.highestCost], ["slowest", summary.slowest], ["estimateGap", summary.estimateGap]].map(([key, value]) => <div key={key as string}><h3>{t(`plan.${key}`)}</h3>{value && typeof value === "object" ? <button className="ghost plan-step-link" onClick={() => jump(value.node)}>{value.label}<span>{fmt(value.value)}</span></button> : <p className="muted">{t("plan.unavailableDetail")}</p>}</div>)}</div>
      <div className="plan-statistics">{(["operations", "relations", "indexes"] as const).map(kind => <PlanStatistics key={kind} id={summary.id} kind={kind} />)}</div>
      <button className="plan-findings-link" onClick={() => { setView(v => ({ ...v, findingsOnly: true })); setMode("tree"); }}>{t("plan.showFindings", { count: summary.findingCount })}</button>
      <details className="plan-capabilities"><summary>{t("plan.capabilities")}</summary><p>{t(`plan.engineNotes.${summary.engine}`, { defaultValue: summary.note })}</p></details>
    </div> : mode === "raw" ? <><div className="plan-raw" aria-busy={rawBusy}><p className="muted small">{t("plan.rawHelp")}</p>{rawBusy ? <p role="status">{t("plan.loading")}</p> : <pre>{raw?.text}</pre>}</div><div className="plan-paging"><button disabled={rawBusy || rawOffsets.length === 1} onClick={() => setRawOffsets(o => o.slice(0, -1))}>{t("history.previous")}</button><span className="muted small">{raw && t("plan.bytes", { from: raw.offset, to: raw.offset + new TextEncoder().encode(raw.text).length, total: raw.total })}</span><button disabled={rawBusy || raw?.nextOffset == null} onClick={() => { if (raw?.nextOffset != null) setRawOffsets(o => [...o, raw.nextOffset!]); }}>{t("history.next")}</button></div></> : <>
      <div className="plan-tools"><label>{t("plan.sortBy")}<select aria-label={t("plan.sortBy")} value={view.sort} onChange={e => setView(v => ({ ...v, sort: e.target.value as PlanSort }))}><option value="tree">{t("plan.treeOrder")}</option><option value="nodeType">{t("plan.operation")}</option>{METRICS.map(m => <option key={m.key} value={m.key}>{t(`plan.metrics.${m.label}`)}</option>)}<option value="findings">{t("plan.findings")}</option></select></label><button className="ghost" onClick={() => setView(v => ({ ...v, descending: !v.descending }))}>{t(view.descending ? "plan.descending" : "plan.ascending")}</button><label className="check"><input type="checkbox" checked={view.findingsOnly} onChange={e => setView(v => ({ ...v, findingsOnly: e.target.checked }))} />{t("plan.findingsOnly")}</label>{view.collapsed.length > 0 && <button className="ghost" onClick={() => setView(v => ({ ...v, collapsed: [] }))}>{t("plan.expandAll")}</button>}</div>
      <div className="plan-main"><div className="plan-content" aria-busy={!page}>{!page ? <div className="empty" role="status">{t("plan.loading")}</div> : page.nodes.length === 0 ? <div className="empty">{t("plan.noFindings")}</div> : mode === "graph" ? <PlanGraph nodes={page.nodes} selected={selected?.id ?? null} onSelect={setSelected} onJump={jump} /> : <div className="plan-table-scroll"><table className="plan-table"><thead><tr><th><button onClick={() => sort("nodeType")}>{t("plan.operation")}</button></th><th><button onClick={() => sort("relation")}>{t("plan.relation")}</button></th><th><button onClick={() => sort("index")}>{t("plan.index")}</button></th>{METRICS.map(metric => <th key={metric.key} aria-sort={view.sort === metric.key ? view.descending ? "descending" : "ascending" : "none"}><button onClick={() => sort(metric.key)}>{t(`plan.metrics.${metric.label}`)}</button></th>)}<th>{t("plan.findings")}</th></tr></thead><tbody>{page.nodes.map(node => <tr key={node.id} data-node-id={node.id} className={selected?.id === node.id ? "selected" : ""}><td><div className="plan-tree-label" style={{ paddingInlineStart: `calc(var(--plan-indent) * ${node.depth})` }}>
        {node.children.length > 0 ? <button className={`ghost icon-button plan-disclosure ${view.collapsed.includes(node.id) ? "" : "expanded"}`} aria-label={t(view.collapsed.includes(node.id) ? "plan.expandStep" : "plan.collapseStep", { name: node.nodeType })} onClick={() => collapse(node.id)}><Icon name="chevronRight" /></button> : <span className="plan-leaf-spacer" />}
        <button className="ghost plan-node-name" onClick={() => setSelected(node)} aria-pressed={selected?.id === node.id}><span className="muted small">{node.id + 1}</span>{node.nodeType}</button></div></td><td><code>{node.relation}</code></td><td><code>{node.index}</code></td>{METRICS.map(metric => <td key={metric.key} className="plan-numeric">{fmt(metric.value(node))}</td>)}<td>{node.findings.length > 0 && <button className="ghost plan-finding-count" onClick={() => setSelected(node)}>{t("plan.findingCount", { count: node.findings.length })}</button>}</td></tr>)}</tbody></table></div>}</div>
        <aside className="plan-inspector" aria-label={t("plan.details")}>{selected ? <><h3>{selected.nodeType} <span className="muted small">#{selected.id + 1}</span></h3>{selected.relation && <p><code>{selected.relation}</code></p>}{selected.index && <p><code>{selected.index}</code></p>}{selected.joinType && <p>{t("plan.joinType", { value: selected.joinType })}</p>}
          <dl className="plan-detail-metrics">{METRICS.map(metric => <div key={metric.key}><dt>{t(`plan.metrics.${metric.label}`)}</dt><dd>{fmt(metric.value(selected))}</dd></div>)}</dl>
          {[["filter", selected.filter], ["indexCondition", selected.indexCondition], ["hashCondition", selected.hashCondition], ["sortMethod", selected.sortMethod]].map(([key, value]) => value && <div key={key}><h4>{t(`plan.${key}`)}</h4><pre>{value}</pre></div>)}
          <h4>{t("plan.findings")}</h4>{selected.findings.length ? <ul className="plan-findings">{selected.findings.map(finding => <li key={finding.kind}><strong>{t(`plan.findingNames.${finding.kind}`)}</strong><p>{t(`plan.findingHelp.${finding.kind}`, { defaultValue: finding.explanation })}</p></li>)}</ul> : <p className="muted">{t("plan.noStepFindings")}</p>}
        </> : <div className="empty">{t("plan.selectStep")}</div>}</aside>
      </div>{paging}
    </>}
  </section>;
}

function PlanStatistics({ id, kind }: { id: number; kind: PlanStatKind }) {
  const { t, i18n } = useTranslation("editor");
  const [offset, setOffset] = useState(0); const [page, setPage] = useState<PlanStatsPage | null>(null); const [error, setError] = useState<string | null>(null);
  useEffect(() => { let active = true; void call(commands.planStatistics(id, kind, offset, PAGE_SIZE)).then(result => { if (active) setPage(result); }).catch((e: unknown) => { if (active) setError(reason(e)); }); return () => { active = false; }; }, [id, kind, offset]);
  const fmt = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 3 });
  return <section><h3>{t(`plan.stats.${kind}`)} <span className="muted small">{page?.total}</span></h3>{kind === "operations" && <p className="muted small">{t("plan.inclusiveHelp")}</p>}{error && <p className="error" role="alert">{error}</p>}<ul>{page?.items.map(item => <li key={item.name}><code>{item.name}</code><span className="muted small">{t("plan.statCount", { count: item.nodes })}{kind === "operations" && ` · ${item.cumulativeTimeMs == null ? t("plan.unavailable") : `${fmt.format(item.cumulativeTimeMs)} ms`}`}</span></li>)}</ul>{page?.total === 0 && <p className="muted small">{t("plan.none")}</p>}{page && page.total > PAGE_SIZE && <div className="plan-paging"><button disabled={offset === 0} onClick={() => setOffset(o => o - PAGE_SIZE)}>{t("history.previous")}</button><button disabled={page.nextOffset === null} onClick={() => { if (page.nextOffset !== null) setOffset(page.nextOffset); }}>{t("history.next")}</button></div>}</section>;
}

function PlanGraph({ nodes, selected, onSelect, onJump }: { nodes: PlanNode[]; selected: number | null; onSelect(node: PlanNode): void; onJump(id: number): void }) {
  const { t, i18n } = useTranslation("editor"); const stage = useRef<HTMLDivElement>(null);
  const [geometry, setGeometry] = useState({ width: 0, height: 0, gap: 0 });
  useEffect(() => { if (stage.current) { const css = getComputedStyle(stage.current); setGeometry({ width: parseFloat(css.getPropertyValue("--plan-node-w")), height: parseFloat(css.getPropertyValue("--plan-node-h")), gap: parseFloat(css.getPropertyValue("--space-xl")) }); } }, []);
  const positions = new Map(nodes.map((node, index) => [node.id, index]));
  const width = (Math.max(...nodes.map(n => n.depth)) + 1) * geometry.width;
  const height = nodes.length * geometry.height;
  const fmt = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 2 });
  return <div className="plan-graph-scroll"><div className="plan-graph-stage" ref={stage} style={{ width, height }}>
    {geometry.width > 0 && <svg className="plan-edges" viewBox={`0 0 ${width} ${height}`} width={width} height={height} aria-hidden="true">{nodes.map((node, index) => { const parent = node.parent === null ? undefined : positions.get(node.parent); if (parent === undefined) return null; const x = nodes[parent]!.depth * geometry.width + geometry.width - geometry.gap, y = parent * geometry.height + geometry.height / 2; const cx = node.depth * geometry.width, cy = index * geometry.height + geometry.height / 2; return <path key={node.id} d={`M${x},${y} H${x + geometry.gap / 2} V${cy} H${cx}`} />; })}</svg>}
    {nodes.map((node, index) => <article className={`plan-graph-node ${selected === node.id ? "selected" : ""}`} key={node.id} data-node-id={node.id} style={{ left: node.depth * geometry.width, top: index * geometry.height } as CSSProperties}><button className="ghost plan-graph-name" onClick={() => onSelect(node)} aria-pressed={selected === node.id}><span className="muted small">#{node.id + 1}</span> {node.nodeType}</button>{node.relation && <code title={node.relation}>{node.relation}</code>}<p className="muted small">{t("plan.graphMetrics", { cost: node.metrics.cost == null ? t("plan.unavailable") : fmt.format(node.metrics.cost), rows: node.metrics.actualRows == null ? node.metrics.estimatedRows == null ? t("plan.unavailable") : fmt.format(node.metrics.estimatedRows) : fmt.format(node.metrics.actualRows) })}</p>{node.findings.length > 0 && <button className="ghost plan-finding-count" onClick={() => onSelect(node)}>{t("plan.findingCount", { count: node.findings.length })}</button>}{node.parent !== null && !positions.has(node.parent) && <button className="ghost small" onClick={() => onJump(node.parent!)}>{t("plan.parentStep", { id: node.parent + 1 })}</button>}</article>)}
  </div></div>;
}
