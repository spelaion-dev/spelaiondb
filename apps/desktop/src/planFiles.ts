import { chooseOpenFile } from "./dialogs";
import { useApp } from "./store";
import i18n from "./i18n";

/** Opening a plan creates a local viewer, never an SQL tab that can execute its contents. */
export async function openSavedPlan(path?: string) {
  const selected = path ?? await chooseOpenFile(i18n.t("editor:plan.open"), [
    { name: i18n.t("editor:plan.files"), extensions: ["json", "txt", "plan", "sqlplan"] },
    { name: i18n.t("editor:files.allFiles"), extensions: ["*"] },
  ]);
  if (!selected) return;
  const app = useApp.getState();
  const previous = app.tabs.find((tab) => tab.plan?.path === selected);
  if (previous) { app.setActiveTab(previous.id); return; }
  const id = app.addTab(null, "");
  app.updateTab(id, { title: selected.split(/[\\/]/).pop() ?? selected, plan: { path: selected } });
}
