// What can be done to a table or a view (SDB-92), a schema, or a routine, sequence, trigger or
// other schema object (SDB-441) the explorer shows. A row's menu in the explorer (`Sidebar.tsx`)
// and its entry in the command palette (`commands.ts`) call these same functions, so the two
// cannot drift; `scripts/commands.test.mjs` compares their items. Rename…, Drop…, Duplicate
// table…, New table… and generated SQL open the same dialogs from either place
// (`ExplorerDialogHost`): the core writes and shows the SQL first, and runs it (ADR-026).

import { create } from "zustand";
import {
  call,
  commands,
  type BookmarkItem,
  type CreateKindItem,
  type NameFilter,
  type ObjectActionsItem,
  type ObjectItem,
  type ObjectTargetItem,
  type SchemaObjectItem,
  type SqlTemplateItem,
} from "./api";
import { connectTabFromExplorer, EXPLORER_REFRESH_EVENT, explorerDatabase, SET_ACTIVE_SCHEMA_EVENT } from "./connect";
import { openDiagram } from "./diagramTabs";
import { useApp } from "./store";
import { useExport } from "./components/ExportDialog";
import { useImport } from "./components/ImportDialog";
import { isConnected, isRunning } from "./tabState";

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

// ------------------------------------------------------------------- other schema objects

/** A function or a procedure, which the engine may write as source and call. */
const isCallable = (object: SchemaObjectItem) => object.kind === "procedure" || object.kind === "function";

/** "Edit source": where the core writes routines as the statement that replaces them. */
export const canEditRoutineSource = (object: SchemaObjectItem, actions: ObjectActionsItem | undefined) =>
  isCallable(object) && actions?.routineSource === true;

/** "Run…": a routine whose call the engine writes. */
export const canRunRoutine = (object: SchemaObjectItem, actions: ObjectActionsItem | undefined) =>
  isCallable(object) && actions?.routineCalls === true;

/** "Generate call": a procedure always, a function where the engine writes its call. */
export const canGenerateCall = (object: SchemaObjectItem, actions: ObjectActionsItem | undefined) =>
  object.kind === "procedure" || (object.kind === "function" && actions?.routineCalls === true);

/** The object's definition, written by the driver, in a new tab: nothing runs. */
export async function openObjectDefinition(connectionId: string, session: string, object: SchemaObjectItem) {
  openSqlTab(connectionId, session, object.schema, await call(commands.objectDefinition(session, object)));
}

/** "Edit source": the core writes the routine as the statement that replaces it (ADR-047). */
export async function editRoutineSource(connectionId: string, session: string, object: SchemaObjectItem) {
  openSqlTab(connectionId, session, object.schema, await call(commands.editableSource(session, { kind: "routine", object })));
}

/** "Generate call": the routine's call, written by the driver, in a new tab; "Run…" also runs it
 *  there, so the editor asks its parameters and its usual barriers apply (ADR-047). */
export async function openRoutineCall(connectionId: string, session: string, object: SchemaObjectItem, run = false) {
  openSqlTab(connectionId, session, object.schema, await call(commands.procedureCall(session, object)), run);
}

/** The object's bookmark (ADR-026), with its detail (a routine's arguments) to tell overloads apart. */
export function schemaObjectBookmark(connectionId: string, object: SchemaObjectItem): BookmarkItem {
  return { connection: connectionId, schema: object.schema, name: object.name, kind: object.kind, detail: object.detail ?? "" };
}

/** Drop…: the core's SQL shown, with "Cascade" where the engine has it, then run (ADR-026). */
export function dropSchemaObject(connectionId: string, session: string, object: SchemaObjectItem, onDone?: () => void) {
  useExplorerDialog.getState().open({ kind: "drop", session, connectionId, schema: object.schema, name: object.name, target: { kind: "schemaObject", object }, onDone });
}

// ------------------------------------------------------------------------------- schemas

/** A schema's bookmark (ADR-026). */
export function schemaBookmark(connectionId: string, schema: string): BookmarkItem {
  return { connection: connectionId, schema, name: "", kind: "schema", detail: "" };
}

/** "Import into a new table…": the import dialog, its table still to be named. */
export function importIntoNewTable(connectionId: string, session: string, schema: string) {
  useImport.getState().open({ session, connectionId, schema, table: null, text: null });
}

/** The ER diagram of the schema, with the connection's object filter (ADR-025). */
export function openSchemaDiagram(connectionId: string, session: string, schema: string, filter: NameFilter | null) {
  openDiagram(connectionId, { kind: "schema", schema, filter }, session);
}

/** "New table…" opens the create table form (ADR-047); another kind opens the core's statement
 *  in a new tab to finish (ADR-026). */
export async function createInSchema(connectionId: string, session: string, schema: string, kind: CreateKindItem, onDone?: () => void) {
  if (kind === "table") {
    useExplorerDialog.getState().open({ kind: "createTable", session, connectionId, schema, onDone });
    return;
  }
  const sql = await call(commands.createSql(session, kind, schema));
  openSqlTab(connectionId, session, kind === "schema" ? undefined : schema, sql);
}

/** What a schema's menu creates in it: what the engine creates, but not another schema. */
export function schemaCreateKinds(actions: ObjectActionsItem | undefined): CreateKindItem[] {
  return actions?.create.filter((kind) => kind !== "schema") ?? [];
}

/** Drop…: the schema, after its SQL is shown (ADR-026). */
export function dropSchema(connectionId: string, session: string, schema: string, onDone?: () => void) {
  useExplorerDialog.getState().open({ kind: "drop", session, connectionId, schema, name: schema, target: { kind: "schema", name: schema }, onDone });
}

/** The editor tab "Set as the editor's schema" applies to: the active query tab, connected to
 *  this connection and idle; `null` when there is none. */
export function schemaEditorTab(connectionId: string): string | null {
  const app = useApp.getState();
  const tab = app.tabs.find((t) => t.id === app.activeTab);
  return tab && !tab.table && !tab.tableDetails && !tab.plan && !tab.diagram && tab.connectionId === connectionId && isConnected(tab) && !isRunning(tab)
    ? tab.id
    : null;
}

/** "Set as the editor's schema": the editor tab changes its own session's schema. */
export function setEditorSchema(tabId: string, schema: string) {
  window.dispatchEvent(new CustomEvent(SET_ACTIVE_SCHEMA_EVENT, { detail: { tabId, schema } }));
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
  | { kind: "drop"; session: string; connectionId: string; schema: string; name: string; target: ObjectTargetItem; onDone?(): void }
  | { kind: "duplicate"; session: string; connectionId: string; object: ObjectItem }
  | { kind: "createTable"; session: string; connectionId: string; schema: string; onDone?(): void };

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
  const target: ObjectTargetItem = { kind: "relation", schema: object.schema, name: object.name, relation: object.kind };
  useExplorerDialog.getState().open({ kind: "drop", session, connectionId, schema: object.schema, name: object.name, target, onDone });
}

/** Duplicate table… (SDB-394): a new name and schema, its rows or not, run in a new tab. */
export function duplicateTable(connectionId: string, session: string, object: ObjectItem) {
  useExplorerDialog.getState().open({ kind: "duplicate", session, connectionId, object });
}
