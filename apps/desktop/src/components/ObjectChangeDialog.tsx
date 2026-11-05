// Renaming or dropping an object of the explorer (ADR-026): the core writes the SQL, the
// dialog shows it, and it runs only when the user confirms here (or opens it in the editor).

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ObjectChangeItem } from "../api";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { useApp } from "../store";

export function ObjectChangeDialog({
  session,
  connectionId,
  schema,
  title,
  confirmLabel,
  danger = false,
  rename,
  cascade = false,
  change,
  onClose,
  onDone,
}: {
  session: string;
  connectionId: string;
  /** The schema the SQL is opened in, if it goes to the editor. */
  schema: string;
  title: string;
  /** The button that runs it: "Rename", "Drop". */
  confirmLabel: string;
  danger?: boolean;
  /** Asks for a new name, starting from the current one. */
  rename?: { current: string };
  /** Offers "Cascade" (only where the engine has it). */
  cascade?: boolean;
  /** The change for what the dialog holds now. */
  change(fields: { newName: string; cascade: boolean }): ObjectChangeItem;
  onClose(): void;
  /** After it ran: the tree reads the schema again. */
  onDone(): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => s.connections.find((c) => c.id === connectionId));
  const [newName, setNewName] = useState(rename?.current ?? "");
  const [cascading, setCascading] = useState(false);
  const [sql, setSql] = useState<string | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const nameBox = useRef<HTMLInputElement>(null);
  const latest = useRef(change);
  latest.current = change;

  useEffect(() => {
    nameBox.current?.select();
  }, []);

  // The SQL follows what is typed; an answer to an older name is ignored.
  useEffect(() => {
    let current = true;
    const timer = window.setTimeout(() => {
      call(commands.previewObjectChange(session, latest.current({ newName, cascade: cascading })))
        .then((text) => {
          if (!current) return;
          setSql(text);
          setPreviewError(null);
        })
        .catch((e: unknown) => {
          if (!current) return;
          setSql(null);
          setPreviewError(e instanceof Error ? e.message : String(e));
        });
    }, 120);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [session, newName, cascading]);

  async function run() {
    setBusy(true);
    setError(null);
    try {
      await call(commands.applyObjectChange(session, change({ newName, cascade: cascading }), true));
      onDone();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  function openInEditor() {
    if (!sql) return;
    const tab = useApp.getState().addTab(connectionId, sql);
    connectTabFromExplorer(tab, session, explorerDatabase(connectionId, schema));
    onClose();
  }

  const readOnly = connection?.readOnly ?? false;
  const production = connection?.env?.toLowerCase().startsWith("prod") ?? false;
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
          if (e.key === "Escape" && !busy) onClose();
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
        {cascade && (
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
          {sql ?? previewError ?? "…"}
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
          <button type="button" onClick={onClose} disabled={busy}>
            {t("cancel")}
          </button>
          <button
            type="submit"
            className={danger ? "danger" : "primary"}
            disabled={!sql || readOnly || busy}
            autoFocus={!rename}
          >
            {busy ? t("objectChange.running") : confirmLabel}
          </button>
        </div>
      </form>
    </div>
  );
}
