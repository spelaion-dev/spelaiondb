// "Duplicate table…" (SDB-394): a new name, the schema (this one or another of the connection),
// the rows or not, and the indexes and constraints where the engine's form lets them go. The core
// writes the statements from the catalog and says what the copy keeps and what it does not
// (ADR-026); Run opens them in an editor tab of their own and runs them there as shown, so the
// guard and the connection type's confirmation ask what they ask of any run, and a read-only
// connection refuses them. The rows are copied by the server, never through the app.

import { useEffect, useState } from "react";
import { type ConnectionKey, tabKey } from "../connectionKey";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { DuplicatePlanItem, TablePartItem } from "../bindings";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { useApp } from "../store";

export function DuplicateTableDialog({
  session,
  identity,
  schema,
  name,
  onClose,
}: {
  session: string;
  identity: ConnectionKey;
  schema: string;
  name: string;
  onClose(): void;
}) {
  const { t } = useTranslation("shell");
  const [newName, setNewName] = useState(`${name}_copy`);
  const [newSchema, setNewSchema] = useState(schema);
  const [withData, setWithData] = useState(false);
  const [withIndexes, setWithIndexes] = useState(true);
  const [plan, setPlan] = useState<DuplicatePlanItem | null>(null);
  const [error, setError] = useState<string | null>(null);

  // The statements follow the form; an answer to an older form is ignored.
  useEffect(() => {
    let current = true;
    const timer = window.setTimeout(() => {
      call(commands.duplicateTablePlan(session, { schema, name, newSchema, newName, withData, withIndexes }))
        .then((next) => {
          if (!current) return;
          setPlan(next);
          setError(null);
        })
        .catch((e: unknown) => {
          if (!current) return;
          setPlan(null);
          setError(e instanceof Error ? e.message : String(e));
        });
    }, 200);
    return () => { current = false; window.clearTimeout(timer); };
  }, [session, schema, name, newSchema, newName, withData, withIndexes]);

  /** A new tab on the connection with the statements; `run` runs them once connected. */
  function openTab(run: boolean) {
    if (!plan) return;
    const app = useApp.getState();
    const tab = app.addTab(tabKey(identity), plan.sql);
    if (run) app.updateTab(tab, { runOnOpen: true });
    connectTabFromExplorer(tab, session, explorerDatabase(identity, newSchema));
    onClose();
  }

  const parts = (list: TablePartItem[]) => list.map((part) => t(`duplicate.parts.${part}`)).join(", ");
  const title = t("duplicate.title", { name });
  return (
    <div className="modal-backdrop">
      <form
        className="modal object-change-dialog duplicate-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(e) => {
          e.preventDefault();
          openTab(true);
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") { e.stopPropagation(); onClose(); }
        }}
      >
        <h2>{title}</h2>
        <div className="form-pair">
          <label className="field grow">
            <span>{t("duplicate.newName")}</span>
            <input type="text" name="newName" className="mono" value={newName} autoFocus spellCheck={false} onFocus={(e) => e.currentTarget.select()} onChange={(e) => setNewName(e.target.value)} />
          </label>
          <label className="field">
            <span>{t("duplicate.schema")}</span>
            <input type="text" name="newSchema" className="mono" value={newSchema} spellCheck={false} onChange={(e) => setNewSchema(e.target.value)} />
          </label>
        </div>
        <label className="check">
          <input type="checkbox" name="withData" checked={withData} onChange={(e) => setWithData(e.target.checked)} />
          <span>{t("duplicate.withData")}</span>
        </label>
        {plan?.indexesOptional && (
          <label className="check">
            <input type="checkbox" name="withIndexes" checked={withIndexes} onChange={(e) => setWithIndexes(e.target.checked)} />
            <span>{t("duplicate.withIndexes")}</span>
          </label>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono" aria-label={t("objectChange.sql")}>
          {plan?.sql ?? (error ? "" : "…")}
        </pre>
        {plan && (
          <>
            <p className="small" role="status">{t("duplicate.kept", { parts: parts(plan.kept) })}</p>
            {plan.notKept.length > 0 && <p className="warn small">{t("duplicate.notKept", { parts: parts(plan.notKept) })}</p>}
          </>
        )}
        <p className="muted small">{t("duplicate.runsInTab")}</p>
        {error && (
          <p className="error small" role="alert">
            {t("duplicate.failed", { detail: error })}
          </p>
        )}
        <div className="modal-actions">
          <button type="button" onClick={() => openTab(false)} disabled={!plan}>
            {t("objectChange.openInEditor")}
          </button>
          <span className="spacer" />
          <button type="button" onClick={onClose}>
            {t("cancel")}
          </button>
          <button type="submit" className="primary" disabled={!plan}>
            {t("duplicate.run")}
          </button>
        </div>
      </form>
    </div>
  );
}
