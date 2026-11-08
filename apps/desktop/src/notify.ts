// A system notification when a long run finishes in the background (ADR-035). Opt-in, with a
// threshold, in Settings › Scripts. It names the tab and whether something failed; it never
// carries SQL, data or error text.

import { call, commands, type ScriptSummaryItem } from "./api";
import { tabLabel } from "./connect";
import i18n from "./i18n";
import { usePreferences } from "./preferences";
import { useApp, type Tab } from "./store";

/** The app is not what the user is looking at: another window has the focus, it is
 *  minimised, or another tab is in front. */
function inBackground(tab: Tab): boolean {
  return document.hidden || !document.hasFocus() || useApp.getState().activeTab !== tab.id;
}

export function notifyFinished(tab: Tab, summary: ScriptSummaryItem, elapsedMs: number): void {
  const { notify, notifyAfterSeconds } = usePreferences.getState().script;
  if (!notify || elapsedMs < notifyAfterSeconds * 1000 || !inBackground(tab)) return;
  // The tab's current name: the user may have renamed it while the script ran.
  const current = useApp.getState().tabs.find((t) => t.id === tab.id);
  const name = tabLabel(current ?? tab);
  const failed = summary.failed > 0 || summary.stop != null;
  const title = i18n.t(failed ? "editor:notification.failed" : "editor:notification.ok", { tab: name });
  const counts = i18n.t("editor:results.summary", { ok: summary.succeeded, failed: summary.failed, notRun: summary.notRun });
  const seconds = Math.round(elapsedMs / 1000);
  const body = i18n.t("editor:notification.body", { summary: counts, duration: `${seconds} s` });
  void call(commands.notifyRunFinished(title, body)).catch(() => undefined);
}
