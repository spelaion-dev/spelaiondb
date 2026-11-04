// Side panel: open repository, connections (by folder) and schema explorer.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { Trans, useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  call,
  commands,
  type ConnectionItem,
  type IndexDetails,
  type ObjectItem,
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
import { EngineIcon } from "./EngineIcon";

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

/** Sorts names as Settings → Explorer says (columns are never sorted). */
function useSortByName() {
  const { i18n } = useTranslation();
  const sortAlphabetically = usePreferences((s) => s.explorer.sortAlphabetically);
  const sortIgnoreCase = usePreferences((s) => s.explorer.sortIgnoreCase);
  return <T,>(items: readonly T[], name: (item: T) => string) =>
    sortByName(items, name, { sortAlphabetically, sortIgnoreCase }, i18n.language);
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
  children,
}: {
  label: string;
  count: number;
  initiallyOpen?: boolean;
  children: ReactNode;
}) {
  const { t, i18n } = useTranslation("shell");
  const [open, setOpen] = useState(initiallyOpen);
  return (
    <div className="node">
      <div
        className="row folder-row"
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
        <span className="muted small">{count.toLocaleString(i18n.language)}</span>
      </div>
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
      useApp.getState().setWorkspace(await call(commands.trustWorkspace()));
    }
  }

  // Group by folder (the id path without its last segment). Connections not in any folder
  // come first, without a title, so they do not look like part of the previous folder.
  const groups = new Map<string, ConnectionItem[]>([
    [JSON.stringify([false]), []],
  ]);
  for (const c of connections) {
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
          className="ghost add-connection"
          onClick={() => useApp.getState().setConnectionDialog({ kind: "add" })}
          aria-label={t("connections:add")}
          title={t("connections:add")}
        >
          +
        </button>
      </div>
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
                />
              ))}
            </div>
          );
        })}
      </div>
    </aside>
  );
}

function ConnectionNode({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [open, setOpen] = useState(false);
  const [schemas, setSchemas] = useState<SchemaItem[] | null>(null);
  const [serverObjects, setServerObjects] = useState<ServerObjectItem[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /** Bumped by "Refresh all schemas": every schema and table node reads its data again. */
  const [generation, setGeneration] = useState(0);
  const session = `meta:${connection.id}`;

  function newQuery() {
    const tab = addTab(connection.id);
    if (schemas) connectTabFromExplorer(tab, session);
  }

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
    // Without its roles or logins the connection still shows its schemas.
    const [all, server] = await Promise.all([
      call(commands.listSchemas(session)),
      call(commands.listServerObjects(session)).catch((e: unknown) => {
        setError(errorText(e));
        return [];
      }),
    ]);
    setSchemas(all);
    setServerObjects(server);
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
  const shownServerObjects = sort(
    serverObjects.filter((o) => showSystem || !o.system),
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
      <div className="row connection-row">
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
        <button
          className="ghost small"
          onClick={newQuery}
          title={t("newQuery")}
        >
          +
        </button>
        <ConnectionMenu connection={connection} onRefresh={schemas ? () => void refreshAll() : undefined} />
      </div>
      {open && (
        <div className="children">
          {busy && <div className="muted small">{t(schemas ? "refreshing" : "connecting")}</div>}
          {error && <div className="error small">{error}</div>}
          {!foldersFirst && schemaNodes}
          {SERVER_FOLDERS.map((folder) => {
            const items = shownServerObjects.filter((o) => serverFolder(o.kind) === folder);
            return (
              items.length > 0 && (
                <Folder key={folder} label={t(`folders.${folder}`)} count={items.length}>
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
    </div>
  );
}

function SchemaNode({
  session,
  connectionId,
  schema,
  generation,
  onRefreshAll,
}: {
  session: string;
  connectionId: string;
  schema: string;
  /** Changes when the connection refreshes all its schemas. */
  generation: number;
  onRefreshAll(): void;
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

  async function load() {
    setError(null);
    try {
      // Without its functions or triggers the schema still shows its tables.
      const [relations, rest] = await Promise.all([
        call(commands.listObjects(session, schema)),
        call(commands.listSchemaObjects(session, schema)).catch((e: unknown) => {
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
        </div>
      )}
      {open && (
        <div className="children">
          {error && <div className="error small">{error}</div>}
          {objects &&
            SCHEMA_FOLDERS.map((folder) => {
              const relations = sort(
                objects.filter((o) => relationFolder(o.kind) === folder),
                (o) => o.name,
              );
              const rest = sort(
                others.filter((o) => schemaObjectFolder(o.kind) === folder),
                (o) => o.name,
              );
              const count = relations.length + rest.length;
              return (
                count > 0 && (
                  <Folder key={folder} label={t(`folders.${folder}`)} count={count}>
                    {relations.map((o) => (
                      <ObjectNode
                        key={o.name}
                        session={session}
                        connectionId={connectionId}
                        object={o}
                        generation={childGeneration}
                      />
                    ))}
                    {rest.map((o) => (
                      <SchemaObjectNode
                        key={`${o.kind}:${o.name}:${o.detail ?? ""}`}
                        session={session}
                        connectionId={connectionId}
                        object={o}
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
  generation,
}: {
  session: string;
  connectionId: string;
  object: ObjectItem;
  /** Changes when its schema (or the whole connection) is refreshed. */
  generation: string;
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

  async function loadDetails() {
    setDetails(
      await call(
        commands.describeTable(session, object.schema, object.name),
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
  const templates: SqlTemplateItem[] = view
    ? ["select"]
    : ["select", "insert", "update", "delete"];
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
        className="row"
        onClick={() => void toggle()}
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
      {open && details && (
        <div className="children">
          <Folder label={t("folders.columns")} count={details.columns.length} initiallyOpen>
            {details.columns.map((c) => (
              <div key={c.name} className="row leaf-row" title={c.comment ?? ""}>
                <span className="label">
                  {details.primaryKey.includes(c.name) ? "🔑 " : ""}
                  {c.name}
                </span>
                <span className="muted small">
                  {c.nativeType}
                  {c.nullable ? "" : " not null"}
                </span>
              </div>
            ))}
          </Folder>
          {details.constraints.length > 0 && (
            <Folder label={t("folders.constraints")} count={details.constraints.length}>
              {sort(details.constraints, (c) => c.name ?? "").map((c, i) => (
                <div key={c.name ?? `${c.kind}:${i}`} className="row leaf-row" title={c.definition}>
                  <span className="label">{c.name ?? t(`constraintKinds.${c.kind}`)}</span>
                  <span className="muted small">{c.definition}</span>
                </div>
              ))}
            </Folder>
          )}
          {details.foreignKeys.length > 0 && (
            <Folder label={t("folders.foreignKeys")} count={details.foreignKeys.length}>
              {sort(details.foreignKeys, (fk) => fk.name).map((fk) => {
                const target = `(${fk.columns.join(", ")}) → ${fk.referencedSchema === object.schema ? "" : `${fk.referencedSchema}.`}${fk.referencedTable} (${fk.referencedColumns.join(", ")})`;
                return (
                  <div key={fk.name} className="row leaf-row" title={target}>
                    <span className="label">{fk.name}</span>
                    <span className="muted small">{target}</span>
                  </div>
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
                  index={index}
                />
              ))}
            </Folder>
          )}
        </div>
      )}
    </div>
  );
}

/** An index: double-click or its menu opens its definition in a new tab (nothing runs). */
function IndexNode({
  session,
  connectionId,
  schema,
  index,
}: {
  session: string;
  connectionId: string;
  schema: string;
  index: IndexDetails;
}) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [menu, setMenu] = useContextMenu();
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
        </div>
      )}
    </div>
  );
}

/** A function, sequence, trigger, type…: double-click or the menu opens its definition. */
function SchemaObjectNode({
  session,
  connectionId,
  object,
}: {
  session: string;
  connectionId: string;
  object: SchemaObjectItem;
}) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);

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
        </div>
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
