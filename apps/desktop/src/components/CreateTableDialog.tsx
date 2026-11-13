// The create table form (ADR-047): columns, a primary key, foreign keys and indexes. The core
// writes the script, the dialog shows it as it follows the form, and Create runs exactly that
// script (or it opens in the editor).

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { AlterationPreviewItem, NewTableItem } from "../bindings";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { useApp, useDialogs } from "../store";
import { Icon } from "./Icons";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

type Column = { id: number; name: string; dataType: string; nullable: boolean; default: string; comment: string; primary: boolean };
type ForeignKey = { id: number; name: string; columns: string; referencedTable: string; referencedColumns: string };
type Index = { id: number; name: string; columns: string; unique: boolean };

/** A comma-separated list of names, as typed. */
const names = (text: string) => text.split(",").map((s) => s.trim()).filter(Boolean);

/** A first column in the engine's own types: an integer key. */
const FIRST: Record<string, { key: string; text: string }> = {
  postgres: { key: "bigint", text: "text" },
  mysql: { key: "BIGINT", text: "VARCHAR(255)" },
  mariadb: { key: "BIGINT", text: "VARCHAR(255)" },
  sqlserver: { key: "BIGINT", text: "NVARCHAR(255)" },
  sqlite: { key: "INTEGER", text: "TEXT" },
  duckdb: { key: "BIGINT", text: "VARCHAR" },
};

let nextId = 1;

export function CreateTableDialog({ session, connectionId, schema, onClose, onDone }: {
  session: string;
  connectionId: string;
  schema: string;
  onClose(): void;
  /** After it ran: the explorer reads the schema again. */
  onDone(table: string): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => s.connections.find((c) => c.id === connectionId));
  const types = FIRST[connection?.engine ?? ""] ?? { key: "INTEGER", text: "VARCHAR(255)" };
  const [name, setName] = useState("");
  const [columns, setColumns] = useState<Column[]>(() => [
    { id: nextId++, name: "id", dataType: types.key, nullable: false, default: "", comment: "", primary: true },
    { id: nextId++, name: "name", dataType: types.text, nullable: true, default: "", comment: "", primary: false },
  ]);
  const [keys, setKeys] = useState<ForeignKey[]>([]);
  const [indexes, setIndexes] = useState<Index[]>([]);
  const [preview, setPreview] = useState<{ key: string; value: AlterationPreviewItem } | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const previousFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  useEffect(() => () => { if (previousFocus.current?.isConnected) previousFocus.current.focus(); }, []);

  const table: NewTableItem = useMemo(() => ({
    schema,
    name,
    columns: columns.map((c) => ({ name: c.name, dataType: c.dataType, nullable: c.nullable, default: c.default || null, comment: c.comment || null })),
    primaryKey: columns.filter((c) => c.primary).map((c) => c.name),
    foreignKeys: keys.map((k) => {
      // "schema.table" names a table of another schema.
      const dot = k.referencedTable.indexOf(".");
      const [referencedSchema, referencedTable] = dot > 0 ? [k.referencedTable.slice(0, dot), k.referencedTable.slice(dot + 1)] : [schema, k.referencedTable];
      return { name: k.name, columns: names(k.columns), referencedSchema: referencedSchema.trim(), referencedTable: referencedTable.trim(), referencedColumns: names(k.referencedColumns) };
    }),
    indexes: indexes.map((i) => ({ name: i.name, columns: names(i.columns), unique: i.unique })),
  }), [schema, name, columns, keys, indexes]);
  const key = JSON.stringify(table);

  useEffect(() => {
    let current = true;
    const timer = window.setTimeout(() => {
      call(commands.previewCreateTable(session, JSON.parse(key) as NewTableItem))
        .then((value) => { if (current) { setPreview({ key, value }); setPreviewError(null); } })
        .catch((e: unknown) => { if (current) { setPreview(null); setPreviewError(message(e)); } });
    }, 200);
    return () => { current = false; window.clearTimeout(timer); };
  }, [session, key]);

  const ready = preview !== null && preview.key === key;
  const readOnly = connection?.readOnly ?? false;
  const production = connection?.kind?.safeguards.confirmDataChanges ?? false;
  const comments = connection?.engine !== "sqlite";

  const updateColumn = (id: number, change: Partial<Column>) => setColumns((items) => items.map((c) => (c.id === id ? { ...c, ...change } : c)));
  const updateKey = (id: number, change: Partial<ForeignKey>) => setKeys((items) => items.map((k) => (k.id === id ? { ...k, ...change } : k)));
  const updateIndex = (id: number, change: Partial<Index>) => setIndexes((items) => items.map((i) => (i.id === id ? { ...i, ...change } : i)));

  async function create() {
    if (!ready || !preview) return;
    const shown = preview.value;
    let confirmed = false;
    if (shown.risks.length > 0) {
      const statements = shown.risks.map((r) => ({ sql: shown.statements[r.statement] ?? shown.sql, risk: r.risk }));
      if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return;
      confirmed = true;
    }
    setBusy(true);
    setError(null);
    try {
      await call(commands.applyCreateTable(session, table, shown.sql, confirmed));
      onDone(name.trim());
    } catch (e) {
      setError(t("createTable.failed", { detail: message(e) }));
    } finally {
      setBusy(false);
    }
  }

  function openInEditor() {
    if (!ready || !preview) return;
    const tab = useApp.getState().addTab(connectionId, preview.value.sql);
    connectTabFromExplorer(tab, session, explorerDatabase(connectionId, schema));
    onClose();
  }

  const title = t("createTable.title", { schema });
  return (
    <div className="modal-backdrop">
      <form className="modal structure-dialog create-table-dialog" role="dialog" aria-modal="true" aria-label={title}
        onSubmit={(e) => { e.preventDefault(); if (!readOnly && !busy) void create(); }}
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); } }}>
        <h2>{title}</h2>
        <label className="field">
          <span>{t("createTable.name")}</span>
          <input type="text" className="mono" value={name} autoFocus spellCheck={false} onChange={(e) => setName(e.target.value)} />
        </label>
        <div className="structure-columns" role="group" aria-label={t("structure.columns")}>
          <div className="structure-scroll">
          <table>
            <thead>
              <tr>
                <th scope="col">{t("structure.name")}</th>
                <th scope="col">{t("structure.type")}</th>
                <th scope="col" className="structure-narrow">{t("structure.nullable")}</th>
                <th scope="col" className="structure-narrow">{t("createTable.primaryKey")}</th>
                <th scope="col">{t("structure.default")}</th>
                {comments && <th scope="col">{t("structure.comment")}</th>}
                <th scope="col" className="structure-icon"><span className="sr-only">{t("createTable.actions")}</span></th>
              </tr>
            </thead>
            <tbody>
              {columns.map((c, index) => (
                <tr key={c.id}>
                  <td><input type="text" className="mono" value={c.name} spellCheck={false} aria-label={t("createTable.columnName", { index: index + 1 })} onChange={(e) => updateColumn(c.id, { name: e.target.value })} /></td>
                  <td><input type="text" className="mono" value={c.dataType} spellCheck={false} aria-label={t("structure.typeOf", { name: c.name || index + 1 })} onChange={(e) => updateColumn(c.id, { dataType: e.target.value })} /></td>
                  <td className="structure-check"><input type="checkbox" checked={c.nullable} aria-label={t("structure.nullableOf", { name: c.name || index + 1 })} onChange={(e) => updateColumn(c.id, { nullable: e.target.checked })} /></td>
                  <td className="structure-check"><input type="checkbox" checked={c.primary} aria-label={t("createTable.primaryKeyOf", { name: c.name || index + 1 })} onChange={(e) => updateColumn(c.id, { primary: e.target.checked, ...(e.target.checked ? { nullable: false } : {}) })} /></td>
                  <td><input type="text" className="mono" value={c.default} spellCheck={false} placeholder={t("structure.noDefault")} aria-label={t("structure.defaultOf", { name: c.name || index + 1 })} onChange={(e) => updateColumn(c.id, { default: e.target.value })} /></td>
                  {comments && <td><input type="text" value={c.comment} aria-label={t("structure.commentOf", { name: c.name || index + 1 })} onChange={(e) => updateColumn(c.id, { comment: e.target.value })} /></td>}
                  <td>
                    <button type="button" className="ghost icon-button" disabled={columns.length === 1} title={t("createTable.removeColumn", { name: c.name || index + 1 })} aria-label={t("createTable.removeColumn", { name: c.name || index + 1 })}
                      onClick={() => setColumns((items) => items.filter((item) => item.id !== c.id))}><Icon name="x" /></button>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          </div>
          <div className="structure-add">
            <button type="button" className="ghost" onClick={() => setColumns((items) => [...items, { id: nextId++, name: "", dataType: types.text, nullable: true, default: "", comment: "", primary: false }])}><Icon name="plus" /> {t("createTable.addColumn")}</button>
            <button type="button" className="ghost" onClick={() => setKeys((items) => [...items, { id: nextId++, name: "", columns: "", referencedTable: "", referencedColumns: "" }])}><Icon name="plus" /> {t("createTable.addForeignKey")}</button>
            <button type="button" className="ghost" onClick={() => setIndexes((items) => [...items, { id: nextId++, name: "", columns: "", unique: false }])}><Icon name="plus" /> {t("createTable.addIndex")}</button>
          </div>
          <p className="muted small">{t("structure.sqlHint")}</p>
        </div>
        {keys.length > 0 && (
          <fieldset className="structure-section">
            <legend>{t("createTable.foreignKeys")}</legend>
            {keys.map((k, index) => (
              <div className="structure-row" key={k.id}>
                <input type="text" className="mono" value={k.name} placeholder={t("createTable.automaticName")} aria-label={t("createTable.foreignKeyName", { index: index + 1 })} onChange={(e) => updateKey(k.id, { name: e.target.value })} />
                <input type="text" className="mono" value={k.columns} placeholder={t("createTable.columnsPlaceholder")} aria-label={t("createTable.foreignKeyColumns", { index: index + 1 })} onChange={(e) => updateKey(k.id, { columns: e.target.value })} />
                <input type="text" className="mono" value={k.referencedTable} placeholder={t("createTable.referencedTable")} aria-label={t("createTable.foreignKeyTable", { index: index + 1 })} onChange={(e) => updateKey(k.id, { referencedTable: e.target.value })} />
                <input type="text" className="mono" value={k.referencedColumns} placeholder={t("createTable.referencedColumns")} aria-label={t("createTable.foreignKeyReferenced", { index: index + 1 })} onChange={(e) => updateKey(k.id, { referencedColumns: e.target.value })} />
                <button type="button" className="ghost icon-button" title={t("createTable.remove")} aria-label={t("createTable.removeForeignKey", { index: index + 1 })} onClick={() => setKeys((items) => items.filter((item) => item.id !== k.id))}><Icon name="x" /></button>
              </div>
            ))}
          </fieldset>
        )}
        {indexes.length > 0 && (
          <fieldset className="structure-section">
            <legend>{t("createTable.indexes")}</legend>
            {indexes.map((i, index) => (
              <div className="structure-row" key={i.id}>
                <input type="text" className="mono" value={i.name} placeholder={t("createTable.automaticName")} aria-label={t("createTable.indexName", { index: index + 1 })} onChange={(e) => updateIndex(i.id, { name: e.target.value })} />
                <input type="text" className="mono" value={i.columns} placeholder={t("createTable.columnsPlaceholder")} aria-label={t("createTable.indexColumns", { index: index + 1 })} onChange={(e) => updateIndex(i.id, { columns: e.target.value })} />
                <label className="check"><input type="checkbox" checked={i.unique} onChange={(e) => updateIndex(i.id, { unique: e.target.checked })} /> {t("createTable.unique")}</label>
                <button type="button" className="ghost icon-button" title={t("createTable.remove")} aria-label={t("createTable.removeIndex", { index: index + 1 })} onClick={() => setIndexes((items) => items.filter((item) => item.id !== i.id))}><Icon name="x" /></button>
              </div>
            ))}
          </fieldset>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono structure-sql" aria-label={t("objectChange.sql")} aria-live="polite">
          {ready ? preview.value.sql : previewError ?? "…"}
        </pre>
        {production && <p className="warn small">{t("objectChange.production")}</p>}
        {readOnly && <p className="warn small">{t("objectChange.readOnly")}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <button type="button" onClick={openInEditor} disabled={!ready || busy}>{t("objectChange.openInEditor")}</button>
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={busy}>{t("cancel")}</button>
          <button type="submit" className="primary" disabled={!ready || readOnly || busy}>{busy ? t("structure.applying") : t("createTable.create")}</button>
        </div>
      </form>
    </div>
  );
}
