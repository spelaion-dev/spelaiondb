// Confirmation before deleting a connection: it removes its file (shared through git for
// project connections) and its saved password. Open sessions on it are closed first.

import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { call, commands, type ConnectionItem } from "../api";
import { useApp } from "../store";
import { confirmOpenTransactions } from "../connect";
import { refreshConnections } from "./Sidebar";

export function DeleteConnection({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("connections");
  const close = () => useApp.getState().setDeletingConnection(null);
  const busyTabs = useApp((s) => s.tabs.some((tab) => tab.connectionId === connection.id && (tab.run?.running || tab.status === "connecting")));
  const [deleting, setDeleting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const file = connection.personal
    ? `connections/${connection.id.replace(/^~\//, "")}.toml`
    : `.spelaion/connections/${connection.id}.toml`;

  async function remove() {
    setDeleting(true);
    setError(null);
    try {
      const app = useApp.getState();
      const tabs = app.tabs.filter((tab) => tab.connectionId === connection.id);
      if (!(await confirmOpenTransactions(tabs.map((tab) => tab.id), "disconnect"))) return;
      // Close what uses it before the file disappears: tabs stay open, without a connection.
      await Promise.all(
        [...tabs.map((tab) => tab.id), `meta:${connection.id}`].map((session) =>
          call(commands.disconnect(session)).catch(() => undefined),
        ),
      );
      for (const tab of tabs) app.updateTab(tab.id, { connectionId: null, status: "disconnected", serverVersion: null });
      await call(commands.deleteConnection(connection.id));
      await refreshConnections();
      close();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setDeleting(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-modal="true" aria-label={t("deleteTitle")} onKeyDown={(e) => {
        if (e.key === "Escape") { e.stopPropagation(); if (!deleting) close(); }
      }}>
        <h2>{t("deleteTitle")}</h2>
        <p>
          <Trans t={t} i18nKey="deleteExplanation" values={{ name: connection.name, file }} components={{ strong: <strong />, code: <code /> }} />
        </p>
        <p className="muted small">{connection.personal ? t("deletePersonalHint") : t("deleteProjectHint")}</p>
        {busyTabs && <p className="error small">{t("deleteBusy")}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <button onClick={close} disabled={deleting}>
            {t("cancel")}
          </button>
          <button className="danger" onClick={() => void remove()} disabled={deleting || busyTabs}>
            {deleting ? t("deleting") : t("deleteConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
