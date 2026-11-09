import { useTranslation } from "react-i18next";
import { commandSpec, withShortcut, type CommandId } from "../commandRegistry";
import { activeContext, runCommand } from "../commands";
import { useApp } from "../store";

type IconName = "connections" | "processes" | "search" | "history" | "saved" | "mcp" | "settings";

function ActivityIcon({ name }: { name: IconName }) {
  return (
    <svg viewBox="0 0 24 24" width="21" height="21" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {name === "connections" && <><ellipse cx="12" cy="5.5" rx="7" ry="3" /><path d="M5 5.5v6c0 1.7 3.1 3 7 3s7-1.3 7-3v-6M5 11.5v6c0 1.7 3.1 3 7 3s7-1.3 7-3v-6" /></>}
      {name === "processes" && <><path d="M3 12h4l2.4-6 4 12 2.3-6H21" /><rect x="2.5" y="3" width="19" height="18" rx="3" /></>}
      {name === "search" && <><circle cx="10.5" cy="10.5" r="6.5" /><path d="M15.3 15.3 20.5 20.5" /></>}
      {name === "history" && <><path d="M4 9a8 8 0 1 1 .4 6.8M4 4.5V9h4.5" /><path d="M12 7.5V12l3.5 2" /></>}
      {name === "saved" && <><path d="M6 3.5h12v17l-6-3.5-6 3.5z" /><path d="M9 8h6M9 11h6" /></>}
      {name === "mcp" && <><rect x="6.5" y="6.5" width="11" height="11" rx="1.5" /><rect x="9.5" y="9.5" width="5" height="5" rx="0.5" /><path d="M9.5 3v3.5M14.5 3v3.5M9.5 17.5V21M14.5 17.5V21M3 9.5h3.5M3 14.5h3.5M17.5 9.5H21M17.5 14.5H21" /></>}
      {name === "settings" && <><path d="M9.7 3.7h4.6l.5 2a7.8 7.8 0 0 1 1.5.9l2-.6 2.3 4-1.5 1.4a8 8 0 0 1 0 1.7l1.5 1.4-2.3 4-2-.6a7.8 7.8 0 0 1-1.5.9l-.5 2H9.7l-.5-2a7.8 7.8 0 0 1-1.5-.9l-2 .6-2.3-4 1.5-1.4a8 8 0 0 1 0-1.7L3.4 10l2.3-4 2 .6a7.8 7.8 0 0 1 1.5-.9z" /><circle cx="12" cy="12" r="2.5" /></>}
    </svg>
  );
}

/** One button per view command of the registry (commandRegistry.ts): same name and action as
 *  in the command palette. */
function ActivityButton({ command, icon, pressed, controls, className }: { command: CommandId; icon: IconName; pressed: boolean; controls?: string; className?: string }) {
  const { t } = useTranslation();
  const spec = commandSpec(command);
  const label = t(spec.label);
  return (
    <button
      className={`activity-button${className ? ` ${className}` : ""}`}
      data-command={command}
      aria-label={label}
      title={withShortcut(label, spec)}
      aria-pressed={pressed}
      aria-controls={controls}
      onClick={() => runCommand(command, activeContext("menu"))}
    >
      <ActivityIcon name={icon} />
    </button>
  );
}

export function ActivityBar() {
  const { t } = useTranslation("shell");
  const section = useApp((s) => s.sidebarSection);
  const historyOpen = useApp((s) => s.historyOpen);
  const snippetsOpen = useApp((s) => s.snippetsOpen);
  const settingsActive = useApp((s) => s.settingsOpen);
  return (
    <nav className="activity-bar" aria-label={t("activityBar")}>
      <ActivityButton command="view.explorer" icon="connections" pressed={section === "connections"} controls="connections-sidebar" />
      <ActivityButton command="view.processes" icon="processes" pressed={section === "processes"} controls="processes-sidebar" />
      <ActivityButton command="view.search" icon="search" pressed={section === "search"} controls="search-sidebar" />
      <div className="activity-bottom">
        <ActivityButton command="view.history" icon="history" pressed={historyOpen} controls="query-history" />
        <ActivityButton command="view.savedQueries" icon="saved" pressed={snippetsOpen} controls="saved-queries" />
        <ActivityButton command="view.mcp" icon="mcp" pressed={section === "mcp"} />
        <ActivityButton command="view.settings" icon="settings" pressed={settingsActive} className="settings-button" />
      </div>
    </nav>
  );
}
