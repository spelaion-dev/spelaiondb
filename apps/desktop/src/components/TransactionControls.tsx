// An editor tab's transaction (ADR-031): the auto-commit switch, the TX badge, Commit and
// Rollback. The core keeps the state and decides what is allowed; this only shows it and
// sends the clicks.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import { useApp, type Tab } from "../store";
import type { TransactionPhaseItem } from "../bindings";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function TransactionControls({ tab, running, onNotice }: {
  tab: Tab;
  running: boolean;
  onNotice(notice: { text: string; error: boolean }): void;
}) {
  const { t } = useTranslation("editor");
  const [busy, setBusy] = useState(false);
  const status = tab.transaction;
  if (!status) return null;
  // The core's state and its answers (ADR-031, ADR-060): nothing here is worked out from flags.
  switch (status.kind) {
    case "notFollowed":
      return <span className="status" title={t("transaction.unsupported")}>{t("transaction.autoCommit")}</span>;
    case "followed":
      break;
    default: {
      const unknown: never = status;
      return unknown;
    }
  }
  const { commit, transaction, smartCommit, inTransaction, autoCommitLocked, endActionsShown } = status;
  const disabled = running || busy;

  async function toggle(on: boolean) {
    setBusy(true);
    try {
      const transaction = await call(commands.setAutoCommit(tab.id, on));
      useApp.getState().updateTab(tab.id, { transaction });
    } catch (e) {
      onNotice({ text: message(e), error: true });
    } finally {
      setBusy(false);
    }
  }

  async function end(commit: boolean) {
    setBusy(true);
    try {
      const outcome = await call(commit ? commands.commitTransaction(tab.id) : commands.rollbackTransaction(tab.id));
      useApp.getState().updateTab(tab.id, { transaction: outcome.status });
      const said = outcome.notices.map((n) => n.message);
      onNotice({
        text: [t(commit ? "transaction.committed" : "transaction.rolledBack"), ...said].join(" "),
        error: outcome.notices.some((n) => n.severity === "WARNING"),
      });
    } catch (e) {
      onNotice({ text: message(e), error: true });
    } finally {
      setBusy(false);
    }
  }

  return (
    <span className="transaction-controls" role="group" aria-label={t("transaction.autoCommit")}>
      <label className="check" title={autoCommitLocked ? t("transaction.autoCommitLocked") : smartCommit ? t("transaction.smartCommitHint") : t("transaction.autoCommitHint")}>
        <input
          type="checkbox"
          checked={commit === "auto"}
          disabled={disabled || autoCommitLocked}
          onChange={(e) => void toggle(e.target.checked)}
        />
        {t("transaction.autoCommit")}
        {smartCommit && <span className="smart">{t("transaction.smart")}</span>}
      </label>
      <TransactionBadge transaction={transaction} />
      {endActionsShown && (
        <>
          <button className="ghost small" disabled={disabled || !inTransaction} onClick={() => void end(true)}>{t("transaction.commit")}</button>
          <button className="ghost small" disabled={disabled || !inTransaction} onClick={() => void end(false)}>{t("transaction.rollback")}</button>
        </>
      )}
    </span>
  );
}

/** The TX badge: what the driver last said, as the core passed it on. */
function TransactionBadge({ transaction }: { transaction: TransactionPhaseItem }) {
  const { t } = useTranslation("editor");
  switch (transaction.kind) {
    case "idle":
      return null;
    case "open":
      return (
        <span className="tx-badge" role="status" title={t("transaction.openHint")}>
          {transaction.statements > 0 ? t("transaction.openCount", { count: transaction.statements }) : t("transaction.open")}
        </span>
      );
    case "failed":
      return <span className="tx-badge tx-badge-failed" role="status" title={t("transaction.failedHint")}>{t("transaction.failed")}</span>;
    default: {
      const unknown: never = transaction;
      return unknown;
    }
  }
}
