import { useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ConnectionItem } from "../api";
import { useApp } from "../store";

/** This preference is local and can be changed before connecting to the database. */
export function HistoryPreference({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("editor");
  const busy = useApp((s) => s.historySaving.includes(connection.id));
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const [error, setError] = useState<string | null>(null);

  async function change(enabled: boolean) {
    if (useApp.getState().workspaceChanging || useApp.getState().historySaving.includes(connection.id)) return;
    useApp.getState().setHistorySaving(connection.id, true);
    setError(null);
    try {
      await call(commands.setHistoryEnabled(connection.id, enabled));
      const app = useApp.getState();
      app.setConnections(app.connections.map((c) => (c.id === connection.id ? { ...c, historyEnabled: enabled } : c)));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      useApp.getState().setHistorySaving(connection.id, false);
    }
  }

  return (
    <div className="history-preference">
      <label
        className="check"
        title={t("history.preferenceHint")}
      >
        <input
          type="checkbox"
          checked={connection.historyEnabled}
          disabled={busy || workspaceChanging}
          onChange={(e) => void change(e.target.checked)}
          aria-label={t("history.preferenceLabel", { name: connection.name })}
        />
        {t("history.save")}{busy ? "…" : ""}
      </label>
      {error && <span className="error small" role="alert">{error}</span>}
    </div>
  );
}
