// Renaming or dropping an object of the explorer (ADR-026): the core writes the SQL, the
// dialog shows it, and it runs only when the user confirms here (or opens it in the editor).

import { useEffect, useRef, useState } from "react";
import { listedConnection, type ConnectionKey, tabKey } from "../connectionKey";
import { useTranslation } from "react-i18next";
import { call, commands, type ObjectChangeItem } from "../api";
import type { ObjectChangePreview } from "../bindings";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { scriptText } from "../dialogState";
import { useApp } from "../store";
import { usePreviewThenApply } from "../useDialogState";

export function ObjectChangeDialog({
  session,
  identity,
  schema,
  title,
  confirmLabel,
  danger = false,
  rename,
  change,
  onClose,
  onDone,
}: {
  session: string;
  identity: ConnectionKey;
  /** The schema the SQL is opened in, if it goes to the editor. */
  schema: string;
  title: string;
  /** The button that runs it: "Rename", "Drop". */
  confirmLabel: string;
  danger?: boolean;
  /** Asks for a new name, starting from the current one. */
  rename?: { current: string };
  /** The change for what the dialog holds now. */
  change(fields: { newName: string; cascade: boolean }): ObjectChangeItem;
  onClose(): void;
  /** After it ran: the tree reads the schema again. */
  onDone(): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => listedConnection(s.connections, identity));
  const [newName, setNewName] = useState(rename?.current ?? "");
  const [cascading, setCascading] = useState(false);
  /** "Cascade" is offered only where the core says the engine has it. */
  const [canCascade, setCanCascade] = useState(false);
  const nameBox = useRef<HTMLInputElement>(null);
  const latest = useRef(change);
  latest.current = change;

  useEffect(() => {
    nameBox.current?.select();
  }, []);

  // The SQL follows what is typed; an answer to an older name is ignored. The last answer stays
  // shown while the next one is asked for.
  const flow = usePreviewThenApply<ObjectChangePreview>(session, JSON.stringify({ newName, cascade: cascading }), 120, (asked) =>
    call(commands.previewObjectChange(session, latest.current(JSON.parse(asked) as { newName: string; cascade: boolean }))));
  const shown = flow.last;
  const sql = shown.kind === "ready" ? shown.value.sql : null;
  const { busy } = flow;
  const error = flow.apply.kind === "failed" ? flow.apply.message : null;
  // What the core said the engine offers stays until it answers again.
  useEffect(() => {
    if (shown.kind === "ready") setCanCascade(shown.value.cascade);
  }, [shown]);

  async function run() {
    await flow.run(async () => {
      await call(commands.applyObjectChange(session, change({ newName, cascade: cascading }), true));
      onDone();
    });
  }

  function openInEditor() {
    if (!sql) return;
    const tab = useApp.getState().addTab(tabKey(identity), sql);
    connectTabFromExplorer(tab, session, explorerDatabase(identity, schema));
    onClose();
  }

  const readOnly = connection?.readOnly ?? false;
  // Decided by the core from the connection's type (ADR-029).
  const production = connection?.kind?.safeguards.confirmDataChanges ?? false;
  return (
    <div className="modal-backdrop">
      <form
        className="modal object-change-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(e) => {
          e.preventDefault();
          if (sql && !readOnly && !busy) void run();
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); }
        }}
      >
        <h2>{title}</h2>
        {rename && (
          <label className="field">
            <span>{t("objectChange.newName")}</span>
            <input
              ref={nameBox}
              type="text"
              name="newName"
              className="mono"
              value={newName}
              autoFocus
              spellCheck={false}
              onChange={(e) => setNewName(e.target.value)}
            />
          </label>
        )}
        {canCascade && (
          <label className="check">
            <input
              type="checkbox"
              name="cascade"
              checked={cascading}
              onChange={(e) => setCascading(e.target.checked)}
            />
            {t("objectChange.cascade")}
          </label>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono" aria-label={t("objectChange.sql")}>
          {scriptText(shown)}
        </pre>
        {production && <p className="warn small">{t("objectChange.production")}</p>}
        {readOnly && <p className="warn small">{t("objectChange.readOnly")}</p>}
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button type="button" onClick={openInEditor} disabled={!sql || busy}>
            {t("objectChange.openInEditor")}
          </button>
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={busy} autoFocus={danger && !rename}>
            {t("cancel")}
          </button>
          <button
            type="submit"
            className={danger ? "danger" : "primary"}
            disabled={!sql || readOnly || busy}
            autoFocus={!rename && !danger}
          >
            {busy ? t("objectChange.running") : confirmLabel}
          </button>
        </div>
      </form>
    </div>
  );
}
