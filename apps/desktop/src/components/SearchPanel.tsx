// Search across the database (ADR-043), a panel of the activity bar (Ctrl+H).
// - Names: tables, views, routines and columns of every schema the explorer has read, on every
//   connection, matched in dbSearch.ts from the explorer's catalog (catalog.ts). Each schema's
//   columns are read once, with one catalog query, when the search first needs them.
// - Data: a value in the chosen tables of one connection. The core writes every statement, with
//   the value bound as a parameter, and runs them as reads in a session of its own; this panel
//   only names the tables and options, shows what the core reports and can cancel it. Each hit
//   opens the table data tab filtered to its row by the core's filter.

import { Channel } from "@tauri-apps/api/core";
import { useDeferredValue, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  call,
  commands,
  type DataSearchEventItem,
  type DataSearchLimitsItem,
  type DataSearchSummaryItem,
  type SearchHitItem,
  type TableStatusItem,
} from "../api";
import { useCatalog, type CatalogSchema } from "../catalog";
import { openTableData } from "../connect";
import { type NameHit, schemasWithoutColumns, searchableTables, searchNames } from "../dbSearch";
import { useApp } from "../store";
import { openFound } from "./OpenObjectDialog";

/** Fired on `window` by the "Search" command: the panel focuses its box. */
export const FOCUS_SEARCH_EVENT = "spelaion-focus-search";

/** Ids of data searches: far from the export dialog's and column statistics', which share the
 *  core's cancel map. */
let nextSearchId = 1_500_000_000;

/** Name matches shown at most; the count says how many there are. */
const NAME_LIMIT = 200;

const FALLBACK_LIMITS: DataSearchLimitsItem = { maxTables: 1000, maxRowsPerTable: 1000, defaultRowsPerTable: 100, maxHits: 1000, maxValueChars: 1000 };

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const keyOf = (connectionId: string, schema: string) => `${connectionId}\u0000${schema}`;
const tableKey = (schema: string, table: string) => `${schema}\u0000${table}`;

type Mode = "names" | "data";

export function SearchPanel({ hidden }: { hidden: boolean }) {
  const { t } = useTranslation("shell");
  const [mode, setMode] = useState<Mode>("names");
  const [text, setText] = useState("");
  const input = useRef<HTMLInputElement>(null);
  const openFirst = useRef<(() => void) | null>(null);
  const startData = useRef<(() => void) | null>(null);

  useEffect(() => {
    const focus = () => requestAnimationFrame(() => input.current?.select());
    window.addEventListener(FOCUS_SEARCH_EVENT, focus);
    return () => window.removeEventListener(FOCUS_SEARCH_EVENT, focus);
  }, []);

  return (
    <aside id="search-sidebar" className="search-sidebar" hidden={hidden} aria-label={t("search.title")}>
      <div className="sidebar-header"><div className="workspace-name">{t("search.title")}</div></div>
      <div className="search-box">
        <input
          ref={input}
          type="search"
          value={text}
          maxLength={FALLBACK_LIMITS.maxValueChars}
          placeholder={mode === "names" ? t("search.namesPlaceholder") : t("search.valuePlaceholder")}
          aria-label={mode === "names" ? t("search.namesPlaceholder") : t("search.valuePlaceholder")}
          onChange={(e) => setText(e.target.value)}
          onKeyDown={(e) => {
            if (e.key !== "Enter") return;
            e.preventDefault();
            (mode === "names" ? openFirst : startData).current?.();
          }}
        />
        <div className="segmented" role="radiogroup" aria-label={t("search.mode")}>
          {(["names", "data"] as const).map((m) => (
            <button key={m} type="button" role="radio" aria-checked={mode === m} className={mode === m ? "active" : ""} onClick={() => setMode(m)}>
              {t(`search.modes.${m}`)}
            </button>
          ))}
        </div>
      </div>
      <NameSearch query={text} hidden={mode !== "names"} openFirst={openFirst} />
      <DataSearch value={text} hidden={mode !== "data"} start={startData} />
    </aside>
  );
}

function NameSearch({ query, hidden, openFirst }: { query: string; hidden: boolean; openFirst: React.MutableRefObject<(() => void) | null> }) {
  const { t } = useTranslation("shell");
  const schemas = useCatalog((s) => s.schemas);
  const connections = useApp((s) => s.connections);
  const [columns, setColumns] = useState(true);
  const [reading, setReading] = useState(0);
  const [failures, setFailures] = useState<Record<string, string>>({});
  const [openError, setOpenError] = useState<string | null>(null);
  const attempted = useRef(new Set<string>());
  const deferred = useDeferredValue(query);
  const list = useMemo(() => Object.values(schemas), [schemas]);

  // Each loaded schema's columns, read once when a search first needs them (ADR-043); a schema
  // the explorer reads again is published without them and read again here.
  const missing = useMemo(
    () => (columns && deferred.trim() ? schemasWithoutColumns(list).filter((s) => !attempted.current.has(keyOf(s.connectionId, s.schema))) : []),
    [columns, deferred, list],
  );
  useEffect(() => {
    if (missing.length === 0) return;
    for (const entry of missing) attempted.current.add(keyOf(entry.connectionId, entry.schema));
    setReading((n) => n + missing.length);
    void (async () => {
      for (const entry of missing) {
        const key = keyOf(entry.connectionId, entry.schema);
        try {
          const read = await call(commands.listColumns(`meta:${entry.connectionId}`, entry.schema));
          useCatalog.getState().setColumns(entry.connectionId, entry.schema, read);
          setFailures((f) => {
            if (!(key in f)) return f;
            const next = { ...f };
            delete next[key];
            return next;
          });
        } catch (e) {
          setFailures((f) => ({ ...f, [key]: errorText(e) }));
        } finally {
          setReading((n) => n - 1);
        }
      }
    })();
  }, [missing]);
  // A schema forgotten by the explorer may be read again once it comes back.
  useEffect(() => {
    for (const key of [...attempted.current]) {
      const entry = schemas[key];
      if (!entry) attempted.current.delete(key);
    }
  }, [schemas]);

  const result = useMemo(() => searchNames(list, deferred, { columns, limit: NAME_LIMIT }), [list, deferred, columns]);
  const connectionName = (id: string) => connections.find((c) => c.id === id)?.name ?? id;
  const groups = useMemo(() => {
    const byConnection = new Map<string, NameHit[]>();
    for (const hit of result.hits) byConnection.set(hit.connectionId, [...(byConnection.get(hit.connectionId) ?? []), hit]);
    return [...byConnection.entries()];
  }, [result]);

  function open(hit: NameHit) {
    setOpenError(null);
    const entry: CatalogSchema | undefined = schemas[keyOf(hit.connectionId, hit.schema)];
    if (hit.kind === "schemaObject") {
      const object = entry?.others.find((o) => o.name === hit.name && o.kind === hit.type && (o.detail ?? null) === (hit.detail ?? null));
      if (!object) return;
      openFound(hit.connectionId, { kind: "schemaObject", object }).catch((e: unknown) => setOpenError(t("search.openFailed", { detail: errorText(e) })));
      return;
    }
    const object = entry?.relations.find((r) => r.name === hit.name) ?? { schema: hit.schema, name: hit.name, kind: "table", estimatedRows: null, comment: null };
    void openFound(hit.connectionId, { kind: "relation", object });
  }
  openFirst.current = result.hits[0] ? () => open(result.hits[0]!) : null;

  const failed = Object.entries(failures);
  return (
    <div className="search-body" hidden={hidden}>
      <label className="check search-option">
        <input type="checkbox" checked={columns} onChange={(e) => setColumns(e.target.checked)} />
        {t("search.includeColumns")}
      </label>
      {reading > 0 && <div className="search-status muted" role="status">{t("search.readingColumns", { count: reading })}</div>}
      {failed.map(([key, detail]) => {
        const [connectionId, schema] = key.split("\u0000");
        return <div key={key} className="row-note" role="alert">{t("search.columnsFailed", { schema, connection: connectionName(connectionId ?? ""), detail })}</div>;
      })}
      {openError && <div className="row-note" role="alert">{openError}</div>}
      {list.length === 0 ? (
        <div className="empty small">{t("search.noCatalog")}</div>
      ) : !deferred.trim() ? (
        <div className="empty small">{t("search.namesHint")}</div>
      ) : result.total === 0 ? (
        reading > 0 ? null : <div className="empty small">{t("search.noNames")}</div>
      ) : (
        <>
          <div className="search-status muted" role="status">
            {result.total > result.hits.length ? t("search.namesLimited", { shown: result.hits.length, count: result.total }) : t("search.namesCount", { count: result.total })}
          </div>
          <div className="search-results">
            {groups.map(([connectionId, hits]) => (
              <section key={connectionId} aria-label={connectionName(connectionId)}>
                <div className="group-title">{connectionName(connectionId)}</div>
                {hits.map((hit) => (
                  <button
                    key={`${hit.kind}\u0000${hit.schema}\u0000${hit.name}\u0000${hit.column ?? ""}\u0000${hit.detail ?? ""}`}
                    type="button"
                    className="search-hit"
                    title={hit.kind === "schemaObject" ? t("search.openDefinition") : t("search.openData")}
                    onClick={() => open(hit)}
                  >
                    <span className="search-hit-name">{hit.kind === "column" ? `${hit.name}.${hit.column}` : hit.name}{hit.detail ? <span className="muted">{hit.detail}</span> : null}</span>
                    <span className="search-hit-meta">{[hit.schema, hit.kind === "column" ? hit.type : t(`search.kinds.${hit.type}`, { defaultValue: hit.type })].filter(Boolean).join(" · ")}</span>
                  </button>
                ))}
              </section>
            ))}
          </div>
        </>
      )}
    </div>
  );
}

interface Outcome {
  schema: string;
  table: string;
  status: TableStatusItem;
}

interface Run {
  id: number;
  connectionId: string;
  running: boolean;
  total: number;
  current: { index: number; schema: string; table: string } | null;
  outcomes: Outcome[];
  summary: DataSearchSummaryItem | null;
  error: string | null;
  cancelling: boolean;
}

function DataSearch({ value, hidden, start }: { value: string; hidden: boolean; start: React.MutableRefObject<(() => void) | null> }) {
  const { t } = useTranslation("shell");
  const schemas = useCatalog((s) => s.schemas);
  const connections = useApp((s) => s.connections);
  const connected = useApp((s) => s.explorerConnected);
  const activeConnection = useApp((s) => s.tabs.find((tab) => tab.id === s.activeTab)?.connectionId ?? null);
  const [limits, setLimits] = useState<DataSearchLimitsItem>(FALLBACK_LIMITS);
  const [connectionId, setConnectionId] = useState("");
  const [chosen, setChosen] = useState<Set<string>>(new Set());
  const [filter, setFilter] = useState("");
  const [caseSensitive, setCaseSensitive] = useState(false);
  const [numbers, setNumbers] = useState(false);
  const [large, setLarge] = useState(false);
  const [rows, setRows] = useState(FALLBACK_LIMITS.defaultRowsPerTable);
  const [run, setRun] = useState<Run | null>(null);
  const current = useRef<number | null>(null);

  useEffect(() => {
    void commands.dataSearchLimits().then((l) => {
      setLimits(l);
      setRows(l.defaultRowsPerTable);
    }).catch(() => undefined);
  }, []);

  const list = useMemo(() => Object.values(schemas), [schemas]);
  const choices = useMemo(
    () => connections.filter((c) => connected.includes(c.id) && list.some((s) => s.connectionId === c.id)),
    [connections, connected, list],
  );
  // The active tab's connection first, then the first one with tables loaded.
  useEffect(() => {
    if (choices.some((c) => c.id === connectionId)) return;
    const next = choices.find((c) => c.id === activeConnection) ?? choices[0];
    setConnectionId(next?.id ?? "");
    setChosen(new Set());
  }, [choices, connectionId, activeConnection]);

  const tables = useMemo(() => searchableTables(list, connectionId), [list, connectionId]);
  const shown = useMemo(() => {
    const f = filter.trim().toLowerCase();
    return f ? tables.filter((x) => `${x.schema}.${x.table}`.toLowerCase().includes(f)) : tables;
  }, [tables, filter]);
  // Tables no longer in the catalog are no longer chosen.
  const chosenTables = tables.filter((x) => chosen.has(tableKey(x.schema, x.table)));

  const running = run?.running ?? false;
  const blocked = !value.trim()
    ? t("search.needValue")
    : chosenTables.length === 0
      ? t("search.needTables")
      : chosenTables.length > limits.maxTables
        ? t("search.tooManyTables", { count: limits.maxTables })
        : null;

  async function search() {
    if (running || blocked) return;
    const id = ++nextSearchId;
    current.current = id;
    const source = connectionId;
    setRun({ id, connectionId: source, running: true, total: chosenTables.length, current: null, outcomes: [], summary: null, error: null, cancelling: false });
    const channel = new Channel<DataSearchEventItem>();
    channel.onmessage = (event) => {
      if (current.current !== id) return;
      setRun((r) => {
        if (!r || r.id !== id) return r;
        if (event.kind === "started") return { ...r, total: event.total, current: { index: event.index, schema: event.schema, table: event.table } };
        return { ...r, outcomes: [...r.outcomes, { schema: event.schema, table: event.table, status: event.status }] };
      });
    };
    try {
      const summary = await call(commands.dataSearch(`meta:${source}`, id, {
        value,
        tables: chosenTables,
        caseSensitive,
        numbers,
        large,
        rowsPerTable: Math.min(Math.max(1, Math.round(rows) || 1), limits.maxRowsPerTable),
      }, channel));
      setRun((r) => (r && r.id === id ? { ...r, running: false, current: null, summary } : r));
    } catch (e) {
      setRun((r) => (r && r.id === id ? { ...r, running: false, current: null, error: errorText(e) } : r));
    }
  }
  start.current = () => void search();

  function cancel() {
    if (!run?.running) return;
    setRun({ ...run, cancelling: true });
    void call(commands.cancelDataSearch(run.id)).catch(() => undefined);
  }

  function toggle(schema: string, table: string) {
    setChosen((previous) => {
      const next = new Set(previous);
      const key = tableKey(schema, table);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  const hits = run?.outcomes.reduce((n, o) => n + (o.status.kind === "searched" ? o.status.hits.length : 0), 0) ?? 0;
  const noColumns = run?.outcomes.filter((o) => o.status.kind === "noColumns").length ?? 0;

  function status(): string | null {
    if (!run) return null;
    if (run.error) return null;
    if (run.running) {
      if (run.cancelling) return t("search.cancelling");
      if (!run.current) return t("search.starting");
      return t("search.progress", { index: run.current.index + 1, total: run.total, table: `${run.current.schema}.${run.current.table}`, count: hits });
    }
    const s = run.summary;
    if (!s) return null;
    if (s.cancelled) return t("search.cancelled", { tables: s.tables, total: run.total, count: s.hits });
    if (s.stopped) return t("search.stopped", { count: s.hits });
    return t("search.done", { count: s.hits, tables: s.tables });
  }

  return (
    <div className="search-body" hidden={hidden}>
      {choices.length === 0 ? (
        <div className="empty small">{t("search.noConnections")}</div>
      ) : (
        <>
          <label className="modal-field search-field">
            <span>{t("search.connection")}</span>
            <select value={connectionId} disabled={running} onChange={(e) => { setConnectionId(e.target.value); setChosen(new Set()); }}>
              {choices.map((c) => <option key={c.id} value={c.id}>{c.name}</option>)}
            </select>
          </label>
          <fieldset className="search-tables" disabled={running}>
            <legend>{t("search.tables")}</legend>
            <div className="search-tables-tools">
              <input type="search" value={filter} placeholder={t("search.filterTables")} aria-label={t("search.filterTables")} onChange={(e) => setFilter(e.target.value)} />
              <button type="button" className="ghost" onClick={() => setChosen((previous) => {
                const next = new Set(previous);
                for (const x of shown) {
                  if (next.size >= limits.maxTables) break;
                  next.add(tableKey(x.schema, x.table));
                }
                return next;
              })}>{t("search.selectShown")}</button>
              <button type="button" className="ghost" onClick={() => setChosen(new Set())}>{t("search.clearTables")}</button>
            </div>
            <div className="search-table-list">
              {tables.length === 0 && <div className="empty small">{t("search.noTables")}</div>}
              {shown.map((x) => (
                <label key={tableKey(x.schema, x.table)} className="check search-table">
                  <input type="checkbox" checked={chosen.has(tableKey(x.schema, x.table))} onChange={() => toggle(x.schema, x.table)} />
                  <span className="search-table-name">{x.table}</span>
                  <span className="muted">{x.schema}</span>
                </label>
              ))}
            </div>
            <div className="muted small">{t("search.chosen", { count: chosenTables.length, total: tables.length })}</div>
          </fieldset>
          <details className="search-options">
            <summary>{t("search.options")}</summary>
            <fieldset disabled={running}>
              <label className="check"><input type="checkbox" checked={caseSensitive} onChange={(e) => setCaseSensitive(e.target.checked)} />{t("search.caseSensitive")}</label>
              <label className="check"><input type="checkbox" checked={numbers} onChange={(e) => setNumbers(e.target.checked)} />{t("search.numbers")}</label>
              <label className="check"><input type="checkbox" checked={large} onChange={(e) => setLarge(e.target.checked)} />{t("search.large")}</label>
              <label className="modal-field search-field">
                <span>{t("search.rowsPerTable")}</span>
                <input type="number" min={1} max={limits.maxRowsPerTable} value={rows} onChange={(e) => setRows(Number(e.target.value))} />
              </label>
            </fieldset>
          </details>
          <div className="search-actions">
            {running ? (
              <button type="button" onClick={cancel} disabled={run?.cancelling}>{t("search.cancel")}</button>
            ) : (
              <button type="button" className="primary" onClick={() => void search()} disabled={blocked !== null} title={blocked ?? undefined}>{t("search.searchData")}</button>
            )}
            {!running && blocked && <span className="muted small">{blocked}</span>}
          </div>
          <div className="muted small search-note">{t("search.readOnly")}</div>
          {run?.error && <div className="row-note" role="alert">{t("search.failed", { detail: run.error })}</div>}
          {status() && <div className="search-status" role="status">{status()}</div>}
          {noColumns > 0 && !run?.running && <div className="muted small search-note">{t("search.noColumns", { count: noColumns })}</div>}
          {run && (
            <div className="search-results">
              {run.outcomes.map((outcome) => <TableHits key={tableKey(outcome.schema, outcome.table)} connectionId={run.connectionId} outcome={outcome} />)}
            </div>
          )}
        </>
      )}
    </div>
  );
}

function TableHits({ connectionId, outcome }: { connectionId: string; outcome: Outcome }) {
  const { t } = useTranslation("shell");
  const name = `${outcome.schema}.${outcome.table}`;
  const { status } = outcome;
  if (status.kind === "failed") return <div className="row-note" role="alert">{t("search.tableFailed", { table: name, detail: status.message })}</div>;
  if (status.kind !== "searched" || status.hits.length === 0) return null;
  const open = (hit: SearchHitItem) => openTableData(connectionId, `meta:${connectionId}`, outcome.schema, outcome.table, hit.filter ?? "");
  return (
    <section aria-label={name}>
      <div className="group-title search-table-title">{name} · {status.hits.length}{status.more ? ` · ${t("search.more")}` : ""}</div>
      {status.hits.map((hit, i) => (
        <button key={i} type="button" className="search-hit" title={hit.filter ? t("search.openRow") : t("search.openTableNoKey")} onClick={() => open(hit)}>
          <span className="search-hit-name">{hit.key.length > 0 ? hit.key.map((k) => `${k.column} = ${k.value}`).join(", ") : t("search.noKey")}</span>
          {hit.matches.map((m) => (
            <span key={m.column} className="search-hit-match">
              <span className="search-hit-column">{m.column}</span>
              <code>{m.text ?? t("search.binaryMatch")}</code>
            </span>
          ))}
        </button>
      ))}
    </section>
  );
}
