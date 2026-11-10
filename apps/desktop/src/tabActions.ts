// What the tab commands do; their names and shortcuts are in `commandRegistry.ts`. Closing goes
// through `closeTabs`, which asks once for every tab with unsaved edits, an open transaction or
// a running query.
//
// Pinned tabs: "close others", "close to the right" and "close all" leave them open, and their
// × is replaced by an unpin button, so a stray click never closes one. Closing that tab itself
// (Ctrl+W, or "Close" in its menu) still closes it: the user named that tab.

import i18n from "./i18n";
import { closeTabs } from "./connect";
import { useApp } from "./store";

function unpinned(ids: string[]): string[] {
  const tabs = useApp.getState().tabs;
  return ids.filter((id) => !tabs.find((t) => t.id === id)?.pinned);
}

export function closeOtherTabs(id: string) {
  const tabs = useApp.getState().tabs;
  return closeTabs(unpinned(tabs.filter((t) => t.id !== id).map((t) => t.id)));
}

export function closeTabsToTheRight(id: string) {
  const tabs = useApp.getState().tabs;
  const at = tabs.findIndex((t) => t.id === id);
  if (at < 0) return Promise.resolve();
  return closeTabs(unpinned(tabs.slice(at + 1).map((t) => t.id)));
}

export function closeAllTabs() {
  return closeTabs(unpinned(useApp.getState().tabs.map((t) => t.id)));
}

/** Whether "close others" / "close to the right" / "close all" would close anything. */
export function closable(id: string): { others: boolean; right: boolean; all: boolean } {
  const tabs = useApp.getState().tabs;
  const at = tabs.findIndex((t) => t.id === id);
  return {
    others: tabs.some((t) => t.id !== id && !t.pinned),
    right: at >= 0 && tabs.slice(at + 1).some((t) => !t.pinned),
    all: tabs.some((t) => !t.pinned),
  };
}

/** A copy of the tab next to it; a renamed tab's copy says it is one. */
export function duplicateTab(id: string) {
  const app = useApp.getState();
  const source = app.tabs.find((t) => t.id === id);
  if (!source) return;
  const copy = app.duplicateTab(id);
  if (copy && source.title) app.renameTab(copy, i18n.t("app:tabs.copyOf", { name: source.title }));
}

export function togglePinned(id: string) {
  const app = useApp.getState();
  const tab = app.tabs.find((t) => t.id === id);
  if (tab) app.setPinned(id, !tab.pinned);
}

export function startRename(id: string) {
  const app = useApp.getState();
  if (!app.tabs.some((t) => t.id === id)) return;
  app.setActiveTab(id);
  app.setRenamingTab(id);
}

/** Moves the tab one place left or right (Ctrl+Shift+PageUp / PageDown), inside its group. */
export function moveTabBy(id: string, delta: number) {
  const app = useApp.getState();
  const at = app.tabs.findIndex((t) => t.id === id);
  if (at >= 0) app.moveTab(id, at + delta);
}

/** Only SQL editor tabs have an editor to hide. */
export function toggleResultsMaximized(id: string) {
  const app = useApp.getState();
  const tab = app.tabs.find((t) => t.id === id);
  if (tab && !tab.table && !tab.tableDetails && !tab.plan && !tab.diagram) app.setResultsMaximized(id, !tab.resultsMaximized);
}
