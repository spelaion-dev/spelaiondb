// `.sql` files in editor tabs (ADR-036). The core reads and writes them (encodings, line
// endings, atomic writes, the changed-on-disk check); this module asks the user where, and
// what to do when the core says the file changed, was deleted or cannot hold a character.

import i18n from "./i18n";
import { call, commands, type FileFormatItem, type SaveFileOutcome, type SqlFileItem } from "./api";
import { chooseOpenFile, chooseSavePath, sqlFileFilters } from "./dialogs";
import { useApp, useDialogs, type Tab, type TabFile } from "./store";

/** Fired on `window` when the recent files changed (a file was opened, saved or forgotten). */
export const RECENT_FILES_EVENT = "spelaion-recent-files";

const isWindows = /Windows/.test(navigator.userAgent);

/** Whether two paths name the same file as the user sees it (case-insensitive on Windows). */
export function samePath(a: string, b: string): boolean {
  const norm = (p: string) => {
    const slashes = p.replaceAll("\\", "/");
    return isWindows ? slashes.toLowerCase() : slashes;
  };
  return norm(a) === norm(b);
}

/** The text differs from what was last read or saved (or that is unknown: restored unsaved). */
export function isModified(tab: Pick<Tab, "sql" | "file">): boolean {
  return Boolean(tab.file && (tab.file.savedText === null || tab.file.savedText !== tab.sql));
}

function tabFile(file: SqlFileItem): TabFile {
  return { path: file.path, name: file.name, format: file.format, hash: file.hash, savedText: file.text };
}

function recentChanged() {
  window.dispatchEvent(new Event(RECENT_FILES_EVENT));
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function filters() {
  return sqlFileFilters(i18n.t("editor:files.sqlFiles"), i18n.t("editor:files.allFiles"));
}

/** Ctrl+O: asks for a file and opens it (or shows the tab that has it). */
export async function openFileDialog(): Promise<void> {
  const app = useApp.getState();
  const near = app.tabs.find((t) => t.id === app.activeTab)?.file?.path;
  const path = await chooseOpenFile(i18n.t("editor:files.openTitle"), filters(), near);
  if (path) await openFilePath(path);
}

/** Opens `path` in a new tab on the active tab's connection, or shows the tab that has it.
 *  Returns the tab, or null when it could not be read (the error is shown). */
export async function openFilePath(path: string): Promise<string | null> {
  const app = useApp.getState();
  const open = app.tabs.find((t) => t.file && samePath(t.file.path, path));
  if (open) {
    app.setActiveTab(open.id);
    return open.id;
  }
  let file: SqlFileItem;
  try {
    file = await call(commands.openSqlFile(path));
  } catch (e) {
    await useDialogs.getState().askChoice({
      title: i18n.t("editor:files.openFailed"),
      message: errorText(e),
      choices: [{ id: "close", label: i18n.t("editor:files.close"), primary: true }],
    });
    return null;
  } finally {
    recentChanged();
  }
  const active = app.tabs.find((t) => t.id === app.activeTab);
  const connectionId = active && !active.table && !active.tableDetails ? active.connectionId : null;
  const id = useApp.getState().addTab(connectionId, file.text);
  useApp.getState().updateTab(id, { file: tabFile(file) });
  return id;
}

/** Writes the result of a save into the tab: the text saved is now the file's. */
function saved(id: string, text: string, outcome: Extract<SaveFileOutcome, { kind: "saved" }>) {
  useApp.getState().updateTab(id, {
    file: { path: outcome.path, name: outcome.name, format: outcome.format, hash: outcome.hash, savedText: text },
    restoreNotes: undefined,
  });
  recentChanged();
}

/**
 * Saves a tab to `path`: `expected` is the hash it last saw (`null`: write whatever is there).
 * Asks when the core reports a conflict. True when the file was written.
 */
async function writeTab(id: string, path: string, format: FileFormatItem | null, expected: string | null): Promise<boolean> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab) return false;
  const text = tab.sql;
  const dialogs = useDialogs.getState();
  const name = path.split(/[\\/]/).pop() ?? path;
  let outcome: SaveFileOutcome;
  try {
    outcome = await call(commands.saveSqlFile(path, text, format, expected));
  } catch (e) {
    await dialogs.askChoice({
      title: i18n.t("editor:files.saveFailed", { name }),
      message: errorText(e),
      choices: [{ id: "close", label: i18n.t("editor:files.close"), primary: true }],
    });
    return false;
  }
  switch (outcome.kind) {
    case "saved":
      saved(id, text, outcome);
      return true;
    case "changedOnDisk": {
      const answer = await dialogs.askChoice({
        title: i18n.t("editor:files.changedTitle", { name }),
        message: i18n.t("editor:files.changedMessage"),
        choices: [
          { id: "cancel", label: i18n.t("editor:files.cancel") },
          { id: "reload", label: i18n.t("editor:files.reload") },
          { id: "overwrite", label: i18n.t("editor:files.overwrite"), danger: true },
        ],
      });
      if (answer === "overwrite") return writeTab(id, path, format, null);
      if (answer === "reload") await reloadTab(id, false);
      return false;
    }
    case "deletedOnDisk": {
      const answer = await dialogs.askChoice({
        title: i18n.t("editor:files.deletedTitle", { name }),
        message: i18n.t("editor:files.deletedMessage"),
        choices: [
          { id: "cancel", label: i18n.t("editor:files.cancel") },
          { id: "write", label: i18n.t("editor:files.writeAgain"), primary: true },
        ],
      });
      return answer === "write" ? writeTab(id, path, format, null) : false;
    }
    case "unrepresentable": {
      const answer = await dialogs.askChoice({
        title: i18n.t("editor:files.encodingTitle", { name }),
        message: i18n.t("editor:files.encodingMessage", { encoding: outcome.encoding, character: outcome.character }),
        choices: [
          { id: "cancel", label: i18n.t("editor:files.cancel") },
          { id: "utf8", label: i18n.t("editor:files.saveUtf8"), primary: true },
        ],
      });
      if (answer !== "utf8") return false;
      const lineEnding = format?.lineEnding ?? "lf";
      return writeTab(id, path, { encoding: "utf8", lineEnding }, expected);
    }
  }
}

/** Ctrl+S on a file tab: saves to its file. A tab without a file asks where (save as). */
export async function saveTab(id: string): Promise<boolean> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab || tab.table || tab.tableDetails) return false;
  if (!tab.file) return saveTabAs(id);
  return writeTab(id, tab.file.path, tab.file.format, tab.file.hash);
}

/** Ctrl+Shift+S: saves the tab's text to a file the user picks; the tab then shows that file. */
export async function saveTabAs(id: string): Promise<boolean> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab || tab.table || tab.tableDetails) return false;
  const path = await chooseSavePath({
    title: i18n.t("editor:files.saveAsTitle"),
    defaultPath: tab.file?.path ?? `${(tab.title ?? i18n.t("editor:files.defaultName")).replace(/[\\/:*?"<>|]/g, "_")}.sql`,
    filters: filters().slice(0, 1),
  });
  if (!path) return false;
  // The system dialog already asked about replacing an existing file. Another file: written
  // in this one's format; a console: a new file.
  return writeTab(id, path, tab.file?.format ?? null, null);
}

/** Reads the tab's file again, dropping its unsaved text (after asking, unless `ask` is false
 *  because the user already chose to). */
export async function reloadTab(id: string, ask = true): Promise<void> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab?.file) return;
  if (ask && isModified(tab)) {
    const answer = await useDialogs.getState().askChoice({
      title: i18n.t("editor:files.reloadTitle", { name: tab.file.name }),
      message: i18n.t("editor:files.reloadMessage"),
      choices: [
        { id: "cancel", label: i18n.t("editor:files.cancel") },
        { id: "reload", label: i18n.t("editor:files.reload"), danger: true },
      ],
    });
    if (answer !== "reload") return;
  }
  try {
    const file = await call(commands.openSqlFile(tab.file.path));
    const revision = (useApp.getState().tabs.find((t) => t.id === id)?.textRevision ?? 0) + 1;
    useApp.getState().updateTab(id, { sql: file.text, textRevision: revision, file: tabFile(file), restoreNotes: undefined });
  } catch (e) {
    await useDialogs.getState().askChoice({
      title: i18n.t("editor:files.openFailed"),
      message: errorText(e),
      choices: [{ id: "close", label: i18n.t("editor:files.close"), primary: true }],
    });
  }
}

/**
 * Before closing tabs: asks about the file tabs with unsaved changes (save, don't save,
 * cancel). True when closing may go on.
 */
export async function confirmUnsavedFiles(ids: string[]): Promise<boolean> {
  const unsaved = useApp.getState().tabs.filter((t) => ids.includes(t.id) && isModified(t));
  if (unsaved.length === 0) return true;
  const names = unsaved.map((t) => t.file?.name ?? "").join(", ");
  const answer = await useDialogs.getState().askChoice({
    title: i18n.t("editor:files.unsavedTitle", { count: unsaved.length, names }),
    message: i18n.t("editor:files.unsavedMessage", { count: unsaved.length, names }),
    choices: [
      { id: "cancel", label: i18n.t("editor:files.cancel") },
      { id: "discard", label: i18n.t("editor:files.dontSave"), danger: true },
      { id: "save", label: i18n.t("editor:files.save"), primary: true },
    ],
  });
  if (answer === "discard") return true;
  if (answer !== "save") return false;
  for (const tab of unsaved) {
    if (!(await saveTab(tab.id))) return false;
  }
  return true;
}

/** The recent files, newest first (empty if they cannot be read). */
export async function recentFiles() {
  try {
    return await call(commands.recentFiles());
  } catch {
    return [];
  }
}

export async function forgetRecentFile(path: string): Promise<void> {
  await call(commands.forgetRecentFile(path)).catch(() => undefined);
  recentChanged();
}
