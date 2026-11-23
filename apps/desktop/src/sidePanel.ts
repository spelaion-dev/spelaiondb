// Hiding and showing the side column (SDB-451): one command ("Toggle side panel", Ctrl+B) that
// the panel's own button runs too. What the column shows is `sidebarSection` alone, `null` when it
// is hidden (ADR-060); the panel to bring back is kept next to it (`sidePanelToRestore`).

import { isSidePanel, useApp, type SidePanel } from "./store";

/** Each panel's button in the activity bar, which shows it again. */
const ACTIVITY_COMMAND: Record<SidePanel, string> = {
  connections: "view.explorer",
  processes: "view.processes",
  search: "view.search",
};

/** The side column's panels and its separator: the focus must not stay inside them once hidden. */
const COLUMN = "#connections-sidebar, #processes-sidebar, #search-sidebar, .sidebar-resizer";

/** Hides the open panel, or brings back the one that was open. Hiding it from inside moves the
 *  focus to that panel's button in the activity bar. */
export function toggleSidePanel() {
  const app = useApp.getState();
  const shown = app.sidebarSection;
  const focusInside = isSidePanel(shown) && document.activeElement instanceof Element && document.activeElement.closest(COLUMN) !== null;
  app.toggleSidePanel();
  if (focusInside) {
    document.querySelector<HTMLElement>(`.activity-bar [data-command="${ACTIVITY_COMMAND[shown]}"]`)?.focus();
  }
}
