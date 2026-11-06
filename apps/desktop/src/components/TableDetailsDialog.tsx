import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type NewIndexKindItem, type ObjectActionsItem, type ObjectTargetItem, type SchemaObjectItem, type TableDetails } from "../api";
import { connectTabFromExplorer, explorerDatabase, EXPLORER_REFRESH_EVENT, openSession } from "../connect";
import { useApp, type Tab } from "../store";
import { ObjectChangeDialog } from "./ObjectChangeDialog";
import { NewTableElementDialog } from "./NewTableElementDialog";

type Row = { cells: string[]; target?: ObjectTargetItem; definition?: string; trigger?: SchemaObjectItem; column?: string };

/** Catalog data is loaded on this tab's own session, independently of the explorer. */
export function TableDetailsView({ tab }: { tab: Tab }) {
  const { t } = useTranslation("shell");
  const view = tab.tableDetails!;
  const { schema, name: table, kind } = view;
  const [details, setDetails] = useState<TableDetails | null>(null);
  const [triggers, setTriggers] = useState<SchemaObjectItem[]>([]);
  const [actions, setActions] = useState<ObjectActionsItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [change, setChange] = useState<{ target: ObjectTargetItem; name: string; rename: boolean } | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [revision, setRevision] = useState(0);
  const [creating, setCreating] = useState(false);
  const [needsRefresh, setNeedsRefresh] = useState(false);
  const refresh = useCallback(() => { setNeedsRefresh(false); setRevision((value) => value + 1); }, []);

  useEffect(() => {
    if (tab.status !== "connected") { setLoading(false); return; }
    if (needsRefresh) return;
    let current = true;
    setLoading(true);
    setError(null);
    Promise.all([
      call(commands.describeTable(tab.id, schema, table, null)),
      call(commands.objectActions(tab.id)),
      kind === "triggers" ? call(commands.listSchemaObjects(tab.id, schema, null, false)) : Promise.resolve([]),
    ]).then(([result, capabilities, objects]) => {
      if (!current) return;
      setDetails(result);
      setActions(capabilities);
      setTriggers(objects.filter((object) => object.kind === "trigger" && object.detail === table));
      setSelected([]);
    }).catch((e: unknown) => { if (current) setError(e instanceof Error ? e.message : String(e)); })
      .finally(() => { if (current) setLoading(false); });
    return () => { current = false; };
  }, [tab.id, tab.status, schema, table, kind, revision, needsRefresh]);

  async function reconnect() {
    if (!tab.connectionId) return;
    // A reconnect never repeats the catalog read; the user chooses when to refresh.
    setNeedsRefresh(true);
    setDetails(null);
    useApp.getState().updateTab(tab.id, { status: "connecting" });
    try {
      const version = await openSession(tab.id, tab.connectionId);
      useApp.getState().updateTab(tab.id, { status: version === null ? "disconnected" : "connected", serverVersion: version, ...(version === null ? {} : { lost: undefined }) });
    } catch (e) {
      useApp.getState().updateTab(tab.id, { status: "disconnected" });
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function openSql(sql: string) {
    if (!tab.connectionId) return;
    const id = useApp.getState().addTab(tab.connectionId, sql);
    connectTabFromExplorer(id, tab.id, explorerDatabase(tab.connectionId, schema));
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
      rows = details.constraints.map((c) => ({ cells: [text(c.name), t(`constraintKinds.${c.kind}`), text(c.columns.join(", ")), text(c.definition)], target: c.name && actions?.dropConstraints ? { kind: "constraint", schema, table, name: c.name, constraint: c.kind } : undefined }));
      break;
    case "foreignKeys":
      headers = [t("tableDetails.name"), t("tableDetails.columns"), t("tableDetails.referencedTable"), t("tableDetails.referencedColumns")];
      rows = details.foreignKeys.map((fk) => ({ cells: [text(fk.name), text(fk.columns.join(", ")), `${fk.referencedSchema}.${fk.referencedTable}`, text(fk.referencedColumns.join(", "))], target: actions?.dropConstraints ? { kind: "foreignKey", schema, table, name: fk.name } : undefined }));
      break;
    case "indexes":
      headers = [t("tableDetails.name"), t("tableDetails.primaryKey"), t("tableDetails.unique"), t("tableDetails.definition")];
      rows = details.indexes.map((index) => ({ cells: [index.name, yesNo(index.primary), yesNo(index.unique), text(index.definition)], definition: index.definition, target: { kind: "index", schema, table, name: index.name, primary: index.primary } }));
      break;
    case "triggers":
      headers = [t("tableDetails.name"), t("tableDetails.comment")];
      rows = triggers.map((trigger) => ({ cells: [trigger.name, text(trigger.comment)], trigger, target: { kind: "schemaObject", object: trigger } }));
      break;
  }
  const title = t(`folders.${kind}`);
  const canCreate = actions !== null && (kind === "constraints" || kind === "foreignKeys" ? actions.addConstraints : kind === "triggers" ? actions.addTriggers : true);
  return (
    <section className="table-details-view" aria-label={t("tableDetails.title", { section: title, table })}>
      <div className="table-details-heading">
        <div><p className="table-details-path">{schema} / {table}</p><h2>{title}</h2></div>
        <div className="table-details-actions">
          <span className="table-details-count">{t("tableDetails.count", { count: rows.length })}</span>
          <button type="button" className="primary" disabled={tab.status !== "connected" || loading || !details || !canCreate}
            title={!canCreate ? t("newTableElement.unsupported") : undefined} onClick={() => setCreating(true)}>{t("newTableElement.add")}</button>
          <button type="button" disabled={tab.status !== "connected" || loading} onClick={refresh}>{t("refresh")}</button>
        </div>
      </div>
      {actions && !canCreate && <p className="muted">{t("newTableElement.unsupported")}</p>}
      {(error || (tab.status === "disconnected" && (tab.lost || tab.run?.error?.message))) && <p className="error" role="alert">{error ?? tab.lost ?? tab.run?.error?.message}</p>}
      {tab.status === "disconnected" && <button type="button" onClick={() => void reconnect()} disabled={!tab.connectionId}>{t("tableDetails.reconnect")}</button>}
      {tab.status === "connected" && needsRefresh && <p className="muted">{t("tableDetails.refreshAfterReconnect")}</p>}
      {tab.status === "connecting" || loading ? <p className="muted">{t("tableDetails.loading")}</p> : null}
      {tab.status === "connected" && details && (
        <>
          {kind === "columns" && selected.length > 0 && actions && (
            <div className="table-details-actions">
              {actions.newIndex.filter((indexKind) => indexKind !== "primaryKey" || details.primaryKey.length === 0).map((indexKind) =>
                <button key={indexKind} type="button" onClick={() => void newIndex(indexKind)}>{t(`newIndex.${indexKind}`, { columns: selected.join(", ") })}</button>)}
            </div>
          )}
          {rows.length === 0 ? <div className="table-details-empty">{t("tableDetails.empty")}</div> : (
            <div className="table-details-scroll" tabIndex={0}>
              <table>
                <thead><tr>{kind === "columns" && <th scope="col">{t("tableDetails.select")}</th>}{headers.map((header) => <th scope="col" key={header}>{header}</th>)}<th scope="col">{t("tableDetails.actions")}</th></tr></thead>
                <tbody>{rows.map((row, index) => <tr key={index}>
                  {kind === "columns" && <td><input type="checkbox" aria-label={t("tableDetails.selectColumn", { name: row.column })} checked={selected.includes(row.column!)} onChange={() => setSelected((items) => items.includes(row.column!) ? items.filter((item) => item !== row.column) : [...items, row.column!])} /></td>}
                  {row.cells.map((cell, column) => <td key={column} title={cell}>{cell}</td>)}
                  <td><div className="table-details-actions">
                    {(row.definition || row.trigger) && <button type="button" onClick={() => void openDefinition(row)}>{t("openDefinition")}</button>}
                    {row.column && row.target && <button type="button" onClick={() => setChange({ target: row.target!, name: row.column!, rename: true })}>{t("objectChange.renameMenu")}</button>}
                    {row.target && <button type="button" className="danger-text" onClick={() => setChange({ target: row.target!, name: row.cells[0] ?? "", rename: false })}>{t("objectChange.dropMenu")}</button>}
                  </div></td>
                </tr>)}</tbody>
              </table>
            </div>
          )}
        </>
      )}
      {change && tab.connectionId && <ObjectChangeDialog
        session={tab.id} connectionId={tab.connectionId} schema={schema}
        title={change.rename ? t("objectChange.renameColumnTitle", { name: change.name, table }) : t("objectChange.dropTitle", { name: change.name })}
        confirmLabel={t(change.rename ? "objectChange.rename" : "objectChange.drop")} danger={!change.rename}
        rename={change.rename ? { current: change.name } : undefined}
        change={({ newName, cascade }) => change.rename ? { action: "rename", target: change.target, newName } : { action: "drop", target: change.target, cascade }}
        onClose={() => setChange(null)} onDone={() => { setChange(null); refresh(); window.dispatchEvent(new CustomEvent(EXPLORER_REFRESH_EVENT, { detail: tab.connectionId })); }}
      />}
      {creating && details && actions && tab.connectionId && <NewTableElementDialog session={tab.id} connectionId={tab.connectionId} schema={schema} table={table} kind={kind}
        details={details} actions={actions} onClose={() => setCreating(false)} onSql={(sql) => { setCreating(false); openSql(sql); }} />}
    </section>
  );
}
