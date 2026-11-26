// Whether a connection's queries go to the local query history: a personal setting kept in the
// local store per workspace and connection, never in the connection's file, so it works the
// same for personal and repository connections. The connection form changes it on Save.

import { call, commands } from "./api";
import { connectionTag, sameConnection, type ConnectionKey } from "./connectionKey";
import i18n from "./i18n";
import { useApp } from "./store";

/**
 * Saves the setting and shows it on the connection. Runs on that connection wait while it
 * changes (`historySaving`), so no statement is recorded, or left out, half-way.
 */
export async function saveHistoryPreference(connection: ConnectionKey, enabled: boolean): Promise<void> {
  const app = useApp.getState();
  const tag = connectionTag(connection);
  if (app.workspaceChanging || app.historySaving.includes(tag)) {
    throw new Error(i18n.t("connections:fields.historyBusy"));
  }
  app.setHistorySaving(tag, true);
  try {
    await call(commands.setHistoryEnabled(connection, enabled));
    const current = useApp.getState();
    current.setConnections(current.connections.map((c) => (sameConnection(c, connection) ? { ...c, historyEnabled: enabled } : c)));
  } finally {
    useApp.getState().setHistorySaving(tag, false);
  }
}
