import { useTranslation } from "react-i18next";
import { useApp } from "../store";

type IconName = "connections" | "processes" | "history" | "saved" | "settings";

function ActivityIcon({ name }: { name: IconName }) {
  return (
    <svg viewBox="0 0 24 24" width="21" height="21" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {name === "connections" && <><ellipse cx="12" cy="5.5" rx="7" ry="3" /><path d="M5 5.5v6c0 1.7 3.1 3 7 3s7-1.3 7-3v-6M5 11.5v6c0 1.7 3.1 3 7 3s7-1.3 7-3v-6" /></>}
      {name === "processes" && <><path d="M3 12h4l2.4-6 4 12 2.3-6H21" /><rect x="2.5" y="3" width="19" height="18" rx="3" /></>}
      {name === "history" && <><path d="M4 9a8 8 0 1 1 .4 6.8M4 4.5V9h4.5" /><path d="M12 7.5V12l3.5 2" /></>}
      {name === "saved" && <><path d="M6 3.5h12v17l-6-3.5-6 3.5z" /><path d="M9 8h6M9 11h6" /></>}
      {name === "settings" && <><path d="M9.7 3.7h4.6l.5 2a7.8 7.8 0 0 1 1.5.9l2-.6 2.3 4-1.5 1.4a8 8 0 0 1 0 1.7l1.5 1.4-2.3 4-2-.6a7.8 7.8 0 0 1-1.5.9l-.5 2H9.7l-.5-2a7.8 7.8 0 0 1-1.5-.9l-2 .6-2.3-4 1.5-1.4a8 8 0 0 1 0-1.7L3.4 10l2.3-4 2 .6a7.8 7.8 0 0 1 1.5-.9z" /><circle cx="12" cy="12" r="2.5" /></>}
    </svg>
  );
}

export function ActivityBar({ connectionsOpen, processesOpen, onToggleConnections, onToggleProcesses }: { connectionsOpen: boolean; processesOpen: boolean; onToggleConnections(): void; onToggleProcesses(): void }) {
  const { t } = useTranslation(["shell", "settings"]);
  const historyOpen = useApp((s) => s.historyOpen);
  const snippetsOpen = useApp((s) => s.snippetsOpen);
  const settingsActive = useApp((s) => s.settingsOpen);
  return (
    <nav className="activity-bar" aria-label={t("activityBar")}>
      <button className="activity-button" aria-label={t("connections")} title={t("connections")} aria-pressed={connectionsOpen} aria-controls="connections-sidebar" onClick={onToggleConnections}>
        <ActivityIcon name="connections" />
      </button>
      <button className="activity-button" aria-label={t("processMonitor.title")} title={t("processMonitor.title")} aria-pressed={processesOpen} aria-controls="processes-sidebar" onClick={onToggleProcesses}>
        <ActivityIcon name="processes" />
      </button>
      <div className="activity-bottom">
        <button className="activity-button" aria-label={t("queryHistory")} title={t("queryHistory")} aria-pressed={historyOpen} aria-controls="query-history" onClick={() => useApp.getState().setHistoryOpen(!historyOpen)}>
          <ActivityIcon name="history" />
        </button>
        <button className="activity-button" aria-label={t("savedQueries")} title={t("savedQueries")} aria-pressed={snippetsOpen} aria-controls="saved-queries" onClick={() => useApp.getState().setSnippetsOpen(!snippetsOpen)}>
          <ActivityIcon name="saved" />
        </button>
        <button className="activity-button settings-button" aria-label={t("settings:title")} title={t("settings:open")} aria-pressed={settingsActive} onClick={() => useApp.getState().openSettings()}>
          <ActivityIcon name="settings" />
        </button>
      </div>
    </nav>
  );
}
