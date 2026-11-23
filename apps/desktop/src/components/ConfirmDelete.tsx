// The confirmation before deleting something kept by the app (docs/ui-guidelines.md, "Dialogs
// and confirmations", SDB-89): a question that names the action, a body that names the target
// and what is lost, Cancel first and focused so Enter by habit never deletes, Escape cancels,
// and the danger button named by what it does. A failed deletion is said in the dialog, which
// stays open so it can be retried or cancelled.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

/** Where the dialog is: asking, deleting, or asking again after a failure. */
type Phase = { kind: "asking" } | { kind: "deleting" } | { kind: "failed"; message: string };

export function ConfirmDelete(props: {
  /** "Delete network profile?" */
  title: string;
  /** The danger button: "Delete profile". */
  action: string;
  /** The target by name and what is lost. */
  children: React.ReactNode;
  /** Deletes; a rejection is shown in the dialog. The caller closes it once deleted. */
  onConfirm(): Promise<void>;
  onCancel(): void;
  /** The body says why it cannot be deleted now (the core would refuse): only Cancel works. */
  blocked?: boolean;
}) {
  const { t } = useTranslation("shell");
  const [phase, setPhase] = useState<Phase>({ kind: "asking" });
  const cancel = useRef<HTMLButtonElement>(null);
  const deleting = phase.kind === "deleting";
  // Disabling the focused button while deleting sends focus to the body: give it back to Cancel.
  useEffect(() => {
    if (!deleting) cancel.current?.focus();
  }, [deleting]);

  async function confirm() {
    setPhase({ kind: "deleting" });
    try {
      await props.onConfirm();
    } catch (e) {
      setPhase({ kind: "failed", message: e instanceof Error ? e.message : String(e) });
    }
  }

  function failure(): string | null {
    switch (phase.kind) {
      case "asking":
      case "deleting":
        return null;
      case "failed":
        return phase.message;
      default: {
        const unknown: never = phase;
        return unknown;
      }
    }
  }
  const error = failure();

  return (
    <div className="modal-backdrop">
      <div
        className="modal confirm-delete"
        role="dialog"
        aria-modal="true"
        aria-label={props.title}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            if (!deleting) props.onCancel();
          }
        }}
      >
        <h2>{props.title}</h2>
        {props.children}
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} type="button" onClick={props.onCancel} disabled={deleting}>
            {t("cancel")}
          </button>
          <button type="button" className="danger" onClick={() => void confirm()} disabled={deleting || props.blocked}>
            {deleting ? t("deleting") : props.action}
          </button>
        </div>
      </div>
    </div>
  );
}
