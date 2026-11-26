// SQL notebooks (ADR-087). A notebook tab is a file tab whose text is the notebook as its file
// holds it: the core writes that text from the cells (`notebook_text`) and reads cells from it
// (`parse_notebook`), so saving, the changed-on-disk check, the unsaved mark and kept tabs are
// the editor's own (ADR-036, `sqlFiles.ts`). The cells live in the tab; their results stay in
// the core, keyed by cell, and are never written to the file.

import i18n from "./i18n";
import { call, commands, type SqlFileItem } from "./api";
import type { Notebook } from "./bindings";
import { type ConnectionKey } from "./connectionKey";
import { useApp, useDialogs, type NotebookCell, type Tab } from "./store";

let cellCounter = 0;

/** A new id for a cell of the page (never written to the file). */
export function cellId(): string {
  cellCounter += 1;
  return `cell-${cellCounter}`;
}

/** Whether `path` names a notebook file (a Markdown file, ADR-087 point 1). */
export function isNotebookPath(path: string): boolean {
  return /\.md$/i.test(path);
}

/** The notebook the core reads and writes, from a tab's cells. */
export function notebookOf(tab: Pick<Tab, "connectionId" | "notebook">, cells: NotebookCell[]): Notebook {
  return {
    connection: tab.connectionId ?? null,
    cells: cells.map((cell) => cell.kind === "sql"
      ? { kind: "sql" as const, name: cell.name, sql: cell.text }
      : { kind: "markdown" as const, text: cell.text }),
  };
}

/** The page's cells, from a notebook the core read. */
export function cellsOf(notebook: Notebook): NotebookCell[] {
  return notebook.cells.map((cell) => cell.kind === "sql"
    ? { id: cellId(), kind: "sql", name: cell.name, text: cell.sql }
    : { id: cellId(), kind: "markdown", name: "", text: cell.text });
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The connection the front matter names, when exactly one listed connection (an open folder's
 *  or a personal one) has that id (ADR-087 point 11); never connected by this. */
export function suggestedConnection(id: string | null | undefined): ConnectionKey | null {
  if (!id) return null;
  const found = useApp.getState().connections.filter((c) => c.id === id);
  return found.length === 1 ? { id: found[0]!.id, folder: found[0]!.folder ?? null } : null;
}

/** Replaces a notebook tab's cells: the core writes its text again soon after. */
export function setCells(id: string, cells: NotebookCell[]) {
  const app = useApp.getState();
  const tab = app.tabs.find((t) => t.id === id);
  if (!tab?.notebook) return;
  app.updateTab(id, { notebook: { ...tab.notebook, cells, revision: tab.notebook.revision + 1 } });
}

/**
 * Has the core write the tab's text from its cells now (before a save, and after each change):
 * returns why it could not (a Markdown cell holding a line that would read back as a cell), or
 * null. The text then is what a save writes and a kept tab keeps.
 */
export async function syncNotebookText(id: string): Promise<string | null> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  const cells = tab?.notebook?.cells;
  if (!tab?.notebook || !cells) return tab?.notebook?.problem ?? null;
  const revision = tab.notebook.revision;
  try {
    const text = await call(commands.notebookText(notebookOf(tab, cells)));
    const now = useApp.getState().tabs.find((t) => t.id === id);
    if (!now?.notebook) return null;
    // Only the latest cells' text is kept: an older answer that arrives later is dropped.
    if (now.notebook.revision === revision) useApp.getState().updateTab(id, { sql: text, notebook: { ...now.notebook, problem: undefined } });
    return null;
  } catch (e) {
    const problem = errorText(e);
    const now = useApp.getState().tabs.find((t) => t.id === id);
    if (now?.notebook) useApp.getState().updateTab(id, { notebook: { ...now.notebook, problem } });
    return problem;
  }
}

/** A new notebook tab on `connection` (none: no connection), with one empty SQL cell. */
export function newNotebook(connection: ConnectionKey | null): string {
  const app = useApp.getState();
  const id = app.addTab(connection, "");
  app.updateTab(id, { notebook: { cells: [{ id: cellId(), kind: "sql", name: "cell_1", text: "" }], revision: 1 } });
  void syncNotebookText(id);
  return id;
}

/** Reads a notebook tab's cells from its text (opened, reloaded or restored). Nothing runs. */
export async function readNotebookCells(id: string): Promise<void> {
  const tab = useApp.getState().tabs.find((t) => t.id === id);
  if (!tab?.notebook) return;
  try {
    const notebook = await call(commands.parseNotebook(tab.sql));
    const now = useApp.getState().tabs.find((t) => t.id === id);
    if (!now?.notebook) return;
    // The text is not written again until the cells change: a file read as it is stays unmodified.
    useApp.getState().updateTab(id, { notebook: { cells: cellsOf(notebook), revision: 0 } });
  } catch (e) {
    const now = useApp.getState().tabs.find((t) => t.id === id);
    if (now?.notebook) useApp.getState().updateTab(id, { notebook: { ...now.notebook, cells: null, problem: errorText(e) } });
  }
}

/** Opens a notebook file read as text by the core (`openSqlFile`), in a new tab. Its front
 *  matter's connection is selected when it is unambiguous; nothing connects or runs. Null when
 *  the text is not a notebook (the reason is shown). */
export async function openNotebookText(file: SqlFileItem): Promise<string | null> {
  let notebook: Notebook;
  try {
    notebook = await call(commands.parseNotebook(file.text));
  } catch (e) {
    await useDialogs.getState().askChoice({
      title: i18n.t("editor:notebook.openFailed", { name: file.name }),
      message: errorText(e),
      choices: [{ id: "close", label: i18n.t("editor:files.close"), primary: true }],
    });
    return null;
  }
  const connection = suggestedConnection(notebook.connection);
  const app = useApp.getState();
  const id = app.addTab(connection, file.text);
  app.updateTab(id, {
    file: { path: file.path, name: file.name, format: file.format, hash: file.hash, savedText: file.text },
    notebook: { cells: cellsOf(notebook), revision: 0 },
    ...(notebook.connection && !connection
      ? { restoreNotes: [{ kind: "connectionMissing" as const, connection: notebook.connection }] }
      : {}),
  });
  return id;
}

/** The name the core gives a new SQL cell: the first free `cell_<n>`. */
export async function newNotebookCellName(tab: Pick<Tab, "connectionId" | "notebook">, cells: NotebookCell[]): Promise<string> {
  try {
    return await call(commands.newNotebookCellName(notebookOf(tab, cells)));
  } catch {
    return `cell_${cells.length + 1}`;
  }
}

/** The names a SQL cell's text seems to reference, for the line under its name. Only shown:
 *  the core finds the references that run, outside strings and comments, when it expands it. */
export function notebookCellsReferenced(text: string): string[] {
  const names = [...text.matchAll(/\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}/g)].map((m) => m[1]!);
  return [...new Set(names)];
}
