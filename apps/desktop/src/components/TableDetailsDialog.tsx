import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type DependenciesItem, type NewIndexKindItem, type ObjectActionsItem, type ObjectPropertiesItem, type ObjectTargetItem, type PartitioningItem, type SchemaObjectItem, type TableDetails, type TableKeyItem } from "../api";
import { connectTabFromExplorer, explorerDatabase, openSession, openTableData, refreshExplorer } from "../connect";
import { connectionKey } from "../connectionKey";
import { useApp, type Tab } from "../store";
import { CONNECTING, DISCONNECTED, connectFailure, connectedWith, lostSession, statusName } from "../tabState";
import { ObjectChangeDialog } from "./ObjectChangeDialog";
import { NewTableElementDialog } from "./NewTableElementDialog";
import { StructureDialog } from "./StructureDialog";
import { formatBytes } from "./ServerObservation";
import type { DroppedConstraintItem, TableStatsItem_Serialize as TableStatsItem } from "../bindings";

type Row = { cells: string[]; target?: ObjectTargetItem; rebuildDrop?: DroppedConstraintItem; definition?: string; trigger?: SchemaObjectItem; column?: string; data?: { schema: string; table: string } };

/** Catalog data is loaded on this tab's own session, independently of the explorer. */
export function TableDetailsView({ tab }: { tab: Tab }) {
  const status = statusName(tab);
  /** Why it has no session: lost, or the last attempt to connect failed. */
  const disconnectedWhy = lostSession(tab)?.reason ?? connectFailure(tab);
  const { t, i18n } = useTranslation("shell");
  const view = tab.tableDetails!;
  const { schema, name: table, kind } = view;
  const [details, setDetails] = useState<TableDetails | null>(null);
  const [triggers, setTriggers] = useState<SchemaObjectItem[]>([]);
  /** The foreign keys of other tables that point to this one (the "References" tab). */
  const [references, setReferences] = useState<TableKeyItem[]>([]);
  /** The "Properties", "Partitions" and "Dependencies" tabs (SDB-393): `null` partitions or
   * dependencies say the engine does not tell, which is never shown as an empty list. */
  const [properties, setProperties] = useState<ObjectPropertiesItem | null>(null);
  const [partitioning, setPartitioning] = useState<PartitioningItem | null>(null);
  const [dependencies, setDependencies] = useState<DependenciesItem | null>(null);
  const [actions, setActions] = useState<ObjectActionsItem | null>(null);
  /** Size on disk, index size and row estimate from the catalog's statistics (`null`: none). */
  const [stats, setStats] = useState<{ value: TableStatsItem | null } | { error: string } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [change, setChange] = useState<{ target: ObjectTargetItem; name: string; rename: boolean } | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [revision, setRevision] = useState(0);
  const [creating, setCreating] = useState(false);
  /** The structure form: columns to edit, or a constraint SQLite drops by a rebuild (ADR-047). */
  const [editing, setEditing] = useState<{ columns?: string[]; drop?: { constraint: DroppedConstraintItem; label: string } } | null>(null);
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const refresh = useCallback(() => { setNeedsRefresh(false); setRevision((value) => value + 1); }, []);

  useEffect(() => {
    if (status !== "connected") { setLoading(false); return; }
    if (needsRefresh) return;
    let current = true;
    setLoading(true);
    setError(null);
    Promise.all([
      call(commands.describeTable(tab.id, schema, table, null)),
      call(commands.objectActions(tab.id)),
      kind === "triggers" ? call(commands.listSchemaObjects(tab.id, schema, null, false)) : Promise.resolve([]),
      kind === "references" ? call(commands.referencingKeys(tab.id, schema, table)) : Promise.resolve([]),
      kind === "properties" ? call(commands.objectProperties(tab.id, schema, table)) : Promise.resolve(null),
      kind === "partitions" ? call(commands.tablePartitions(tab.id, schema, table)) : Promise.resolve(null),
      kind === "dependencies" ? call(commands.objectDependencies(tab.id, schema, table)) : Promise.resolve(null),
      // The statistics are extra: when they cannot be read, the rest of the tab still shows.
      call(commands.tableStats(tab.id, schema, table)).then((value) => ({ value }), (e: unknown) => ({ error: e instanceof Error ? e.message : String(e) })),
    ]).then(([result, capabilities, objects, referencing, props, partitions, uses, statistics]) => {
      if (!current) return;
      setProperties(props);
      setPartitioning(partitions);
      setDependencies(uses);
      setStats(statistics);
      setDetails(result);
      setActions(capabilities);
      setTriggers(objects.filter((object) => object.kind === "trigger" && object.detail === table));
      setReferences(referencing);
      setSelected([]);
    }).catch((e: unknown) => { if (current) setError(e instanceof Error ? e.message : String(e)); })
      .finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [tab.id, status, schema, table, kind, revision, needsRefresh]);

  async function reconnect() {
    if (!tab.connectionId) return;
    // A reconnect never repeats the catalog read; the user chooses when to refresh.
    setNeedsRefresh(true);
    setDetails(null);
    useApp.getState().setConnection(tab.id, CONNECTING);
    try {
      const session = await openSession(tab.id, connectionKey(tab.connectionId));
      useApp.getState().setConnection(tab.id, session === null ? DISCONNECTED : connectedWith(session));
    } catch (e) {
      useApp.getState().setConnection(tab.id, DISCONNECTED);
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function openSql(sql: string) {
    if (!tab.connectionId) return;
    const id = useApp.getState().addTab(tab.connectionId, sql);
    connectTabFromExplorer(id, tab.id, explorerDatabase(connectionKey(tab.connectionId), schema));
  }

  async function newIndex(indexKind: NewIndexKindItem) {
    try {
      setError(null);
      openSql(await call(commands.newIndexSql(tab.id, schema, table, selected, indexKind)));
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
  }

  async function openDefinition(row: Row) {
    try {
      setError(null);
      if (row.trigger) openSql(await call(commands.objectDefinition(tab.id, row.trigger)));
      else if (row.definition) openSql(row.definition.startsWith("--") ? row.definition : `${row.definition.trimEnd().replace(/;$/, "")};`);
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
  }

  const text = (value: string | null | undefined) => value == null || value === "" ? "—" : value;
  const yesNo = (value: boolean) => t(value ? "tableDetails.yes" : "tableDetails.no");
  let headers: string[] = [];
  let rows: Row[] = [];
  if (details) switch (kind) {
    case "columns":
      headers = [t("tableDetails.name"), t("tableDetails.type"), t("tableDetails.nullable"), t("tableDetails.primaryKey"), t("tableDetails.default"), t("tableDetails.comment")];
      rows = details.columns.map((c) => ({ cells: [c.name, c.nativeType, yesNo(c.nullable), yesNo(details.primaryKey.includes(c.name)), text(c.default), text(c.comment)], column: c.name, target: { kind: "column", schema, table, name: c.name } }));
      break;
    case "constraints":
      headers = [t("tableDetails.name"), t("tableDetails.type"), t("tableDetails.columns"), t("tableDetails.definition")];
      rows = details.constraints.map((c) => ({ cells: [text(c.name), t(`constraintKinds.${c.kind}`), text(c.columns.join(", ")), text(c.definition)], target: c.name && actions?.dropConstraints ? { kind: "constraint", schema, table, name: c.name, constraint: c.kind } : undefined,
        rebuildDrop: c.name && c.kind === "unique" && actions?.rebuildDrops ? { kind: "constraint", name: c.name } : undefined }));
      break;
    case "foreignKeys":
      headers = [t("tableDetails.name"), t("tableDetails.columns"), t("tableDetails.referencedTable"), t("tableDetails.referencedColumns")];
      rows = details.foreignKeys.map((fk) => ({ cells: [text(fk.name), text(fk.columns.join(", ")), `${fk.referencedSchema}.${fk.referencedTable}`, text(fk.referencedColumns.join(", "))], target: actions?.dropConstraints ? { kind: "foreignKey", schema, table, name: fk.name } : undefined,
        rebuildDrop: actions?.rebuildDrops ? { kind: "foreignKey", name: fk.name } : undefined }));
      break;
    case "references":
      headers = [t("tableDetails.name"), t("tableDetails.referencingTable"), t("tableDetails.columns"), t("tableDetails.referencedColumns")];
      rows = references.map((key) => ({ cells: [text(key.name), `${key.schema}.${key.table}`, text(key.otherColumns.join(", ")), text(key.columns.join(", "))], data: { schema: key.schema, table: key.table } }));
      break;
    case "indexes":
      headers = [t("tableDetails.name"), t("tableDetails.primaryKey"), t("tableDetails.unique"), t("tableDetails.definition")];
      rows = details.indexes.map((index) => ({ cells: [index.name, yesNo(index.primary), yesNo(index.unique), text(index.definition)], definition: index.definition, target: { kind: "index", schema, table, name: index.name, primary: index.primary } }));
      break;
    case "triggers":
      headers = [t("tableDetails.name"), t("tableDetails.comment")];
      rows = triggers.map((trigger) => ({ cells: [trigger.name, text(trigger.comment)], trigger, target: { kind: "schemaObject", object: trigger } }));
      break;
    case "properties":
      headers = [t("tableDetails.property"), t("tableDetails.value")];
      rows = [
        { cells: [t("properties.comment"), text(properties?.comment)] },
        ...(properties?.properties ?? []).map((p) => ({ cells: [t(`properties.${p.name}`, { defaultValue: p.name }), p.value] })),
      ];
      break;
    case "partitions":
      headers = [t("tableDetails.name"), t("tableDetails.bound"), t("tableDetails.rows"), t("tableDetails.subpartitioned")];
      rows = (partitioning?.partitions ?? []).map((p) => ({
        cells: [p.schema === schema ? p.name : `${p.schema}.${p.name}`, p.bound ?? t("tableDetails.defaultPartition"),
          p.estimatedRows == null ? "—" : new Intl.NumberFormat(i18n.language).format(p.estimatedRows), yesNo(p.subpartitioned)],
      }));
      break;
    case "dependencies": {
      headers = [t("tableDetails.direction"), t("tableDetails.kind"), t("tableDetails.name"), t("tableDetails.detail")];
      const row = (direction: "uses" | "usedBy", d: DependenciesItem["uses"][number]): Row => ({
        cells: [t(`tableDetails.${direction}`), t(`dependencyKinds.${d.kind}`), d.schema === schema ? d.name : `${d.schema}.${d.name}`, text(d.detail)],
        // A table or view on the other side opens with its data.
        data: d.kind === "table" || d.kind === "view" || d.kind === "materializedView" ? { schema: d.schema, table: d.name } : undefined,
      });
      rows = dependencies ? [...dependencies.uses.map((d) => row("uses", d)), ...dependencies.usedBy.map((d) => row("usedBy", d))] : [];
      break;
    }
  }
  const title = t(`folders.${kind}`);
  // References are created as a foreign key of the other table, from its own details; what the
  // catalog says of a table (SDB-393) is not created here.
  const creates = kind !== "references" && kind !== "properties" && kind !== "partitions" && kind !== "dependencies";
  const canCreate = actions !== null && creates && (kind === "constraints" || kind === "foreignKeys" ? actions.addConstraints : kind === "triggers" ? actions.addTriggers : true);
  return (
    <section className="table-details-view" aria-label={t("tableDetails.title", { section: title, table })}>
      <div className="table-details-heading">
        <div><p className="table-details-path">{schema} / {table}</p><h2>{title}</h2>
          {stats && "value" in stats && stats.value && <dl className="table-stats" aria-label={t("tableStats.title")}>
            {stats.value.totalBytes !== null && <div><dt>{t("tableStats.total")}</dt><dd>{formatBytes(stats.value.totalBytes, i18n.language)}</dd></div>}
            {stats.value.tableBytes !== null && <div><dt>{t("tableStats.table")}</dt><dd>{formatBytes(stats.value.tableBytes, i18n.language)}</dd></div>}
            {stats.value.indexBytes !== null && <div><dt>{t("tableStats.indexes")}</dt><dd>{formatBytes(stats.value.indexBytes, i18n.language)}</dd></div>}
            {stats.value.rowEstimate !== null && <div><dt>{t("tableStats.rows")}</dt><dd title={t("tableStats.estimateHint")}>{t("tableStats.rowsValue", { rows: new Intl.NumberFormat(i18n.language).format(stats.value.rowEstimate) })}</dd></div>}
          </dl>}
          {stats && "error" in stats && <p className="muted small">{t("tableStats.failed", { detail: stats.error })}</p>}
        </div>
        <div className="table-details-actions">
          <span className="table-details-count">{t("tableDetails.count", { count: rows.length })}</span>
          {creates && <button type="button" className="primary" disabled={status !== "connected" || loading || !details || !canCreate}
            title={!canCreate ? t("newTableElement.unsupported") : undefined} onClick={() => setCreating(true)}>{t("newTableElement.add")}</button>}
          {kind === "columns" && <button type="button" disabled={status !== "connected" || loading || !details}
            onClick={() => setEditing({ columns: selected.length > 0 ? selected : undefined })}>{t(selected.length > 0 ? "structure.editSelected" : "structure.editColumns", { count: selected.length })}</button>}
          {kind === "properties" && <button type="button" disabled={status !== "connected" || loading || !details}
            onClick={() => setEditing({})}>{t("structure.editComment")}</button>}
          <button type="button" disabled={status !== "connected" || loading} onClick={refresh}>{t("refresh")}</button>
        </div>
      </div>
      {actions && creates && !canCreate && <p className="muted">{t("newTableElement.unsupported")}</p>}
      {kind === "references" && <p className="muted">{t("tableDetails.referencesHint", { table })}</p>}
      {kind === "partitions" && details && !loading && (partitioning
        ? <p className="muted">{t("tableDetails.partitionedBy", { method: partitioning.method, key: partitioning.key })}</p>
        : <p className="muted" role="status">{t("tableDetails.notPartitioned")}</p>)}
      {kind === "dependencies" && details && !loading && !dependencies && <p className="muted" role="status">{t("tableDetails.dependenciesUnknown")}</p>}
      {kind === "dependencies" && dependencies && dependencies.untracked.length > 0 && <p className="muted small">{t("tableDetails.untracked", {
        kinds: dependencies.untracked.map((k) => t(`dependencyKinds.${k}`).toLowerCase()).join(", "),
      })}</p>}
      {(error || disconnectedWhy) && <p className="error" role="alert">{error ?? disconnectedWhy}</p>}
      {status === "disconnected" && <button type="button" onClick={() => void reconnect()} disabled={!tab.connectionId}>{t("tableDetails.reconnect")}</button>}
      {status === "connected" && needsRefresh && <p className="muted">{t("tableDetails.refreshAfterReconnect")}</p>}
      {status === "connecting" || loading ? <p className="muted">{t("tableDetails.loading")}</p> : null}
      {status === "connected" && details && (
        <>
          {kind === "columns" && selected.length > 0 && actions && (
            <div className="table-details-actions">
              {actions.newIndex.filter((indexKind) => indexKind !== "primaryKey" || details.primaryKey.length === 0).map((indexKind) =>
                <button key={indexKind} type="button" onClick={() => void newIndex(indexKind)}>{t(`newIndex.${indexKind}`, { columns: selected.join(", ") })}</button>)}
            </div>
          )}
          {(kind === "partitions" && !partitioning) || (kind === "dependencies" && !dependencies) ? null : rows.length === 0 ? <div className="table-details-empty">{t("tableDetails.empty")}</div> : (
            <div className="table-details-scroll" tabIndex={0}>
              <table>
                <thead><tr>{kind === "columns" && <th scope="col">{t("tableDetails.select")}</th>}{headers.map((header) => <th scope="col" key={header}>{header}</th>)}<th scope="col">{t("tableDetails.actions")}</th></tr></thead>
                <tbody>{rows.map((row, index) => <tr key={index}>
                  {kind === "columns" && <td><input type="checkbox" aria-label={t("tableDetails.selectColumn", { name: row.column })} checked={selected.includes(row.column!)} onChange={() => setSelected((items) => items.includes(row.column!) ? items.filter((item) => item !== row.column) : [...items, row.column!])} /></td>}
                  {row.cells.map((cell, column) => <td key={column} title={cell}>{cell}</td>)}
                  <td><div className="table-details-actions table-row-actions">
                    {(row.definition || row.trigger) && <button type="button" className="ghost" onClick={() => void openDefinition(row)}>{t("openDefinition")}</button>}
                    {row.data && tab.connectionId && <button type="button" className="ghost" onClick={() => tab.connectionId && row.data && openTableData(tab.connectionId, tab.id, row.data.schema, row.data.table)}>{t("tableDetails.openData")}</button>}
                    {row.column && row.target && <button type="button" className="ghost" onClick={() => setChange({ target: row.target!, name: row.column!, rename: true })}>{t("objectChange.renameMenu")}</button>}
                    {row.target && <button type="button" className="ghost table-action-drop" onClick={() => setChange({ target: row.target!, name: row.cells[0] ?? "", rename: false })}>{t("objectChange.dropMenu")}</button>}
                    {!row.target && row.rebuildDrop && <button type="button" className="ghost table-action-drop" onClick={() => setEditing({ drop: { constraint: row.rebuildDrop!, label: row.cells[0] ?? "" } })}>{t("objectChange.dropMenu")}</button>}
                  </div></td>
                </tr>)}</tbody>
              </table>
            </div>
          )}
        </>
      )}
      {change && tab.connectionId && <ObjectChangeDialog
        session={tab.id} identity={connectionKey(tab.connectionId)} schema={schema}
        title={change.rename ? t("objectChange.renameColumnTitle", { name: change.name, table }) : t("objectChange.dropTitle", { name: change.name })}
        confirmLabel={t(change.rename ? "objectChange.rename" : "objectChange.drop")} danger={!change.rename}
        rename={change.rename ? { current: change.name } : undefined}
        change={({ newName, cascade }) => change.rename ? { action: "rename", target: change.target, newName } : { action: "drop", target: change.target, cascade }}
        onClose={() => setChange(null)} onDone={() => { setChange(null); refresh(); if (tab.connectionId) refreshExplorer(connectionKey(tab.connectionId)); }}
      />}
      {editing && tab.connectionId && <StructureDialog session={tab.id} identity={connectionKey(tab.connectionId)} schema={schema} table={table}
        columns={editing.columns} drop={editing.drop} onClose={() => setEditing(null)}
        onDone={() => { setEditing(null); refresh(); if (tab.connectionId) refreshExplorer(connectionKey(tab.connectionId)); }} />}
      {creating && details && actions && tab.connectionId && <NewTableElementDialog session={tab.id} identity={connectionKey(tab.connectionId)} schema={schema} table={table} kind={kind}
        details={details} actions={actions} onClose={() => setCreating(false)} onSql={(sql) => { setCreating(false); openSql(sql); }} />}
    </section>
  );
}
