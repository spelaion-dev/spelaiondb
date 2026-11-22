// What can be done to a table or a view the explorer shows (SDB-92). Its menu in the explorer
// (`Sidebar.tsx`) and its entry in the command palette (`commands.ts`) call these same
// functions, so the two cannot drift; `scripts/commands.test.mjs` compares their items.
// Rename…, Drop…, Duplicate table… and generated SQL open the same dialogs from either place
// (`ExplorerDialogHost`): the core writes and shows the SQL first, and runs it (ADR-026).

import { create } from "zustand";
import { call, commands, type BookmarkItem, type ObjectActionsItem, type ObjectItem, type SqlTemplateItem } from "./api";
import { connectTabFromExplorer, EXPLORER_REFRESH_EVENT, explorerDatabase } from "./connect";
import { openDiagram } from "./diagramTabs";
import { useApp } from "./store";
import { useExport } from "./components/ExportDialog";
import { useImport } from "./components/ImportDialog";

/** The explorer's own session on a connection, which reads its catalog. */
export const explorerSession = (connectionId: string) => `meta:${connectionId}`;

// ------------------------------------------------------------------- what the engine offers

/** What a table's menu can generate, per session: the engine does not change while it is open. */
const tableTemplates = new Map<string, Promise<SqlTemplateItem[]>>();
/** The lists already answered, so a menu opens with the engine's own and does not change. */
const knownTemplates = new Map<string, SqlTemplateItem[]>();

export function templatesOf(session: string): Promise<SqlTemplateItem[]> {
  let templates = tableTemplates.get(session);
  if (!templates) {
    templates = call(commands.tableTemplates(session)).then(
      (list) => {
        knownTemplates.set(session, list);
        return list;
      },
      (e: unknown) => {
        tableTemplates.delete(session);
        throw e;
      },
    );
    tableTemplates.set(session, templates);
  }
  return templates;
}

/** What a relation can generate now: a view only its SELECT; a table the engine's list once it
 *  answered, and until then what every engine has. */
export function relationTemplates(session: string, object: ObjectItem): SqlTemplateItem[] {
  if (isView(object)) return ["select"];
  return knownTemplates.get(session) ?? ["select", "insert", "update", "delete"];
}

/** What the core lets the explorer do on a session's engine (ADR-026), asked once per session. */
const objectActions = new Map<string, ObjectActionsItem>();
const pendingObjectActions = new Map<string, Promise<ObjectActionsItem>>();

/** The engine's answer, if it was already given. */
export function knownObjectActions(session: string): ObjectActionsItem | undefined {
  return objectActions.get(session);
}

/** Asks the core once per session; a failed answer is asked again next time. */
export function loadObjectActions(session: string): Promise<ObjectActionsItem> {
  const known = objectActions.get(session);
  if (known) return Promise.resolve(known);
  let pending = pendingObjectActions.get(session);
  if (!pending) {
    pending = call(commands.objectActions(session));
    pendingObjectActions.set(session, pending);
    const asked = pending;
    asked.then(
      (answer) => {
        if (pendingObjectActions.get(session) === asked) {
          objectActions.set(session, answer);
          pendingObjectActions.delete(session);
        }
      },
      () => {
        if (pendingObjectActions.get(session) === asked) pendingObjectActions.delete(session);
      },
    );
  }
  return pending;
}

/** Forgets what a session's engine offers (it closed, or reconnects to another server). */
export function forgetExplorerSession(session: string) {
  tableTemplates.delete(session);
  knownTemplates.delete(session);
  objectActions.delete(session);
  pendingObjectActions.delete(session);
}

const isView = (object: ObjectItem) => object.kind.includes("view");

/** Whether the relation can be renamed: a table always, a view where the engine renames views. */
export function canRenameRelation(object: ObjectItem, actions: ObjectActionsItem | undefined): boolean {
  return !isView(object) || actions?.renameViews === true;
}

/** Tables (not views or collections) can be duplicated, imported into and drawn with relations. */
export const isPlainTable = (object: ObjectItem) => !isView(object) && object.kind !== "collection";

// ------------------------------------------------------------------------------- opening

/** SQL written by the core, in a new tab of the connection (on the schema's database where the
 *  engine has one per schema): nothing runs unless `run` (a routine's "Run…"). */
export function openSqlTab(connectionId: string, session: string, schema: string | undefined, sql: string, run = false) {
  const tab = useApp.getState().addTab(connectionId, sql);
  // "Run…": the tab runs its script once connected, asking the call's parameters first.
  if (run) useApp.getState().updateTab(tab, { runOnOpen: true });
  connectTabFromExplorer(tab, session, schema === undefined ? undefined : explorerDatabase(connectionId, schema));
}

/** Pages of the table in a table data tab (double-click or "View rows"). */
export function openRelationRows(connectionId: string, session: string, object: ObjectItem) {
  const tab = useApp.getState().addTableTab(connectionId, object.schema, object.name);
  connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
}

/** One of the core's statements for the relation: previewed in the SQL dialog, or (`inEditor`)
 *  opened in a new editor tab. Nothing runs. */
export async function openRelationSql(connectionId: string, session: string, object: ObjectItem, template: SqlTemplateItem, inEditor = false) {
  const sql = await call(commands.tableSql(session, object.schema, object.name, template));
  if (inEditor) openSqlTab(connectionId, session, object.schema, sql);
  else openGeneratedSql(sql);
}

/** "Generate DDL": the driver writes the CREATE statement, previewed in the SQL dialog. */
export async function openRelationDdl(session: string, object: ObjectItem) {
  openGeneratedSql(await call(commands.relationDefinition(session, object.schema, object.name)));
}

/** "Edit source": the core writes the view as the statement that replaces it (ADR-047). */
export async function editViewSource(connectionId: string, session: string, object: ObjectItem) {
  const sql = await call(commands.editableSource(session, { kind: "view", schema: object.schema, name: object.name }));
  openSqlTab(connectionId, session, object.schema, sql);
}

/** The export dialog for these tables (one, or every one chosen with Ctrl+click). */
export function exportRelations(connectionId: string, session: string, tables: { schema: string; name: string }[]) {
  useExport.getState().open({ kind: "tables", session, connectionId, tables });
}

/** The import dialog into this table. */
export function importIntoTable(connectionId: string, session: string, object: ObjectItem) {
  useImport.getState().open({ session, connectionId, schema: object.schema, table: object.name, text: null });
}

/** The ER diagram of the table's related tables, or of the tables chosen with Ctrl+click. */
export function openRelationDiagram(connectionId: string, session: string, object: ObjectItem, chosen: { schema: string; name: string }[] | null = null) {
  openDiagram(connectionId, chosen ? { kind: "tables", tables: chosen } : { kind: "focus", schema: object.schema, table: object.name }, session);
}

// ----------------------------------------------------------------------------- bookmarks

/** A relation's bookmark (ADR-026). Its kind comes from the driver as text; the core refuses one
 *  it does not know. */
export function relationBookmark(connectionId: string, object: ObjectItem): BookmarkItem {
  return { connection: connectionId, schema: object.schema, name: object.name, kind: object.kind as BookmarkItem["kind"], detail: "" };
}

export const sameBookmark = (a: BookmarkItem, b: BookmarkItem) =>
  a.connection === b.connection && a.schema === b.schema && a.name === b.name && a.kind === b.kind && a.detail === b.detail;

/** Whether an object is bookmarked now. */
export function isBookmarked(bookmark: BookmarkItem): boolean {
  return useApp.getState().bookmarks.some((b) => sameBookmark(b, bookmark));
}

/** The bookmarks are read again (after a change, or another workspace). */
export async function reloadBookmarks() {
  useApp.getState().setBookmarks(await call(commands.listBookmarks()));
}

/** Adds the bookmark, or removes it when the object already has one. */
export async function toggleBookmark(bookmark: BookmarkItem) {
  await call(isBookmarked(bookmark) ? commands.removeBookmark(bookmark) : commands.addBookmark(bookmark));
  await reloadBookmarks();
}

// --------------------------------------------------------------------- the shared dialogs

/** A dialog the explorer's menus and the palette open the same way (`ExplorerDialogHost`). */
export type ExplorerDialog =
  | { kind: "sql"; sql: string }
  | { kind: "rename"; session: string; connectionId: string; object: ObjectItem; onDone?(): void }
  | { kind: "drop"; session: string; connectionId: string; object: ObjectItem; onDone?(): void }
  | { kind: "duplicate"; session: string; connectionId: string; object: ObjectItem };

export const useExplorerDialog = create<{ dialog: ExplorerDialog | null; open(dialog: ExplorerDialog): void; close(): void }>((set) => ({
  dialog: null,
  open: (dialog) => set({ dialog }),
  close: () => set({ dialog: null }),
}));

/** An editable preview of SQL; generating SQL never opens a new connection. */
export function openGeneratedSql(sql: string) {
  useExplorerDialog.getState().open({ kind: "sql", sql });
}

/** After a rename or a drop: the caller's schema reads its objects again, or (from the palette)
 *  the connection's explorer does. */
export function changedSchema(connectionId: string, onDone: (() => void) | undefined) {
  if (onDone) onDone();
  else window.dispatchEvent(new CustomEvent(EXPLORER_REFRESH_EVENT, { detail: connectionId }));
}

/** Rename…: the new name asked, the core's SQL shown, then run (ADR-026). */
export function renameRelation(connectionId: string, session: string, object: ObjectItem, onDone?: () => void) {
  useExplorerDialog.getState().open({ kind: "rename", session, connectionId, object, onDone });
}

/** Drop…: the core's SQL shown, with "Cascade" where the engine has it, then run (ADR-026). */
export function dropRelation(connectionId: string, session: string, object: ObjectItem, onDone?: () => void) {
  useExplorerDialog.getState().open({ kind: "drop", session, connectionId, object, onDone });
}

/** Duplicate table… (SDB-394): a new name and schema, its rows or not, run in a new tab. */
export function duplicateTable(connectionId: string, session: string, object: ObjectItem) {
  useExplorerDialog.getState().open({ kind: "duplicate", session, connectionId, object });
}
