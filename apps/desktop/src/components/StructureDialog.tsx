// Structure editing with forms (ADR-047): the columns of a table changed in a form, or a
// constraint SQLite drops by rebuilding the table. The core writes the script from the
// catalog, the dialog shows it as it follows the form, and Apply runs exactly that script.

import { useEffect, useMemo, useRef, useState } from "react";
import { listedConnection, type ConnectionKey } from "../connectionKey";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { AlterationPreviewItem, ColumnEditItem_Serialize as ColumnEditItem, DroppedConstraintItem, TableAlterationItem, TableStructureItem_Serialize as TableStructureItem } from "../bindings";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { errorText, loadedValue, scriptText } from "../dialogState";
import { useApp } from "../store";
import { confirmShown, usePreviewThenApply } from "../useDialogState";

export function StructureDialog({
  session,
  identity,
  schema,
  table,
  columns: only,
  drop,
  onClose,
  onDone,
}: {
  session: string;
  identity: ConnectionKey;
  schema: string;
  table: string;
  /** The columns to show; all of them when absent. */
  columns?: string[];
  /** A constraint to drop instead of editing columns (SQLite's rebuild). */
  drop?: { constraint: DroppedConstraintItem; label: string };
  onClose(): void;
  /** After it ran: the table's views read the catalog again. */
  onDone(): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => listedConnection(s.connections, identity));
  const [structure, setStructure] = useState<TableStructureItem | null>(null);
  const [edits, setEdits] = useState<ColumnEditItem[]>([]);
  /** The table's comment as typed (SDB-393). */
  const [tableComment, setTableComment] = useState("");
  const [loadError, setLoadError] = useState<string | null>(null);
  const previousFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  useEffect(() => () => { if (previousFocus.current?.isConnected) previousFocus.current.focus(); }, []);

  useEffect(() => {
    let current = true;
    call(commands.tableStructure(session, schema, table))
      .then((value) => {
        if (!current) return;
        setStructure(value);
        setTableComment(value.comment ?? "");
        setEdits(value.columns
          .filter((c) => !only || only.includes(c.name))
          .map((c) => ({ name: c.name, dataType: c.dataType, nullable: c.nullable, default: c.default, comment: c.comment })));
      })
      .catch((e: unknown) => { if (current) setLoadError(errorText(e)); });
    return () => { current = false; };
    // The columns shown are chosen once, when the dialog opens.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session, schema, table]);

  const alteration: TableAlterationItem = useMemo(() => ({
    schema,
    table,
    // Only what differs from the catalog is sent: the core compares again anyway.
    columns: edits.filter((edit) => {
      const before = structure?.columns.find((c) => c.name === edit.name);
      return before && (edit.dataType !== before.dataType || edit.nullable !== before.nullable
        || (edit.default ?? "") !== (before.default ?? "") || (edit.comment ?? "") !== (before.comment ?? ""));
    }),
    drop: drop ? [drop.constraint] : [],
    // The form's value, or the catalog's while it shows none: the core changes it only when it
    // differs from the catalog's.
    comment: structure?.comments && !drop ? (tableComment.trim() === "" ? null : tableComment) : (structure?.comment ?? null),
  }), [schema, table, edits, structure, drop, tableComment]);
  const key = JSON.stringify(alteration);
  const commentChanged = (alteration.comment ?? "").trim() !== (structure?.comment ?? "").trim();
  const changed = alteration.columns.length > 0 || alteration.drop.length > 0 || commentChanged;

  // The SQL follows the form; an answer to an older state is ignored.
  const flow = usePreviewThenApply<AlterationPreviewItem>(session, changed ? key : null, 200, (asked) =>
    call(commands.previewTableAlteration(session, JSON.parse(asked) as TableAlterationItem)));
  const { preview, busy } = flow;
  const ready = preview.kind === "ready";
  const error = flow.apply.kind === "failed" ? flow.apply.message : null;
  const readOnly = connection?.readOnly ?? false;
  // Decided by the core from the connection's type (ADR-029).
  const production = connection?.kind?.safeguards.confirmDataChanges ?? false;

  function update(name: string, change: Partial<ColumnEditItem>) {
    setEdits((items) => items.map((item) => (item.name === name ? { ...item, ...change } : item)));
  }

  async function apply() {
    if (preview.kind !== "ready") return;
    const shown = preview.value;
    const confirmed = await confirmShown(connection?.name ?? "", shown);
    if (confirmed === null) return;
    await flow.run(async () => {
      await call(commands.applyTableAlteration(session, alteration, shown.sql, confirmed));
      onDone();
    }, (detail) => t("structure.failed", { detail }));
  }

  function openInEditor() {
    if (preview.kind !== "ready") return;
    const tab = useApp.getState().addTab(identity, preview.value.sql);
    connectTabFromExplorer(tab, session, explorerDatabase(identity, schema));
    onClose();
  }

  const title = drop
    ? t("structure.dropTitle", { name: drop.label })
    : only?.length === 1 ? t("structure.columnTitle", { name: only[0], table }) : t("structure.title", { table });
  const last = loadedValue(flow.last);
  const transactional = last?.transactional ?? structure?.transactional ?? true;
  const rebuilds = last?.rebuilds ?? structure?.rebuilds ?? false;
  return (
    <div className="modal-backdrop">
      <form
        className="modal structure-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(e) => { e.preventDefault(); if (!readOnly && !busy) void apply(); }}
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); } }}
      >
        <h2>{title}</h2>
        <p className="muted small structure-hint">
          {drop ? t("structure.dropHint") : rebuilds ? t("structure.rebuildHint") : transactional ? t("structure.hint") : t("structure.notTransactional")}
        </p>
        {loadError && <p className="error small" role="alert">{t("structure.loadFailed", { detail: loadError })}</p>}
        {!drop && structure?.comments && (
          <label className="field structure-table-comment">
            {t("structure.tableComment")}
            <input type="text" name="structure-table-comment" value={tableComment} disabled={busy} onChange={(e) => setTableComment(e.target.value)} />
          </label>
        )}
        {!drop && structure && (
          <div className="structure-columns" role="group" aria-label={t("structure.columns")}>
            <div className="structure-scroll">
            <table>
              <thead>
                <tr>
                  <th scope="col">{t("structure.name")}</th>
                  <th scope="col">{t("structure.type")}</th>
                  <th scope="col" className="structure-narrow">{t("structure.nullable")}</th>
                  <th scope="col">{t("structure.default")}</th>
                  {structure.comments && <th scope="col">{t("structure.comment")}</th>}
                </tr>
              </thead>
              <tbody>
                {edits.map((edit, index) => {
                  const before = structure.columns.find((c) => c.name === edit.name);
                  const dirty = alteration.columns.some((c) => c.name === edit.name);
                  const generated = before?.generated ?? false;
                  return (
                    <tr key={edit.name} className={dirty ? "changed" : undefined}>
                      <th scope="row">
                        <span className="mono">{edit.name}</span>
                        {dirty && <span className="structure-changed">{t("structure.changed")}</span>}
                      </th>
                      <td>
                        <input type="text" className="mono" value={edit.dataType} spellCheck={false} autoFocus={index === 0}
                          disabled={generated || busy} title={generated ? t("structure.generated") : undefined}
                          aria-label={t("structure.typeOf", { name: edit.name })}
                          onChange={(e) => update(edit.name, { dataType: e.target.value })} />
                      </td>
                      <td className="structure-check">
                        <input type="checkbox" checked={edit.nullable} disabled={busy}
                          aria-label={t("structure.nullableOf", { name: edit.name })}
                          onChange={(e) => update(edit.name, { nullable: e.target.checked })} />
                      </td>
                      <td>
                        <input type="text" className="mono" value={edit.default ?? ""} spellCheck={false} placeholder={t("structure.noDefault")}
                          disabled={generated || busy} title={generated ? t("structure.generated") : undefined}
                          aria-label={t("structure.defaultOf", { name: edit.name })}
                          onChange={(e) => update(edit.name, { default: e.target.value === "" ? null : e.target.value })} />
                      </td>
                      {structure.comments && (
                        <td>
                          <input type="text" value={edit.comment ?? ""} disabled={busy}
                            aria-label={t("structure.commentOf", { name: edit.name })}
                            onChange={(e) => update(edit.name, { comment: e.target.value === "" ? null : e.target.value })} />
                        </td>
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
            </div>
            <p className="muted small">{t("structure.sqlHint")}</p>
          </div>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono structure-sql" aria-label={t("objectChange.sql")} aria-live="polite">
          {!changed && !drop ? t("structure.nothing") : scriptText(preview)}
        </pre>
        {production && <p className="warn small">{t("objectChange.production")}</p>}
        {readOnly && <p className="warn small">{t("objectChange.readOnly")}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <button type="button" onClick={openInEditor} disabled={!ready || busy}>{t("objectChange.openInEditor")}</button>
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={busy} autoFocus={!!drop}>{t("cancel")}</button>
          <button type="submit" className={drop ? "danger" : "primary"} disabled={!ready || readOnly || busy}>
            {busy ? t("structure.applying") : drop ? t("objectChange.drop") : t("structure.apply")}
          </button>
        </div>
      </form>
    </div>
  );
}
