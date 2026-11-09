import { useEffect, useRef, useState, type FormEvent } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ObjectActionsItem, type TableDetails } from "../api";
import type { NewTableElementItem } from "../bindings";
import { useApp, type TableDetailKind } from "../store";

/** Common native types; custom types and other sizes can still be entered explicitly. */
const COLUMN_TYPES: Record<string, string[]> = {
  postgres: ["INTEGER", "BIGINT", "SMALLINT", "NUMERIC", "NUMERIC(10,2)", "REAL", "DOUBLE PRECISION", "BOOLEAN", "TEXT", "VARCHAR(255)", "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "UUID", "JSON", "JSONB", "BYTEA"],
  mysql: ["INT", "BIGINT", "SMALLINT", "TINYINT", "DECIMAL(10,2)", "FLOAT", "DOUBLE", "BOOLEAN", "VARCHAR(255)", "TEXT", "LONGTEXT", "DATE", "TIME", "DATETIME", "TIMESTAMP", "BLOB"],
  mariadb: ["INT", "BIGINT", "SMALLINT", "TINYINT", "DECIMAL(10,2)", "FLOAT", "DOUBLE", "BOOLEAN", "VARCHAR(255)", "TEXT", "LONGTEXT", "DATE", "TIME", "DATETIME", "TIMESTAMP", "BLOB"],
  sqlserver: ["INT", "BIGINT", "SMALLINT", "TINYINT", "DECIMAL(10,2)", "FLOAT", "REAL", "BIT", "NVARCHAR(255)", "VARCHAR(255)", "NVARCHAR(MAX)", "DATE", "TIME", "DATETIME2", "DATETIMEOFFSET", "UNIQUEIDENTIFIER", "VARBINARY(MAX)"],
  sqlite: ["INTEGER", "REAL", "TEXT", "BLOB", "NUMERIC"],
  duckdb: ["INTEGER", "BIGINT", "SMALLINT", "TINYINT", "DECIMAL(10,2)", "FLOAT", "DOUBLE", "BOOLEAN", "VARCHAR", "TEXT", "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "UUID", "JSON", "BLOB"],
};
const CUSTOM_TYPE = "__custom__";

/** Collects inputs only; the core writes SQL which opens in the editor for review. */
export function NewTableElementDialog({ session, connectionId, schema, table, kind, details, actions, onClose, onSql }: {
  session: string; connectionId: string; schema: string; table: string; kind: TableDetailKind; details: TableDetails;
  actions: ObjectActionsItem; onClose(): void; onSql(sql: string): void;
}) {
  const { t } = useTranslation("shell");
  const engine = useApp((s) => s.connections.find((connection) => connection.id === connectionId)?.engine);
  const previousFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  useEffect(() => () => { if (previousFocus.current?.isConnected) previousFocus.current.focus(); }, []);
  const [name, setName] = useState("");
  const [dataType, setDataType] = useState("");
  const [customType, setCustomType] = useState("");
  const [nullable, setNullable] = useState(true);
  const [defaultValue, setDefaultValue] = useState("");
  const [constraintKind, setConstraintKind] = useState("unique");
  const [expression, setExpression] = useState("");
  const [columns, setColumns] = useState<string[]>([]);
  const [referencedSchema, setReferencedSchema] = useState(schema);
  const [referencedTable, setReferencedTable] = useState("");
  const [referencedColumns, setReferencedColumns] = useState("");
  const [unique, setUnique] = useState(false);
  const [timing, setTiming] = useState("after");
  const [event, setEvent] = useState("insert");
  const [body, setBody] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const needsColumns = kind === "indexes" || kind === "foreignKeys" || (kind === "constraints" && constraintKind !== "check");

  async function submit(eventObject: FormEvent) {
    eventObject.preventDefault();
    let element: NewTableElementItem;
    switch (kind) {
      case "columns": element = { kind: "column", name, dataType: dataType === CUSTOM_TYPE ? customType : dataType, nullable, default: defaultValue }; break;
      case "constraints": element = { kind: "constraint", name, constraintKind, columns, expression }; break;
      case "foreignKeys": element = { kind: "foreignKey", name, columns, referencedSchema, referencedTable, referencedColumns: referencedColumns.split("\n").map((s) => s.trim()).filter(Boolean) }; break;
      case "indexes": element = { kind: "index", name, columns, unique }; break;
      case "triggers": element = { kind: "trigger", name, timing, event, body }; break;
      // A reference is a foreign key of the other table: created from that table's details.
      case "references": return;
    }
    setBusy(true);
    setError(null);
    try {
      onSql(await call(commands.newTableElementSql(session, schema, table, element)));
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  }

  return <div className="modal-backdrop">
    <form className="modal new-table-element" role="dialog" aria-modal="true" aria-label={t("newTableElement.title", { section: t(`folders.${kind}`), table })}
      onSubmit={(e) => void submit(e)} onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); } }}>
      <header className="new-table-element-header">
        <div>
          <p className="new-table-element-path"><span>{schema}</span><span aria-hidden="true">/</span><span>{table}</span></p>
          <h2>{t("newTableElement.title", { section: t(`folders.${kind}`), table })}</h2>
          <p className="muted small">{t("newTableElement.editorHint")}</p>
        </div>
        <button type="button" className="ghost new-table-element-close" aria-label={t("newTableElement.cancel")} disabled={busy} onClick={onClose}>×</button>
      </header>
      <div className="new-table-element-fields">
        <label className={`field${kind === "columns" ? "" : " new-table-element-wide"}`}><span>{t("newTableElement.name")}</span><input type="text" className="mono" autoFocus required value={name} onChange={(e) => setName(e.target.value)} /></label>
        {kind === "columns" && <>
          <label className="field"><span>{t("newTableElement.dataType")}</span><select required value={dataType} onChange={(e) => setDataType(e.target.value)}>
            <option value="" disabled>{t("newTableElement.chooseType")}</option>
            {(COLUMN_TYPES[engine ?? ""] ?? []).map((type) => <option key={type} value={type}>{type}</option>)}
            <option value={CUSTOM_TYPE}>{t("newTableElement.customType")}</option>
          </select></label>
          {dataType === CUSTOM_TYPE && <label className="field new-table-element-wide"><span>{t("newTableElement.customTypeLabel")}</span><input type="text" className="mono" required value={customType} placeholder="VARCHAR(100)" onChange={(e) => setCustomType(e.target.value)} /></label>}
          <label className="field new-table-element-wide"><span>{t("newTableElement.default")}</span><input type="text" className="mono" value={defaultValue} onChange={(e) => setDefaultValue(e.target.value)} /></label>
          <label className="check new-table-element-toggle new-table-element-wide"><input type="checkbox" checked={nullable} onChange={(e) => setNullable(e.target.checked)} /> {t("newTableElement.nullable")}</label>
        </>}
        {kind === "constraints" && <>
          <label className="field new-table-element-wide"><span>{t("newTableElement.constraintKind")}</span><select value={constraintKind} onChange={(e) => setConstraintKind(e.target.value)}>
            <option value="unique">{t("newTableElement.unique")}</option>
            {details.primaryKey.length === 0 && <option value="primaryKey">{t("tableDetails.primaryKey")}</option>}
            {actions.addCheckConstraints && <option value="check">CHECK</option>}
          </select></label>
          {constraintKind === "check" && <label className="field new-table-element-wide"><span>{t("newTableElement.expression")}</span><input type="text" className="mono" required value={expression} onChange={(e) => setExpression(e.target.value)} /></label>}
        </>}
        {needsColumns && <fieldset className="new-table-element-wide"><legend>{t("newTableElement.columns")}</legend>
          <div className="new-table-element-column-list">
            {details.columns.map((column) => <label key={column.name} className={`new-table-element-column${columns.includes(column.name) ? " selected" : ""}`}>
              <input type="checkbox" checked={columns.includes(column.name)} onChange={() => setColumns((items) => items.includes(column.name) ? items.filter((item) => item !== column.name) : [...items, column.name])} />
              <span className="mono">{column.name}</span>
              {columns.includes(column.name) && <span className="new-table-element-order" aria-hidden="true">{columns.indexOf(column.name) + 1}</span>}
            </label>)}
          </div>
        </fieldset>}
        {kind === "foreignKeys" && <>
          <label className="field"><span>{t("newTableElement.referencedSchema")}</span><input type="text" className="mono" required value={referencedSchema} onChange={(e) => setReferencedSchema(e.target.value)} /></label>
          <label className="field"><span>{t("newTableElement.referencedTable")}</span><input type="text" className="mono" required value={referencedTable} onChange={(e) => setReferencedTable(e.target.value)} /></label>
          <label className="field new-table-element-wide"><span>{t("newTableElement.referencedColumns")}</span><textarea className="mono" required value={referencedColumns} onChange={(e) => setReferencedColumns(e.target.value)} /></label>
        </>}
        {kind === "indexes" && <label className="check new-table-element-toggle new-table-element-wide"><input type="checkbox" checked={unique} onChange={(e) => setUnique(e.target.checked)} /> {t("newTableElement.unique")}</label>}
        {kind === "triggers" && <>
          <label className="field"><span>{t("newTableElement.timing")}</span><select value={timing} onChange={(e) => setTiming(e.target.value)}>{actions.beforeTriggers && <option value="before">BEFORE</option>}<option value="after">AFTER</option></select></label>
          <label className="field"><span>{t("newTableElement.event")}</span><select value={event} onChange={(e) => setEvent(e.target.value)}><option value="insert">INSERT</option><option value="update">UPDATE</option><option value="delete">DELETE</option></select></label>
          <label className="field new-table-element-wide"><span>{t(actions.triggerFunction ? "newTableElement.function" : "newTableElement.body")}</span><textarea className="mono" required value={body} onChange={(e) => setBody(e.target.value)} /></label>
        </>}
      </div>
      {error && <p className="error" role="alert">{error}</p>}
      <div className="modal-actions"><button type="button" disabled={busy} onClick={onClose}>{t("newTableElement.cancel")}</button><button type="submit" className="primary" disabled={busy || (needsColumns && columns.length === 0)}>{t("newTableElement.review")}</button></div>
    </form>
  </div>;
}
