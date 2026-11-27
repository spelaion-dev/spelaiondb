import { useTranslation } from "react-i18next";
import { commandSpec, type CommandId } from "../commandRegistry";
import { commandTitle } from "../shortcuts";
import { activeContext, runCommand } from "../commands";
import { useApp } from "../store";
import { Icon, type IconName } from "./Icons";

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
      title={commandTitle(label, command)}
      aria-pressed={pressed}
      aria-controls={controls}
      onClick={() => runCommand(command, activeContext("menu"))}
    >
      <Icon name={icon} size={20} />
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
      <ActivityButton command="view.explorer" icon="database" pressed={section === "connections"} controls="connections-sidebar" />
      <ActivityButton command="view.processes" icon="processes" pressed={section === "processes"} controls="processes-sidebar" />
      <ActivityButton command="view.search" icon="search" pressed={section === "search"} controls="search-sidebar" />
      <div className="activity-bottom">
        <ActivityButton command="view.history" icon="history" pressed={historyOpen} controls="query-history" />
        <ActivityButton command="view.savedQueries" icon="saved" pressed={snippetsOpen} controls="saved-queries" />
        <ActivityButton command="view.tasks" icon="tasks" pressed={section === "tasks"} />
        <ActivityButton command="view.mcp" icon="mcp" pressed={section === "mcp"} />
        <ActivityButton command="view.settings" icon="settings" pressed={settingsActive} className="settings-button" />
      </div>
    </nav>
  );
}
