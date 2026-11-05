// Side panel: open repository, connections (by folder) and schema explorer.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Trans, useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  call,
  commands,
  type ConnectionItem,
  type CreateKindItem,
  type ExplorerFilter,
  type FilterActionItem,
  type FilterKindItem,
  type IndexDetails,
  type NameFilter,
  type ObjectActionsItem,
  type ObjectItem,
  type ObjectPathItem,
  type ObjectTargetItem,
  type SchemaItem,
  type SchemaObjectItem,
  type SchemaObjectKindItem,
  type ServerObjectItem,
  type ServerObjectKindItem,
  type SqlTemplateItem,
  type TableDetails,
} from "../api";
import {
  connectTabFromExplorer,
  explorerDatabase,
  openSession,
  SCHEMAS_REFRESHED_EVENT,
} from "../connect";
import { sortByName } from "../explorerSort";
import { usePreferences } from "../preferences";
import { useApp, useDialogs } from "../store";
import { ConnectionMenu } from "./ConnectionMenu";
import { ExplorerFilterDialog, isFilterEmpty, type FilterKind } from "./ExplorerFilterDialog";
import { EngineIcon } from "./EngineIcon";
import { ObjectChangeDialog } from "./ObjectChangeDialog";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

const compactCounts = new Map<string, Intl.NumberFormat>();

/** Keep large row counts readable in the narrow explorer, respecting the chosen language. */
function compactCount(value: number, language: string): string {
  let formatter = compactCounts.get(language);
  if (!formatter) {
    formatter = new Intl.NumberFormat(language, { notation: "compact", maximumFractionDigits: 1 });
    compactCounts.set(language, formatter);
  }
  return formatter.format(value);
}

/**
 * Runs `action` when `value` changes after the first render. Refreshing passes a new
 * generation down the tree: what a node already loaded is read again, and what is open stays
 * open.
 */
function useOnChange(value: unknown, action: () => void) {
  const previous = useRef(value);
  const latest = useRef(action);
  latest.current = action;
  useEffect(() => {
    if (previous.current === value) return;
    previous.current = value;
    latest.current();
  }, [value]);
}

/** F5 on a row refreshes what that row shows (and not the rows around it). */
function isRefreshKey(e: React.KeyboardEvent): boolean {
  if (e.key !== "F5") return false;
  e.preventDefault();
  e.stopPropagation();
  return true;
}

/** A context menu at the pointer that closes on Escape or a click elsewhere. */
function useContextMenu() {
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  useEffect(() => {
    if (!menu) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (
        e instanceof MouseEvent &&
        (e.target as Element | null)?.closest(".object-menu")
      )
        return;
      setMenu(null);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [menu]);
  return [menu, setMenu] as const;
}

/** Fired on `window` to put the keyboard in the explorer's quick filter (Ctrl+Shift+F). */
export const FOCUS_EXPLORER_FILTER_EVENT = "spelaion-focus-explorer-filter";

/** Whether a name contains the quick filter's text, ignoring case. An empty filter keeps all. */
function matchesFilter(name: string, filter: string): boolean {
  return filter === "" || name.toLocaleLowerCase().includes(filter.toLocaleLowerCase());
}

/** A plug: "Show only connected connections". */
function PlugIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d="M9 3v5M15 3v5M6.5 8h11v3a5.5 5.5 0 0 1-11 0zM12 16.5V21" />
    </svg>
  );
}

/** The magnifying glass of the quick filter. */
function SearchIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" aria-hidden="true" focusable="false">
      <circle cx="10.5" cy="10.5" r="6" />
      <path d="M15 15l5 5" />
    </svg>
  );
}

/** Whether the explorer's filters go into the catalog queries (Settings → Explorer). */
const serverSide = () => usePreferences.getState().explorer.serverSideFilters;

/** Sorts names as Settings → Explorer says (columns are never sorted). */
function useSortByName() {
  const { i18n } = useTranslation();
  const sortAlphabetically = usePreferences((s) => s.explorer.sortAlphabetically);
  const sortIgnoreCase = usePreferences((s) => s.explorer.sortIgnoreCase);
  return <T,>(items: readonly T[], name: (item: T) => string) =>
    sortByName(items, name, { sortAlphabetically, sortIgnoreCase }, i18n.language);
}

/** What a table's menu can generate, per session: the engine does not change while it is open. */
const tableTemplates = new Map<string, Promise<SqlTemplateItem[]>>();
/** The lists already answered, so a menu opens with the engine's own and does not change. */
const knownTemplates = new Map<string, SqlTemplateItem[]>();
function templatesOf(session: string): Promise<SqlTemplateItem[]> {
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
function forgetTemplates(session: string) {
  tableTemplates.delete(session);
  knownTemplates.delete(session);
  objectActions.delete(session);
}

/** What the core lets the explorer do on a session's engine (ADR-026), asked once per session
 *  (`enabled` once it is open). */
const objectActions = new Map<string, ObjectActionsItem>();
function useObjectActions(session: string, enabled = true): ObjectActionsItem | undefined {
  const [actions, setActions] = useState(() => objectActions.get(session));
  useEffect(() => {
    if (actions || !enabled) return;
    let current = true;
    void call(commands.objectActions(session))
      .then((answer) => {
        objectActions.set(session, answer);
        if (current) setActions(answer);
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [session, actions, enabled]);
  return actions;
}

/** SQL written by the core, in a new tab of the connection (on the schema's database where the
 *  engine has one per schema): nothing runs. */
function openSqlTab(connectionId: string, session: string, schema: string | undefined, sql: string) {
  const tab = useApp.getState().addTab(connectionId, sql);
  connectTabFromExplorer(tab, session, schema === undefined ? undefined : explorerDatabase(connectionId, schema));
}

/** "New table", "New view"…: the core's statement, to finish in a new tab (ADR-026). */
async function openCreate(connectionId: string, session: string, kind: CreateKindItem, schema: string) {
  const sql = await call(commands.createSql(session, kind, schema));
  openSqlTab(connectionId, session, kind === "schema" ? undefined : schema, sql);
}

/** The folder whose right-click offers "New …" of a kind. */
const CREATE_FOLDERS: Partial<Record<SchemaFolder, CreateKindItem>> = {
  tables: "table",
  views: "view",
  materializedViews: "materializedView",
  sequences: "sequence",
};

/** Delete drops the focused object (after its dialog), as in DBeaver. */
function isDropKey(e: React.KeyboardEvent): boolean {
  if (e.key !== "Delete" || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return false;
  e.preventDefault();
  e.stopPropagation();
  return true;
}

/** "Drop…" in an object's menu. */
function DropMenuItem({ onClick }: { onClick(): void }) {
  const { t } = useTranslation("shell");
  return (
    <button role="menuitem" className="danger-text" title="Delete" onClick={onClick}>
      {t("objectChange.dropMenu")}
    </button>
  );
}

/** Drops an object after showing the core's SQL, with "Cascade" where the engine has it. */
function DropDialog({
  session,
  connectionId,
  schema,
  name,
  target,
  onClose,
  onDone,
}: {
  session: string;
  connectionId: string;
  schema: string;
  name: string;
  target: ObjectTargetItem;
  onClose(): void;
  onDone(): void;
}) {
  const { t } = useTranslation("shell");
  return (
    <ObjectChangeDialog
      session={session}
      connectionId={connectionId}
      schema={schema}
      title={t("objectChange.dropTitle", { name })}
      confirmLabel={t("objectChange.drop")}
      danger
      change={({ cascade }) => ({ action: "drop", target, cascade })}
      onClose={onClose}
      onDone={onDone}
    />
  );
}

/** A constraint or foreign key of a table: its menu can drop it, where the engine can. */
function LeafRow({
  session,
  connectionId,
  schema,
  label,
  detail,
  target,
  onChanged,
}: {
  session: string;
  connectionId: string;
  schema: string;
  label: string;
  detail: string;
  /** What "Drop…" drops; none when it cannot be dropped from here. */
  target: ObjectTargetItem | null;
  onChanged(): void;
}) {
  const [menu, setMenu] = useContextMenu();
  const [dropping, setDropping] = useState(false);
  return (
    <div className="node">
      <div
        className="row leaf-row"
        title={detail}
        tabIndex={target ? 0 : undefined}
        onKeyDown={(e) => {
          if (target && isDropKey(e)) setDropping(true);
        }}
        onContextMenu={(e) => {
          if (!target) return;
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
      >
        <span className="label">{label}</span>
        <span className="muted small">{detail}</span>
      </div>
      {menu && target && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          <DropMenuItem
            onClick={() => {
              setMenu(null);
              setDropping(true);
            }}
          />
        </div>
      )}
      {dropping && target && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={schema}
          name={label}
          target={target}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onChanged();
          }}
        />
      )}
    </div>
  );
}

/** F2 renames the focused object, as in DBeaver. */
function isRenameKey(e: React.KeyboardEvent): boolean {
  if (e.key !== "F2" || e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return false;
  e.preventDefault();
  e.stopPropagation();
  return true;
}

/**
 * Writes names to the clipboard, one per line: as the tree shows them, or as the engine writes
 * them in SQL (`schema.table`, quoted where needed), which the core decides.
 */
async function copyNames(session: string, paths: ObjectPathItem[], qualified: boolean) {
  const names = qualified
    ? await call(commands.qualifiedNames(session, paths))
    : paths.map((path) => path.names[path.names.length - 1] ?? path.schema);
  await navigator.clipboard.writeText(names.join("\n"));
}

/** Ctrl+C copies the focused row's names and Ctrl+Shift+C their qualified names, as in DBeaver. */
function handleCopyKey(
  e: React.KeyboardEvent,
  session: string,
  paths: () => ObjectPathItem[],
  onError: (message: string) => void,
): boolean {
  if (!(e.ctrlKey || e.metaKey) || e.altKey || e.key.toLowerCase() !== "c") return false;
  e.preventDefault();
  e.stopPropagation();
  copyNames(session, paths(), e.shiftKey).catch((err: unknown) => onError(errorText(err)));
  return true;
}

/** "Copy" and DBeaver's "Advanced Copy" (the qualified name), for one name or several. */
function CopyMenuItems({
  session,
  paths,
  close,
  onError,
}: {
  session: string;
  paths: ObjectPathItem[];
  close(): void;
  onError(message: string): void;
}) {
  const { t, i18n } = useTranslation("shell");
  const several = paths.length > 1;
  const count = paths.length.toLocaleString(i18n.language);
  const copy = (qualified: boolean) => {
    close();
    copyNames(session, paths, qualified).catch((e: unknown) => onError(errorText(e)));
  };
  return (
    <>
      <button role="menuitem" onClick={() => copy(false)} title="Ctrl+C">
        {several ? t("copyNames", { count }) : t("copyName")}
      </button>
      <button role="menuitem" onClick={() => copy(true)} title="Ctrl+Shift+C">
        {several ? t("copyQualifiedNames", { count }) : t("copyQualifiedName")}
      </button>
    </>
  );
}

/** "Hide" or "Show only" names: they go into the connection's filter (ADR-025). */
type FilterName = (kind: FilterKindItem, names: string[], action: FilterActionItem) => void;

/** The two menu items DBeaver calls "Hide selected" and "Show only selected". */
function FilterMenuItems({
  kind,
  names,
  onFilterName,
  close,
}: {
  kind: FilterKindItem;
  /** The object right-clicked, or every one chosen with Ctrl+click. */
  names: string[];
  onFilterName: FilterName;
  close(): void;
}) {
  const { t, i18n } = useTranslation("shell");
  const one = names.length === 1 ? names[0] : null;
  const count = names.length.toLocaleString(i18n.language);
  return (
    <>
      <hr />
      <button
        role="menuitem"
        onClick={() => {
          close();
          onFilterName(kind, names, "hide");
        }}
      >
        {one !== null ? t("hideName", { name: one }) : t("hideNames", { count })}
      </button>
      <button
        role="menuitem"
        onClick={() => {
          close();
          onFilterName(kind, names, "showOnly");
        }}
      >
        {one !== null ? t("showOnlyName", { name: one }) : t("showOnlyNames", { count })}
      </button>
    </>
  );
}

/** The explorer's folders under a schema, in order (ADR-024). */
const SCHEMA_FOLDERS = [
  "tables",
  "foreignTables",
  "views",
  "materializedViews",
  "functions",
  "procedures",
  "aggregates",
  "sequences",
  "triggers",
  "types",
  "events",
  "synonyms",
] as const;
type SchemaFolder = (typeof SCHEMA_FOLDERS)[number];

function relationFolder(kind: string): SchemaFolder {
  switch (kind) {
    case "foreign_table":
      return "foreignTables";
    case "view":
      return "views";
    case "materialized_view":
      return "materializedViews";
    default:
      return "tables";
  }
}

function schemaObjectFolder(kind: SchemaObjectKindItem): SchemaFolder {
  switch (kind) {
    case "function":
      return "functions";
    case "procedure":
      return "procedures";
    case "aggregate":
      return "aggregates";
    case "sequence":
      return "sequences";
    case "trigger":
      return "triggers";
    case "enum":
    case "domain":
    case "type":
      return "types";
    case "event":
      return "events";
    case "synonym":
      return "synonyms";
  }
}

const SERVER_FOLDERS = ["roles", "users", "logins", "tablespaces", "extensions"] as const;

function serverFolder(kind: ServerObjectKindItem): (typeof SERVER_FOLDERS)[number] {
  switch (kind) {
    case "role":
      return "roles";
    case "user":
      return "users";
    case "login":
      return "logins";
    case "tablespace":
      return "tablespaces";
    case "extension":
      return "extensions";
  }
}

/** A folder of the explorer, closed until clicked, with how many items it holds. */
function Folder({
  label,
  count,
  initiallyOpen = false,
  forceOpen = false,
  filtered = false,
  onConfigureFilter,
  create,
  children,
}: {
  label: string;
  count: number;
  initiallyOpen?: boolean;
  /** Shown open whatever its state (the quick filter shows what matches inside). */
  forceOpen?: boolean;
  /** The connection's filter hides some of what it holds (ADR-025). */
  filtered?: boolean;
  /** Its right-click menu offers "Configure filter…", as DBeaver's folders do. */
  onConfigureFilter?: () => void;
  /** Its right-click menu offers "New …" of what it holds. */
  create?: { label: string; run(): void };
  children: ReactNode;
}) {
  const { t, i18n } = useTranslation("shell");
  const [chosen, setOpen] = useState(initiallyOpen);
  const [menu, setMenu] = useContextMenu();
  const open = chosen || forceOpen;
  return (
    <div className="node">
      <div
        className="row folder-row"
        onContextMenu={(e) => {
          if (!onConfigureFilter && !create) return;
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        onClick={() => setOpen(!open)}
        role="button"
        tabIndex={0}
        aria-expanded={open}
        aria-label={t("toggleFolder", { name: label })}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen(!open);
          }
        }}
      >
        <span className="twisty">{open ? "▾" : "▸"}</span>
        <span className="label">{label}</span>
        {filtered && (
          <span className="tag" title={t("filteredHint")}>
            {t("filtered")}
          </span>
        )}
        <span className="muted small">{count.toLocaleString(i18n.language)}</span>
      </div>
      {menu && (onConfigureFilter || create) && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          {create && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                create.run();
              }}
            >
              {create.label}
            </button>
          )}
          {onConfigureFilter && (
            <button
              role="menuitem"
              onClick={() => {
                setMenu(null);
                onConfigureFilter();
              }}
            >
              {t("configureFilter")}
            </button>
          )}
        </div>
      )}
      {open && <div className="children">{children}</div>}
    </div>
  );
}

export async function refreshConnections() {
  const app = useApp.getState();
  app.setConnections(await call(commands.listConnections()));
  app.setWorkspace(await call(commands.workspaceInfo()));
}

export function Sidebar({ hidden = false }: { hidden?: boolean }) {
  const { t, i18n } = useTranslation("shell");
  const workspace = useApp((s) => s.workspace);
  const connections = useApp((s) => s.connections);
  const cannotSwitch = useApp(
    (s) =>
      s.workspaceChanging ||
      s.historySaving.length > 0 ||
      s.tabs.some((t) => t.status === "connecting" || t.run?.running),
  );
  const [showProblems, setShowProblems] = useState(false);
  const [folderError, setFolderError] = useState<string | null>(null);
  const filter = useApp((s) => s.explorerFilter);
  const openConnections = useApp((s) => s.openConnections);
  const filterBox = useRef<HTMLInputElement>(null);
  useEffect(() => {
    const focus = () => requestAnimationFrame(() => filterBox.current?.focus());
    window.addEventListener(FOCUS_EXPLORER_FILTER_EVENT, focus);
    return () => window.removeEventListener(FOCUS_EXPLORER_FILTER_EVENT, focus);
  }, []);
  const filterText = filter ?? "";
  const explorer = usePreferences((s) => s.explorer);
  const explorerConnected = useApp((s) => s.explorerConnected);
  const tabs = useApp((s) => s.tabs);
  const tabConnected = tabs.filter((tab) => tab.status === "connected").map((tab) => tab.connectionId);
  /** Opened in the explorer or used by a connected tab. */
  const connected = (c: ConnectionItem) => explorerConnected.includes(c.id) || tabConnected.includes(c.id);
  /** With a filter, connections whose name matches, and those already expanded. */
  const shown = (c: ConnectionItem) =>
    (!explorer.connectedOnly || connected(c)) &&
    (matchesFilter(c.name, filterText) || openConnections.includes(c.id));

  async function openFolder() {
    const path = await open({ directory: true, title: t("openFolderTitle") });
    if (typeof path !== "string") return;
    const app = useApp.getState();
    // A connection ID can refer to a different server in the next folder. Keep the SQL,
    // but make the user choose a connection again before executing any existing tab.
    if (
      app.tabs.some((t) => t.status === "connecting" || t.run?.running) ||
      app.historySaving.length > 0
    )
      return;
    app.setWorkspaceChanging(true);
    setFolderError(null);
    try {
      const sessions = [
        ...app.tabs.map((t) => t.id),
        ...app.connections.map((c) => `meta:${c.id}`),
      ];
      app.clearTabConnections();
      app.setConnections([]);
      await Promise.all(
        sessions.map((session) => call(commands.disconnect(session))),
      );
      await Promise.all(
        app.tabs.flatMap((tab) =>
          (tab.run?.results ?? []).map((r) =>
            call(commands.releaseResult(r.id)),
          ),
        ),
      );
      app.setWorkspace(await call(commands.openWorkspace(path)));
      await refreshConnections();
    } catch (e) {
      setFolderError(errorText(e));
      await refreshConnections().catch(() => undefined);
    } finally {
      app.setWorkspaceChanging(false);
    }
  }

  async function trust() {
    if (!workspace?.workspace) return;
    if (await useDialogs.getState().askTrust(workspace.workspace)) {
      useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace.workspace)));
    }
  }

  // Group by folder (the id path without its last segment). Connections not in any folder
  // come first, without a title, so they do not look like part of the previous folder.
  const groups = new Map<string, ConnectionItem[]>([
    [JSON.stringify([false]), []],
  ]);
  for (const c of connections.filter(shown)) {
    const key = JSON.stringify([c.personal, ...c.folders]);
    groups.set(key, [...(groups.get(key) ?? []), c]);
  }

  const folderName = workspace?.workspace?.split(/[\\/]/).filter(Boolean).pop();
  return (
    <aside id="connections-sidebar" className="sidebar" hidden={hidden}>
      <div className="sidebar-header">
        <div className="workspace-name" title={workspace?.workspace ?? ""}>
          {folderName ?? t("noFolder")}
        </div>
        <button
          className="ghost"
          onClick={() => void openFolder()}
          title={t("openAnotherFolder")}
          disabled={cannotSwitch}
        >
          {t("openFolder")}
        </button>
        <button
          className={`ghost icon-button${explorer.connectedOnly ? " active" : ""}`}
          onClick={() => void usePreferences.getState().setExplorer({ ...explorer, connectedOnly: !explorer.connectedOnly })}
          aria-label={t("connectedOnly")}
          aria-pressed={explorer.connectedOnly}
          title={t("connectedOnly")}
        >
          <PlugIcon />
        </button>
        <button
          className={`ghost icon-button${filter !== null ? " active" : ""}`}
          onClick={() => {
            const app = useApp.getState();
            if (app.explorerFilter === null) {
              app.setExplorerFilter("");
              window.dispatchEvent(new Event(FOCUS_EXPLORER_FILTER_EVENT));
            } else app.setExplorerFilter(null);
          }}
          aria-label={t("quickFilter")}
          aria-pressed={filter !== null}
          title={t("quickFilterHint")}
        >
          <SearchIcon />
        </button>
        <button
          className="ghost add-connection"
          onClick={() => useApp.getState().setConnectionDialog({ kind: "add" })}
          aria-label={t("connections:add")}
          title={t("connections:add")}
        >
          +
        </button>
      </div>
      {filter !== null && (
        <div className="explorer-filter">
          <input
            ref={filterBox}
            type="search"
            value={filter}
            placeholder={t("quickFilterPlaceholder")}
            aria-label={t("quickFilter")}
            onChange={(e) => useApp.getState().setExplorerFilter(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                e.preventDefault();
                useApp.getState().setExplorerFilter(filter === "" ? null : "");
              }
            }}
          />
        </div>
      )}
      {folderError && (
        <div className="banner" role="alert">
          {folderError}
        </div>
      )}
      {workspace?.hasWorkspaceConnections && !workspace.trusted && (
        <div className="banner">
          {t("repositoryUntrusted")}
          <button onClick={() => void trust()}>{t("reviewAndTrust")}</button>
        </div>
      )}
      {workspace && workspace.problems.length > 0 && (
        <div className="banner banner-warn">
          <button
            className="link"
            onClick={() => setShowProblems(!showProblems)}
          >
            {t("configurationErrors", {
              count: workspace.problems.length,
              displayCount: workspace.problems.length.toLocaleString(
                i18n.language,
              ),
            })}
          </button>
          {showProblems && (
            <ul>
              {workspace.problems.map((p) => (
                <li key={p}>{p}</li>
              ))}
            </ul>
          )}
        </div>
      )}
      <div className="tree">
        {connections.length > 0 && filterText !== "" && !connections.some(shown) && (
          <div className="empty small">{t("quickFilterNothing")}</div>
        )}
        {connections.length > 0 && filterText === "" && explorer.connectedOnly && !connections.some(shown) && (
          <div className="empty small">
            {t("noneConnected")}{" "}
            <button className="link" onClick={() => void usePreferences.getState().setExplorer({ ...explorer, connectedOnly: false })}>
              {t("showAllConnections")}
            </button>
          </div>
        )}
        {connections.length === 0 && (
          <div className="empty small">
            <Trans
              t={t}
              i18nKey="noConnections"
              components={{ code: <code />, strong: <strong /> }}
            />
          </div>
        )}
        {[...groups.entries()].map(([group, items]) => {
          const first = items[0];
          const label = first
            ? (first.personal ? [t("personal")] : [])
                .concat(first.folders)
                .join(" / ")
            : "";
          return (
            <div key={group}>
              {label && <div className="group-title">{label}</div>}
              {items.map((c) => (
                <ConnectionNode
                  key={`${workspace?.workspace ?? ""}:${c.id}`}
                  connection={c}
                  filter={filterText}
                />
              ))}
            </div>
          );
        })}
      </div>
    </aside>
  );
}

function ConnectionNode({ connection, filter }: { connection: ConnectionItem; filter: string }) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [open, setOpen] = useState(false);
  const [schemas, setSchemas] = useState<SchemaItem[] | null>(null);
  const [serverObjects, setServerObjects] = useState<ServerObjectItem[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /** Bumped by "Refresh all schemas": every schema and table node reads its data again. */
  const [generation, setGeneration] = useState(0);
  /** This connection's include and exclude patterns (ADR-025), read when it is opened. */
  const [filters, setFilters] = useState<ExplorerFilter | null>(null);
  const [editingFilters, setEditingFilters] = useState<{ filter: ExplorerFilter; kind?: FilterKind } | null>(null);
  const [rowMenu, setRowMenu] = useContextMenu();
  /** "Hide" / "Show only": the filter changes and the tree is read again. */
  const filterName: FilterName = (kind, names, action) => {
    useApp.getState().clearExplorerSelection();
    void call(commands.filterExplorerName(connection.id, kind, names, action))
      .then((next) => {
        setFilters(next);
        void refreshAll();
      })
      .catch((e: unknown) => setError(errorText(e)));
  };
  /** Opens the filter dialog with the saved patterns: one kind from a folder, or all. */
  function configureFilter(kind?: FilterKind) {
    setRowMenu(null);
    void call(commands.getExplorerFilter(connection.id))
      .then((filter) => setEditingFilters({ filter, kind }))
      .catch((e: unknown) => setError(errorText(e)));
  }
  const session = `meta:${connection.id}`;
  const actions = useObjectActions(session, schemas !== null);

  function newQuery() {
    const tab = addTab(connection.id);
    if (schemas) connectTabFromExplorer(tab, session);
  }

  useEffect(() => {
    useApp.getState().setConnectionOpen(connection.id, open);
  }, [connection.id, open]);
  useEffect(
    () => () => {
      useApp.getState().setConnectionOpen(connection.id, false);
      useApp.getState().setExplorerConnected(connection.id, false);
    },
    [connection.id],
  );

  async function toggle() {
    if (open) return setOpen(false);
    setOpen(true);
    if (schemas) return;
    setBusy(true);
    setError(null);
    try {
      if ((await openSession(session, connection.id)) === null) {
        setOpen(false);
        return;
      }
      await load();
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  async function load() {
    // The connection may have been edited to point to another engine; asked again now, so
    // tables' menus have the list before they open.
    forgetTemplates(session);
    const filter = await call(commands.getExplorerFilter(connection.id));
    setFilters(filter);
    // Without its roles or logins the connection still shows its schemas.
    const [all, server] = await Promise.all([
      call(commands.listSchemas(session, filter.schemas, usePreferences.getState().explorer.serverSideFilters)),
      call(commands.listServerObjects(session)).catch((e: unknown) => {
        setError(errorText(e));
        return [];
      }),
    ]);
    setSchemas(all);
    setServerObjects(server);
    useApp.getState().setExplorerConnected(connection.id, true);
    void templatesOf(session).catch(() => undefined);
  }

  /** Schemas, server objects and everything already loaded under them, read again. */
  async function refreshAll() {
    if (!schemas || busy) return;
    setBusy(true);
    setError(null);
    try {
      await load();
      setGeneration((g) => g + 1);
      window.dispatchEvent(new CustomEvent(SCHEMAS_REFRESHED_EVENT, { detail: connection.id }));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const showSystem = usePreferences((s) => s.explorer.showSystem);
  const foldersFirst = usePreferences((s) => s.explorer.foldersFirst);
  const sort = useSortByName();
  const shownSchemas = schemas && sort(schemas.filter((s) => showSystem || !s.system), (s) => s.name);
  // A connection whose name matches the quick filter shows everything in it.
  const inner = matchesFilter(connection.name, filter) ? "" : filter;
  const shownServerObjects = sort(
    serverObjects.filter((o) => (showSystem || !o.system) && matchesFilter(o.name, inner)),
    (o) => o.name,
  );
  const schemaNodes = shownSchemas?.map((s) => (
    <SchemaNode
      key={s.name}
      session={session}
      connectionId={connection.id}
      schema={s.name}
      generation={generation}
      onRefreshAll={() => void refreshAll()}
      filter={inner}
      objectFilter={filters?.objects ?? null}
      columnFilter={filters?.columns ?? null}
      onConfigureFilter={configureFilter}
      onFilterName={filterName}
    />
  ));
  const prod = connection.env?.toLowerCase().startsWith("prod");
  return (
    <div
      className="node"
      onKeyDown={(e) => {
        if (isRefreshKey(e)) void refreshAll();
      }}
    >
      <div
        className="row connection-row"
        onContextMenu={(e) => {
          e.preventDefault();
          setRowMenu({ x: e.clientX, y: e.clientY });
        }}
      >
        <button
          className="twisty"
          onClick={() => void toggle()}
          aria-expanded={open}
          aria-label={t("toggleConnection", { connection: connection.name })}
        >
          {open ? "▾" : "▸"}
        </button>
        <EngineIcon engine={connection.engine} />
        <span
          className="label"
          onDoubleClick={newQuery}
          title={connection.id}
        >
          {connection.name}
        </span>
        {connection.tunnel && (
          <span
            className="tag"
            title={t("tunnel", { name: connection.tunnel })}
          >
            SSH
          </span>
        )}
        {prod && <span className="tag tag-prod">prod</span>}
        {!isFilterEmpty(filters, "schemas") && (
          <span className="tag" title={t("filteredHint")}>
            {t("filtered")}
          </span>
        )}
        <button
          className="ghost small"
          onClick={newQuery}
          title={t("newQuery")}
        >
          +
        </button>
        <ConnectionMenu
          connection={connection}
          onRefresh={schemas ? () => void refreshAll() : undefined}
          onConfigureFilters={() => configureFilter()}
        />
      </div>
      {rowMenu && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: rowMenu.x, top: rowMenu.y }}>
          <button role="menuitem" onClick={() => configureFilter("schemas")}>
            {t("configureSchemaFilter")}
          </button>
          {schemas && (
            <button
              role="menuitem"
              onClick={() => {
                setRowMenu(null);
                void refreshAll();
              }}
            >
              {t("refreshAllSchemas")}
            </button>
          )}
          {schemas && actions?.create.includes("schema") && (
            <>
              <hr />
              <button
                role="menuitem"
                onClick={() => {
                  setRowMenu(null);
                  openCreate(connection.id, session, "schema", "").catch((e: unknown) => setError(errorText(e)));
                }}
              >
                {t("create.schema")}
              </button>
            </>
          )}
        </div>
      )}
      {open && (
        <div className="children">
          {busy && <div className="muted small">{t(schemas ? "refreshing" : "connecting")}</div>}
          {error && <div className="error small">{error}</div>}
          {!foldersFirst && schemaNodes}
          {SERVER_FOLDERS.map((folder) => {
            const items = shownServerObjects.filter((o) => serverFolder(o.kind) === folder);
            return (
              items.length > 0 && (
                <Folder key={folder} label={t(`folders.${folder}`)} count={items.length} forceOpen={inner !== ""}>
                  {items.map((o) => (
                    <div key={`${o.kind}:${o.name}`} className="row leaf-row" title={o.detail ?? ""}>
                      <span className="label">{o.name}</span>
                      {o.detail && <span className="muted small">{o.detail}</span>}
                    </div>
                  ))}
                </Folder>
              )
            );
          })}
          {foldersFirst && schemaNodes}
        </div>
      )}
      {editingFilters && (
        <ExplorerFilterDialog
          connection={connection}
          filter={editingFilters.filter}
          kind={editingFilters.kind}
          onClose={() => setEditingFilters(null)}
          onSaved={(next) => {
            setEditingFilters(null);
            setFilters(next);
            void refreshAll();
          }}
        />
      )}
    </div>
  );
}

function SchemaNode({
  session,
  connectionId,
  schema,
  generation,
  onRefreshAll,
  filter,
  objectFilter,
  columnFilter,
  onConfigureFilter,
  onFilterName,
}: {
  session: string;
  connectionId: string;
  schema: string;
  /** Changes when the connection refreshes all its schemas. */
  generation: number;
  onRefreshAll(): void;
  /** The quick filter's text ("" when nothing is filtered here). */
  filter: string;
  /** The connection's object and column patterns (ADR-025), null before they are read. */
  objectFilter: NameFilter | null;
  columnFilter: NameFilter | null;
  onConfigureFilter(kind: FilterKind): void;
  onFilterName: FilterName;
}) {
  const { t, i18n } = useTranslation("shell");
  const [open, setOpen] = useState(false);
  const [objects, setObjects] = useState<ObjectItem[] | null>(null);
  const [others, setOthers] = useState<SchemaObjectItem[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [menu, setMenu] = useContextMenu();
  /** Bumped by "Refresh schema": the tables already described read their details again. */
  const [refreshed, setRefreshed] = useState(0);
  const sort = useSortByName();
  const actions = useObjectActions(session);
  const [dropping, setDropping] = useState(false);

  async function load() {
    setError(null);
    try {
      // Without its functions or triggers the schema still shows its tables.
      const [relations, rest] = await Promise.all([
        call(commands.listObjects(session, schema, objectFilter, serverSide())),
        call(commands.listSchemaObjects(session, schema, objectFilter, serverSide())).catch((e: unknown) => {
          setError(errorText(e));
          return [];
        }),
      ]);
      setOthers(rest);
      setObjects(relations);
    } catch (e) {
      setError(errorText(e));
    }
  }

  async function toggle() {
    setOpen(!open);
    if (open || objects) return;
    await load();
  }

  /** Read again what was loaded: now if the schema is open, when it is opened otherwise. */
  function reload() {
    if (!objects) return;
    if (open) void load();
    else setObjects(null);
  }

  function refresh() {
    setMenu(null);
    reload();
    setRefreshed((n) => n + 1);
  }

  useOnChange(generation, reload);
  const childGeneration = `${generation}.${refreshed}`;

  // A schema whose name matches the quick filter shows everything in it; otherwise only what
  // matches, and it is hidden if it is closed and nothing loaded in it matches.
  const inner = matchesFilter(schema, filter) ? "" : filter;
  const anyMatch =
    objects !== null &&
    (objects.some((o) => matchesFilter(o.name, inner)) || others.some((o) => matchesFilter(o.name, inner)));
  if (inner !== "" && !open && !anyMatch) return null;

  return (
    <div
      className="node"
      onKeyDown={(e) => {
        if (isRefreshKey(e)) refresh();
      }}
    >
      <div
        className="row"
        onClick={(e) => { if (e.detail === 1) void toggle(); }}
        onDoubleClick={() => {
          const database = explorerDatabase(connectionId, schema);
          if (!database) return;
          const tab = useApp.getState().addTab(connectionId);
          connectTabFromExplorer(tab, session, database);
        }}
        role="button"
        tabIndex={0}
        aria-expanded={open}
        aria-label={t("toggleSchema", { name: schema })}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        onKeyDown={(e) => {
          if (handleCopyKey(e, session, () => [{ schema: "", names: [schema] }], setError)) return;
          if (isDropKey(e)) {
            if (actions?.dropSchemas) setDropping(true);
            return;
          }
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            void toggle();
          }
        }}
      >
        <span className="twisty">{open ? "▾" : "▸"}</span>
        <span className="label">{schema}</span>
        {objects && (
          <span className="muted small">
            {(objects.length + others.length).toLocaleString(i18n.language)}
          </span>
        )}
      </div>
      {menu && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          <CopyMenuItems
            session={session}
            paths={[{ schema: "", names: [schema] }]}
            close={() => setMenu(null)}
            onError={setError}
          />
          <hr />
          <button role="menuitem" onClick={refresh}>
            {t("refreshSchema")}
          </button>
          <button
            role="menuitem"
            onClick={() => {
              setMenu(null);
              onRefreshAll();
            }}
          >
            {t("refreshAllSchemas")}
          </button>
          <FilterMenuItems kind="schemas" names={[schema]} onFilterName={onFilterName} close={() => setMenu(null)} />
          {actions && actions.create.some((kind) => kind !== "schema") && <hr />}
          {actions?.create
            .filter((kind) => kind !== "schema")
            .map((kind) => (
              <button
                key={kind}
                role="menuitem"
                onClick={() => {
                  setMenu(null);
                  openCreate(connectionId, session, kind, schema).catch((e: unknown) => setError(errorText(e)));
                }}
              >
                {t(`create.${kind}`)}
              </button>
            ))}
          {actions?.dropSchemas && (
            <>
              <hr />
              <DropMenuItem
                onClick={() => {
                  setMenu(null);
                  setDropping(true);
                }}
              />
            </>
          )}
        </div>
      )}
      {dropping && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={schema}
          name={schema}
          target={{ kind: "schema", name: schema }}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onRefreshAll();
          }}
        />
      )}
      {open && (
        <div className="children">
          {error && <div className="error small">{error}</div>}
          {objects &&
            SCHEMA_FOLDERS.map((folder) => {
              const relations = sort(
                objects.filter((o) => relationFolder(o.kind) === folder && matchesFilter(o.name, inner)),
                (o) => o.name,
              );
              const rest = sort(
                others.filter((o) => schemaObjectFolder(o.kind) === folder && matchesFilter(o.name, inner)),
                (o) => o.name,
              );
              const count = relations.length + rest.length;
              return (
                count > 0 && (
                  <Folder
                    key={folder}
                    label={t(`folders.${folder}`)}
                    count={count}
                    forceOpen={inner !== ""}
                    filtered={!!objectFilter && (objectFilter.include.length > 0 || objectFilter.exclude.length > 0)}
                    onConfigureFilter={() => onConfigureFilter("objects")}
                    create={(() => {
                      const kind = CREATE_FOLDERS[folder];
                      return kind && actions?.create.includes(kind)
                        ? {
                            label: t(`create.${kind}`),
                            run: () => {
                              openCreate(connectionId, session, kind, schema).catch((e: unknown) => setError(errorText(e)));
                            },
                          }
                        : undefined;
                    })()}
                  >
                    {relations.map((o) => (
                      <ObjectNode
                        key={o.name}
                        session={session}
                        connectionId={connectionId}
                        object={o}
                        onSchemaChanged={refresh}
                        generation={childGeneration}
                        columnFilter={columnFilter}
                        onConfigureFilter={onConfigureFilter}
                        onFilterName={onFilterName}
                      />
                    ))}
                    {rest.map((o) => (
                      <SchemaObjectNode
                        key={`${o.kind}:${o.name}:${o.detail ?? ""}`}
                        session={session}
                        connectionId={connectionId}
                        object={o}
                        onSchemaChanged={refresh}
                        onFilterName={onFilterName}
                      />
                    ))}
                  </Folder>
                )
              );
            })}
        </div>
      )}
    </div>
  );
}

const KIND_BADGES: Record<string, string> = {
  table: "T",
  partitioned_table: "P",
  foreign_table: "F",
  view: "V",
  materialized_view: "M",
};

function ObjectNode({
  session,
  connectionId,
  object,
  onSchemaChanged,
  generation,
  columnFilter,
  onConfigureFilter,
  onFilterName,
}: {
  session: string;
  connectionId: string;
  object: ObjectItem;
  /** After it was renamed or dropped: its schema reads its objects again. */
  onSchemaChanged(): void;
  /** Changes when its schema (or the whole connection) is refreshed. */
  generation: string;
  /** The connection's column patterns (ADR-025). */
  columnFilter: NameFilter | null;
  onConfigureFilter(kind: FilterKind): void;
  onFilterName: FilterName;
}) {
  const { t, i18n } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [open, setOpen] = useState(false);
  const [details, setDetails] = useState<TableDetails | null>(null);
  const [menu, setMenu] = useContextMenu();
  const sort = useSortByName();
  /** Exact count asked from the menu ("counting" while it runs). */
  const [count, setCount] = useState<number | "counting" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [dropping, setDropping] = useState(false);
  const actions = useObjectActions(session);

  async function loadDetails() {
    setDetails(
      await call(
        commands.describeTable(session, object.schema, object.name, columnFilter),
      ).catch(() => null),
    );
  }

  async function toggle() {
    setOpen(!open);
    if (open || details) return;
    await loadDetails();
  }

  /** Columns, keys and indexes read again (now if open), and the exact count forgotten. */
  function refresh() {
    setMenu(null);
    setCount(null);
    if (!details) return;
    if (open) void loadDetails();
    else setDetails(null);
  }
  useOnChange(generation, refresh);

  /** The core writes the SQL with the engine's quoting; it opens in a new tab, not run. */
  async function openSql(template: SqlTemplateItem) {
    setMenu(null);
    try {
      setError(null);
      const tab = addTab(
        connectionId,
        await call(
          commands.tableSql(session, object.schema, object.name, template),
        ),
      );
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  /** Tables and views chosen with Ctrl+click in this session, this one among them or not. */
  const selection = useApp((s) => s.explorerSelection).filter((o) => o.session === session);
  const chosen = selection.some((o) => o.schema === object.schema && o.name === object.name);
  /** A SELECT joining the chosen tables on their foreign keys, in a new tab. */
  async function openJoin(inner: boolean) {
    setMenu(null);
    try {
      setError(null);
      const sql = await call(
        commands.joinSql(
          session,
          selection.map((o) => ({ schema: o.schema, name: o.name })),
          inner,
        ),
      );
      const tab = addTab(connectionId, sql);
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  /** "Generate DDL": the driver writes the CREATE statement; it opens in a new tab. */
  async function openDdl() {
    setMenu(null);
    try {
      setError(null);
      const tab = addTab(
        connectionId,
        await call(commands.relationDefinition(session, object.schema, object.name)),
      );
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  /** Pages of the table in a table data tab (double-click or "View rows"). */
  const preview = () => {
    setMenu(null);
    const tab = useApp.getState().addTableTab(connectionId, object.schema, object.name);
    connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
  };
  async function countRows() {
    setMenu(null);
    setError(null);
    setCount("counting");
    try {
      setCount(
        await call(commands.countRows(session, object.schema, object.name)),
      );
    } catch (e) {
      setCount(null);
      setError(e instanceof Error ? e.message : String(e));
    }
  }
  const view = object.kind.includes("view");
  const canRename = !view || actions?.renameViews === true;
  const [tableMenu, setTableMenu] = useState<SqlTemplateItem[] | undefined>(() => knownTemplates.get(session));
  useEffect(() => {
    if (!menu || view || tableMenu) return;
    let current = true;
    void templatesOf(session)
      .then((list) => {
        if (current) setTableMenu(list);
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [menu, view, session, tableMenu]);
  // Until the engine's list arrives, only what every engine has.
  const templates: SqlTemplateItem[] = view ? ["select"] : (tableMenu ?? ["select", "insert", "update", "delete"]);
  /** What "Copy" takes: every table chosen with Ctrl+click, or this one. */
  const copyPaths: ObjectPathItem[] = (chosen ? selection : [object]).map((o) => ({ schema: o.schema, names: [o.name] }));
  const rowCount = typeof count === "number" ? count : count === null ? object.estimatedRows : null;
  const countDescription = rowCount == null ? undefined : `${t(typeof count === "number" ? "sql.exactCount" : "sql.estimate")}: ${rowCount.toLocaleString(i18n.language)}`;
  return (
    <div
      className="node"
      onKeyDown={(e) => {
        if (isRefreshKey(e)) refresh();
      }}
    >
      <div
        className={`row${chosen ? " selected" : ""}`}
        aria-selected={chosen}
        onClick={(e) => {
          if (e.ctrlKey || e.metaKey) {
            useApp.getState().toggleExplorerSelection({ session, schema: object.schema, name: object.name });
            return;
          }
          useApp.getState().clearExplorerSelection();
          void toggle();
        }}
        onDoubleClick={preview}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        title={object.comment ?? t("viewRowsHint")}
        role="button"
        tabIndex={0}
        aria-expanded={open}
        aria-label={t("toggleObject", { name: object.name })}
        aria-description={countDescription}
        onKeyDown={(e) => {
          if (handleCopyKey(e, session, () => copyPaths, setError)) return;
          if (isRenameKey(e)) {
            if (canRename) setRenaming(true);
            return;
          }
          if (isDropKey(e)) {
            setDropping(true);
            return;
          }
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            void toggle();
          }
        }}
      >
        <span className="twisty">{open ? "▾" : "▸"}</span>
        <span
          className={`kind ${view ? "kind-view" : "kind-table"}`}
          title={t(`kinds.${object.kind}`)}
        >
          {KIND_BADGES[object.kind] ?? "T"}
        </span>
        <span className="label">{object.name}</span>
        {count === "counting" ? (
          <span className="muted small">{t("sql.counting")}</span>
        ) : count != null ? (
          <span className="small" title={countDescription}>
            {compactCount(count, i18n.language)}
          </span>
        ) : (
          object.estimatedRows != null && (
            <span className="muted small" title={countDescription}>
              ~{compactCount(object.estimatedRows, i18n.language)}
            </span>
          )
        )}
      </div>
      {menu && (
        <div
          className="grid-menu object-menu"
          role="menu"
          style={{ position: "fixed", left: menu.x, top: menu.y }}
        >
          <button role="menuitem" onClick={preview}>
            {t("sql.preview")}
          </button>
          <button role="menuitem" onClick={() => void openSql("preview")}>
            {t("sql.openAsSql")}
          </button>
          <button role="menuitem" onClick={() => void countRows()}>
            {t("sql.count")}
          </button>
          <button role="menuitem" onClick={refresh}>
            {t("refresh")}
          </button>
          <hr />
          <CopyMenuItems session={session} paths={copyPaths} close={() => setMenu(null)} onError={setError} />
          {canRename && (
            <button
              role="menuitem"
              title="F2"
              onClick={() => {
                setMenu(null);
                setRenaming(true);
              }}
            >
              {t("objectChange.renameMenu")}
            </button>
          )}
          <DropMenuItem
            onClick={() => {
              setMenu(null);
              setDropping(true);
            }}
          />
          <FilterMenuItems
            kind="objects"
            names={chosen ? selection.map((o) => o.name) : [object.name]}
            onFilterName={onFilterName}
            close={() => setMenu(null)}
          />
          <hr />
          {chosen && selection.length > 1 && (
            <>
              <button role="menuitem" onClick={() => void openJoin(false)}>
                {t("sql.join", { count: selection.length.toLocaleString(i18n.language) })}
              </button>
              <button role="menuitem" onClick={() => void openJoin(true)}>
                {t("sql.innerJoin", { count: selection.length.toLocaleString(i18n.language) })}
              </button>
            </>
          )}
          <button role="menuitem" onClick={() => void openDdl()}>
            {t("generateDdl")}
          </button>
          {templates.map((template) => (
            <button
              key={template}
              role="menuitem"
              onClick={() => void openSql(template)}
            >
              {t(`sql.${template}`)}
            </button>
          ))}
        </div>
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
      {renaming && (
        <ObjectChangeDialog
          session={session}
          connectionId={connectionId}
          schema={object.schema}
          title={t("objectChange.renameTitle", { name: object.name })}
          confirmLabel={t("objectChange.rename")}
          rename={{ current: object.name }}
          change={({ newName }) => ({
            action: "rename",
            target: { kind: "relation", schema: object.schema, name: object.name, relation: object.kind },
            newName,
          })}
          onClose={() => setRenaming(false)}
          onDone={() => {
            setRenaming(false);
            onSchemaChanged();
          }}
        />
      )}
      {dropping && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={object.schema}
          name={object.name}
          target={{ kind: "relation", schema: object.schema, name: object.name, relation: object.kind }}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onSchemaChanged();
          }}
        />
      )}
      {open && details && (
        <div className="children">
          <Folder
            label={t("folders.columns")}
            count={details.columns.length}
            initiallyOpen
            filtered={!!columnFilter && (columnFilter.include.length > 0 || columnFilter.exclude.length > 0)}
            onConfigureFilter={() => onConfigureFilter("columns")}
          >
            {details.columns.map((c) => (
              <ColumnRow
                key={c.name}
                session={session}
                connectionId={connectionId}
                schema={object.schema}
                table={object.name}
                onTableChanged={refresh}
                column={c}
                primary={details.primaryKey.includes(c.name)}
                onFilterName={onFilterName}
              />
            ))}
          </Folder>
          {details.constraints.length > 0 && (
            <Folder label={t("folders.constraints")} count={details.constraints.length}>
              {sort(details.constraints, (c) => c.name ?? "").map((c, i) => (
                <LeafRow
                  key={c.name ?? `${c.kind}:${i}`}
                  session={session}
                  connectionId={connectionId}
                  schema={object.schema}
                  label={c.name ?? t(`constraintKinds.${c.kind}`)}
                  detail={c.definition}
                  target={
                    c.name != null && actions?.dropConstraints
                      ? { kind: "constraint", schema: object.schema, table: object.name, name: c.name, constraint: c.kind }
                      : null
                  }
                  onChanged={refresh}
                />
              ))}
            </Folder>
          )}
          {details.foreignKeys.length > 0 && (
            <Folder label={t("folders.foreignKeys")} count={details.foreignKeys.length}>
              {sort(details.foreignKeys, (fk) => fk.name).map((fk) => {
                const target = `(${fk.columns.join(", ")}) → ${fk.referencedSchema === object.schema ? "" : `${fk.referencedSchema}.`}${fk.referencedTable} (${fk.referencedColumns.join(", ")})`;
                return (
                  <LeafRow
                    key={fk.name}
                    session={session}
                    connectionId={connectionId}
                    schema={object.schema}
                    label={fk.name}
                    detail={target}
                    target={
                      actions?.dropConstraints
                        ? { kind: "foreignKey", schema: object.schema, table: object.name, name: fk.name }
                        : null
                    }
                    onChanged={refresh}
                  />
                );
              })}
            </Folder>
          )}
          {details.indexes.length > 0 && (
            <Folder label={t("folders.indexes")} count={details.indexes.length}>
              {sort(details.indexes, (index) => index.name).map((index) => (
                <IndexNode
                  key={index.name}
                  session={session}
                  connectionId={connectionId}
                  schema={object.schema}
                  table={object.name}
                  index={index}
                  onTableChanged={refresh}
                />
              ))}
            </Folder>
          )}
        </div>
      )}
    </div>
  );
}

/** A column of a table: its menu can hide it, or show only it (ADR-025). */
function ColumnRow({
  session,
  connectionId,
  schema,
  table,
  onTableChanged,
  column,
  primary,
  onFilterName,
}: {
  session: string;
  connectionId: string;
  schema: string;
  table: string;
  /** After a column was renamed or dropped: the table reads its columns again. */
  onTableChanged(): void;
  column: TableDetails["columns"][number];
  primary: boolean;
  onFilterName: FilterName;
}) {
  const { t } = useTranslation("shell");
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [dropping, setDropping] = useState(false);
  const paths = [{ schema, names: [table, column.name] }];
  return (
    <div className="node">
      <div
        className="row leaf-row"
        title={column.comment ?? ""}
        tabIndex={0}
        onKeyDown={(e) => {
          if (handleCopyKey(e, session, () => paths, setError)) return;
          if (isRenameKey(e)) setRenaming(true);
          if (isDropKey(e)) setDropping(true);
        }}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
      >
        <span className="label">
          {primary ? "🔑 " : ""}
          {column.name}
        </span>
        <span className="muted small">
          {column.nativeType}
          {column.nullable ? "" : " not null"}
        </span>
      </div>
      {menu && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          <CopyMenuItems session={session} paths={paths} close={() => setMenu(null)} onError={setError} />
          <button
            role="menuitem"
            title="F2"
            onClick={() => {
              setMenu(null);
              setRenaming(true);
            }}
          >
            {t("objectChange.renameMenu")}
          </button>
          <DropMenuItem
            onClick={() => {
              setMenu(null);
              setDropping(true);
            }}
          />
          <FilterMenuItems kind="columns" names={[column.name]} onFilterName={onFilterName} close={() => setMenu(null)} />
        </div>
      )}
      {renaming && (
        <ObjectChangeDialog
          session={session}
          connectionId={connectionId}
          schema={schema}
          title={t("objectChange.renameColumnTitle", { name: column.name, table })}
          confirmLabel={t("objectChange.rename")}
          rename={{ current: column.name }}
          change={({ newName }) => ({
            action: "rename",
            target: { kind: "column", schema, table, name: column.name },
            newName,
          })}
          onClose={() => setRenaming(false)}
          onDone={() => {
            setRenaming(false);
            onTableChanged();
          }}
        />
      )}
      {dropping && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={schema}
          name={`${table}.${column.name}`}
          target={{ kind: "column", schema, table, name: column.name }}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onTableChanged();
          }}
        />
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/** An index: double-click or its menu opens its definition in a new tab (nothing runs). */
function IndexNode({
  session,
  connectionId,
  schema,
  table,
  index,
  onTableChanged,
}: {
  session: string;
  connectionId: string;
  schema: string;
  table: string;
  index: IndexDetails;
  /** After it was dropped: the table reads its indexes again. */
  onTableChanged(): void;
}) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);
  const [dropping, setDropping] = useState(false);
  const paths = [{ schema, names: [index.name] }];
  const openDefinition = () => {
    setMenu(null);
    const sql = index.definition.startsWith("--") ? index.definition : `${index.definition.trimEnd().replace(/;$/, "")};`;
    const tab = addTab(connectionId, sql);
    connectTabFromExplorer(tab, session, explorerDatabase(connectionId, schema));
  };
  return (
    <div className="node">
      <div
        className="row leaf-row"
        onDoubleClick={openDefinition}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        title={index.definition}
        role="button"
        tabIndex={0}
        aria-label={t("openObject", { name: index.name })}
        onKeyDown={(e) => {
          if (handleCopyKey(e, session, () => paths, setError)) return;
          if (isDropKey(e)) {
            setDropping(true);
            return;
          }
          if (e.key === "Enter") {
            e.preventDefault();
            openDefinition();
          }
        }}
      >
        <span className="label">{index.name}</span>
        {(index.primary || index.unique) && (
          <span className="muted small">{index.primary ? "PRIMARY KEY" : "UNIQUE"}</span>
        )}
      </div>
      {menu && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          <button role="menuitem" onClick={openDefinition}>
            {t("openDefinition")}
          </button>
          <hr />
          <CopyMenuItems session={session} paths={paths} close={() => setMenu(null)} onError={setError} />
          <DropMenuItem
            onClick={() => {
              setMenu(null);
              setDropping(true);
            }}
          />
        </div>
      )}
      {dropping && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={schema}
          name={index.name}
          target={{ kind: "index", schema, table, name: index.name, primary: index.primary }}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onTableChanged();
          }}
        />
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}

/** A function, sequence, trigger, type…: double-click or the menu opens its definition. */
function SchemaObjectNode({
  session,
  connectionId,
  object,
  onSchemaChanged,
  onFilterName,
}: {
  session: string;
  connectionId: string;
  object: SchemaObjectItem;
  /** After it was dropped: its schema reads its objects again. */
  onSchemaChanged(): void;
  onFilterName: FilterName;
}) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);
  const [dropping, setDropping] = useState(false);

  /** The driver writes the DDL; it opens in a new tab and nothing runs. */
  async function openDefinition() {
    setMenu(null);
    setError(null);
    try {
      const tab = addTab(connectionId, await call(commands.objectDefinition(session, object)));
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    } catch (e) {
      setError(errorText(e));
    }
  }
  /** "Generate CALL": the procedure's call, written by the driver, in a new tab. */
  async function openCall() {
    setMenu(null);
    setError(null);
    try {
      const tab = addTab(connectionId, await call(commands.procedureCall(session, object)));
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    } catch (e) {
      setError(errorText(e));
    }
  }

  const routine =
    object.kind === "function" || object.kind === "procedure" || object.kind === "aggregate";
  const detail =
    object.kind === "trigger" && object.detail
      ? t("onTable", { table: object.detail })
      : object.kind === "enum"
        ? "ENUM"
        : object.kind === "domain"
          ? `DOMAIN ${object.detail ?? ""}`.trim()
          : routine
            ? null
            : object.detail;
  return (
    <div className="node">
      <div
        className="row leaf-row"
        onDoubleClick={() => void openDefinition()}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        title={object.comment ?? t("definitionHint")}
        role="button"
        tabIndex={0}
        aria-label={t("openObject", { name: object.name })}
        onKeyDown={(e) => {
          if (handleCopyKey(e, session, () => [{ schema: object.schema, names: [object.name] }], setError)) return;
          if (isDropKey(e)) {
            setDropping(true);
            return;
          }
          if (e.key === "Enter") {
            e.preventDefault();
            void openDefinition();
          }
        }}
      >
        <span className="label">
          {object.name}
          {routine && object.detail !== null && (
            <span className="muted">({object.detail})</span>
          )}
        </span>
        {detail && <span className="muted small">{detail}</span>}
      </div>
      {menu && (
        <div
          className="grid-menu object-menu"
          role="menu"
          style={{ position: "fixed", left: menu.x, top: menu.y }}
        >
          <button role="menuitem" onClick={() => void openDefinition()}>
            {t("openDefinition")}
          </button>
          {object.kind === "procedure" && (
            <button role="menuitem" onClick={() => void openCall()}>
              {t("generateCall")}
            </button>
          )}
          <hr />
          <CopyMenuItems
            session={session}
            paths={[{ schema: object.schema, names: [object.name] }]}
            close={() => setMenu(null)}
            onError={setError}
          />
          <DropMenuItem
            onClick={() => {
              setMenu(null);
              setDropping(true);
            }}
          />
          <FilterMenuItems kind="objects" names={[object.name]} onFilterName={onFilterName} close={() => setMenu(null)} />
        </div>
      )}
      {dropping && (
        <DropDialog
          session={session}
          connectionId={connectionId}
          schema={object.schema}
          name={object.name}
          target={{ kind: "schemaObject", object }}
          onClose={() => setDropping(false)}
          onDone={() => {
            setDropping(false);
            onSchemaChanged();
          }}
        />
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
