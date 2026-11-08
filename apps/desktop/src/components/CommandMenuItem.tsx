// A menu item that is a command of the registry (SDB-18): its name, its shortcut, whether it
// can run and what it does all come from `commandRegistry.ts` and `commands.ts`, the same as in
// the command palette and the global shortcuts.

import { useTranslation } from "react-i18next";
import { commandSpec, type CommandId } from "../commandRegistry";
import { commandEnabled, commandLabel, runCommand, type CommandContext } from "../commands";
import { useApp } from "../store";

export function CommandMenuItem({ id, tab, onClose }: { id: CommandId; tab: string | null; onClose(): void }) {
  const { t } = useTranslation();
  // Whether it can run, and its name (Pin / Unpin), follow the tabs.
  useApp((s) => s.tabs);
  useApp((s) => s.closedTabs.length);
  const context: CommandContext = { tab, source: "menu" };
  const shortcut = commandSpec(id).shortcut;
  return (
    <button
      role="menuitem"
      data-command={id}
      disabled={!commandEnabled(id, context)}
      onClick={() => {
        onClose();
        runCommand(id, context);
      }}
    >
      <span>{t(commandLabel(id, context))}</span>
      <span className="menu-shortcut">{shortcut ?? ""}</span>
    </button>
  );
}
