// Side panel: open repository, connections (by folder) and schema explorer.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { Trans, useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  call,
  commands,
  type BookmarkItem,
  type ConnectionItem,
  type CreateKindItem,
  type ExplorerFilter,
  type FilterActionItem,
  type FilterKindItem,
  type IndexDetails,
  type NameFilter,
  type NewIndexKindItem,
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
  EXPLORER_REFRESH_EVENT,
  openSession,
  SCHEMAS_REFRESHED_EVENT,
  SET_ACTIVE_SCHEMA_EVENT,
} from "../connect";
import { connectionTooltip, hasTags, sameTag, tagsInUse } from "../connectionInfo";
import { sortByName } from "../explorerSort";
import { usePreferences } from "../preferences";
import { useApp, useDialogs, type ExplorerReveal, type TableDetailKind } from "../store";
import { ConnectionMenu } from "./ConnectionMenu";
import { ExplorerFilterDialog, isFilterEmpty, type FilterKind } from "./ExplorerFilterDialog";
import { EngineIcon } from "./EngineIcon";
import { ObjectChangeDialog } from "./ObjectChangeDialog";
import { openFound } from "./OpenObjectDialog";

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

/** Tags shown next to a connection's name; the others are only in its tooltip. */
const VISIBLE_TAGS = 2;

/** Whether a name contains the quick filter's text, ignoring case. An empty filter keeps all. */
function matchesFilter(name: string, filter: string): boolean {
  return filter === "" || name.toLocaleLowerCase().includes(filter.toLocaleLowerCase());
}

/** A separate submenu, positioned outside the scrollable explorer menu. */
function GenerateMenu({ children }: { children: ReactNode }) {
  const { t } = useTranslation("shell");
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const submenu = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (!open) return;
    const position = () => {
      if (!trigger.current || !submenu.current) return;
      const anchor = trigger.current.getBoundingClientRect();
      const rect = submenu.current.getBoundingClientRect();
      const x = anchor.right + rect.width <= window.innerWidth - 8 ? anchor.right : anchor.left - rect.width;
      submenu.current.style.left = `${Math.max(8, Math.min(x, window.innerWidth - rect.width - 8))}px`;
      submenu.current.style.top = `${Math.max(8, Math.min(anchor.top, window.innerHeight - rect.height - 8))}px`;
    };
    position();
    window.addEventListener("resize", position);
    window.addEventListener("scroll", position, true);
    return () => {
      window.removeEventListener("resize", position);
      window.removeEventListener("scroll", position, true);
    };
  }, [open, children]);
  return (
    <div className="explorer-generate-group" onMouseEnter={() => setOpen(true)} onMouseLeave={(e) => {
      if (!submenu.current?.contains(e.relatedTarget as Node | null) && !e.currentTarget.contains(document.activeElement)) setOpen(false);
    }} onBlur={(e) => {
      if (!e.currentTarget.contains(e.relatedTarget) && !submenu.current?.contains(e.relatedTarget)) setOpen(false);
    }}>
      <button ref={trigger} role="menuitem" aria-haspopup="menu" aria-expanded={open} onFocus={() => setOpen(true)} onClick={() => setOpen(true)}>
        {t("generateSql")} ▸
      </button>
      {open && createPortal(
        <div ref={submenu} className="grid-menu object-menu explorer-generate-items" role="menu" aria-label={t("generateSql")}>{children}</div>,
        document.body,
      )}
    </div>
  );
}

/** An editable preview; generating SQL never opens a new connection. */
function GeneratedSqlDialog({ initialSql, onClose }: { initialSql: string; onClose(): void }) {
  const { t } = useTranslation("shell");
  const [sql, setSql] = useState(initialSql);
  const [copied, setCopied] = useState(false);
  const [error, setError] = useState<string | null>(null);
  async function copy() {
    try {
      await navigator.clipboard.writeText(sql);
      setCopied(true);
      setError(null);
    } catch (e) {
      setError(errorText(e));
    }
  }
  return (
    <div className="modal-backdrop">
      <section className="modal generated-sql-dialog" role="dialog" aria-modal="true" aria-label={t("generatedSql.title")}
        onKeyDown={(e) => { if (e.key === "Escape") onClose(); }}>
        <h2>{t("generatedSql.title")}</h2>
        <textarea autoFocus spellCheck={false} aria-label={t("generatedSql.editor")} value={sql}
          onChange={(e) => { setSql(e.target.value); setCopied(false); }} />
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <button type="button" onClick={() => void copy()}>{t(copied ? "generatedSql.copied" : "generatedSql.copy")}</button>
          <button type="button" onClick={onClose}>{t("generatedSql.close")}</button>
        </div>
      </section>
    </div>
  );
}

/** A plug: "Show only connected connections". */
function PlugIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d="M9 3v5M15 3v5M6.5 8h11v3a5.5 5.5 0 0 1-11 0zM12 16.5V21" />
    </svg>
  );
}

/** Two chain links: "Link with editor". */
function LinkIcon() {
  return (
    <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d="M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1" />
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
  pendingObjectActions.delete(session);
}

/** What the core lets the explorer do on a session's engine (ADR-026), asked once per session
 *  (`enabled` once it is open). */
const objectActions = new Map<string, ObjectActionsItem>();
const pendingObjectActions = new Map<string, Promise<ObjectActionsItem>>();
function useObjectActions(session: string, enabled = true): ObjectActionsItem | undefined {
  const [actions, setActions] = useState(() => objectActions.get(session));
  useEffect(() => {
    if (actions || !enabled) return;
    let current = true;
    let pending = pendingObjectActions.get(session);
    if (!pending) {
      pending = call(commands.objectActions(session));
      pendingObjectActions.set(session, pending);
    }
    void pending
      .then((answer) => {
        if (pendingObjectActions.get(session) === pending) {
          objectActions.set(session, answer);
          pendingObjectActions.delete(session);
        }
        if (current) setActions(answer);
      })
      .catch(() => { if (pendingObjectActions.get(session) === pending) pendingObjectActions.delete(session); });
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
  icon,
  target,
  onChanged,
}: {
  session: string;
  connectionId: string;
  schema: string;
  label: string;
  detail: string;
  icon: "constraints" | "foreignKeys";
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
        <TreeIcon kind={icon} />
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

/**
 * Shows a row the explorer was asked to reveal (`matches`): scrolls it into view and outlines
 * it for a moment, without taking the keyboard from where it is.
 */
function useRevealedRow(matches: (reveal: ExplorerReveal) => boolean) {
  const reveal = useApp((s) => s.explorerReveal);
  const row = useRef<HTMLDivElement>(null);
  const [revealed, setRevealed] = useState(false);
  const hit = reveal !== null && matches(reveal);
  useEffect(() => {
    if (!reveal || !hit) return;
    row.current?.scrollIntoView({ block: "center" });
    setRevealed(true);
    useApp.getState().revealDone(reveal.nonce);
    const timer = window.setTimeout(() => setRevealed(false), 1500);
    return () => window.clearTimeout(timer);
    // Once per request.
  }, [reveal?.nonce, hit]);
  return { row, revealed: revealed ? " revealed" : "" };
}

/** The bookmarks are read again (after a change, or another workspace). */
async function reloadBookmarks() {
  useApp.getState().setBookmarks(await call(commands.listBookmarks()));
}

const sameBookmark = (a: BookmarkItem, b: BookmarkItem) =>
  a.connection === b.connection && a.schema === b.schema && a.name === b.name && a.kind === b.kind && a.detail === b.detail;

/** Whether an object is bookmarked (ADR-026). */
function useBookmarked(bookmark: BookmarkItem): boolean {
  return useApp((s) => s.bookmarks.some((b) => sameBookmark(b, bookmark)));
}

/** "Add bookmark" or "Remove bookmark" in an object's menu. */
function BookmarkMenuItem({ bookmark, close, onError }: { bookmark: BookmarkItem; close(): void; onError(message: string): void }) {
  const { t } = useTranslation("shell");
  const bookmarked = useBookmarked(bookmark);
  return (
    <button
      role="menuitem"
      onClick={() => {
        close();
        call(bookmarked ? commands.removeBookmark(bookmark) : commands.addBookmark(bookmark))
          .then(reloadBookmarks)
          .catch((e: unknown) => onError(errorText(e)));
      }}
    >
      {t(bookmarked ? "removeBookmark" : "addBookmark")}
    </button>
  );
}

/** A star after a bookmarked object's name. */
function BookmarkStar({ bookmark }: { bookmark: BookmarkItem }) {
  const { t } = useTranslation("shell");
  return useBookmarked(bookmark) ? (
    <span className="bookmark-star" title={t("bookmarked")} aria-label={t("bookmarked")}>
      ★
    </span>
  ) : null;
}

const RELATION_KINDS = ["table", "partitioned_table", "view", "materialized_view", "foreign_table"];

/** Shows a bookmark's object in the tree, connecting first if needed: the user asked. */
function showBookmark(bookmark: BookmarkItem) {
  useApp.getState().revealInExplorer({
    connectionId: bookmark.connection,
    schema: bookmark.schema,
    name: bookmark.kind === "schema" ? undefined : bookmark.name,
    connect: true,
  });
}

/** Opens a bookmark's object (a table's rows, another object's DDL); a schema, or an object of
 *  a connection not connected in the explorer yet, is shown in the tree instead. */
async function openBookmark(bookmark: BookmarkItem) {
  if (bookmark.kind === "schema" || !useApp.getState().explorerConnected.includes(bookmark.connection)) {
    showBookmark(bookmark);
    return;
  }
  const { schema, name, kind } = bookmark;
  await openFound(
    bookmark.connection,
    RELATION_KINDS.includes(kind)
      ? { kind: "relation", object: { schema, name, kind, estimatedRows: null, comment: null } }
      : {
          kind: "schemaObject",
          object: { schema, name, kind: kind as SchemaObjectKindItem, detail: bookmark.detail || null, comment: null },
        },
  );
}

/** The bookmarks, above the connections: a click shows the object, a double-click opens it. */
function BookmarksSection() {
  const { t } = useTranslation("shell");
  const bookmarks = useApp((s) => s.bookmarks);
  if (bookmarks.length === 0) return null;
  return (
    <div className="bookmarks">
      <Folder label={t("bookmarks")} count={bookmarks.length} initiallyOpen>
        {bookmarks.map((b) => (
          <BookmarkRow key={`${b.connection}|${b.schema}|${b.name}|${b.kind}|${b.detail}`} bookmark={b} />
        ))}
      </Folder>
    </div>
  );
}

function BookmarkRow({ bookmark }: { bookmark: BookmarkItem }) {
  const { t } = useTranslation("shell");
  const connectionName = useApp((s) => s.connections.find((c) => c.id === bookmark.connection)?.name ?? bookmark.connection);
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);
  const label = bookmark.kind === "schema" ? bookmark.schema : bookmark.name;
  const where = bookmark.kind === "schema" ? connectionName : `${connectionName} · ${bookmark.schema}`;
  const open = () => {
    setMenu(null);
    setError(null);
    openBookmark(bookmark).catch((e: unknown) => setError(errorText(e)));
  };
  const remove = () => {
    setMenu(null);
    call(commands.removeBookmark(bookmark))
      .then(reloadBookmarks)
      .catch((e: unknown) => setError(errorText(e)));
  };
  return (
    <div className="node">
      <div
        className="row leaf-row"
        role="button"
        tabIndex={0}
        aria-label={t("bookmarkOpen", { name: label })}
        title={t("bookmarkHint")}
        onClick={(e) => {
          if (e.detail === 1) showBookmark(bookmark);
        }}
        onDoubleClick={open}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            e.preventDefault();
            open();
          } else if (isDropKey(e)) remove();
        }}
      >
        <span className="bookmark-star" aria-hidden="true">
          ★
        </span>
        <span className="label">{label}</span>
        <span className="muted small">{where}</span>
      </div>
      {menu && (
        <div className="grid-menu object-menu" role="menu" style={{ position: "fixed", left: menu.x, top: menu.y }}>
          <button role="menuitem" onClick={open}>
            {t("bookmarkOpenMenu")}
          </button>
          <button
            role="menuitem"
            onClick={() => {
              setMenu(null);
              showBookmark(bookmark);
            }}
          >
            {t("showInExplorer")}
          </button>
          <hr />
          <button role="menuitem" title="Delete" onClick={remove}>
            {t("removeBookmark")}
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

type TreeIconKind = "folder" | "table" | "view" | "columns" | "constraints" | "foreignKeys" | "indexes" | "triggers" | "column";

/** Small, neutral line icons distinguish explorer nodes without competing with their labels. */
function TreeIcon({ kind }: { kind: TreeIconKind }) {
  return (
    <svg className={`tree-icon tree-icon-${kind}`} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.35" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      {kind === "folder" && <path d="M1.8 4.1h4l1.3 1.4h7v6.8H1.8z" />}
      {(kind === "table" || kind === "view") && <><rect x="2" y="2.5" width="12" height="11" rx="1" /><path d="M2 6.2h12M6 6.2v7.3M10 6.2v7.3" /></>}
      {kind === "columns" && <><rect x="2.2" y="2.5" width="11.6" height="11" rx="1" /><path d="M6 3v10M10 3v10" /></>}
      {kind === "column" && <><rect x="4" y="2" width="8" height="12" rx="1" /><path d="M4 6h8M4 10h8" /></>}
      {kind === "constraints" && <><circle cx="6" cy="6" r="2.5" /><path d="m7.8 7.8 4.9 4.9m-1.5-1.5 1.4-1.4m-3 0 1.4-1.4" /></>}
      {kind === "foreignKeys" && <><path d="M6.5 10.8 5.3 12a2.5 2.5 0 0 1-3.5-3.5l2.3-2.3a2.5 2.5 0 0 1 3.5 0M9.5 5.2 10.7 4a2.5 2.5 0 0 1 3.5 3.5l-2.3 2.3a2.5 2.5 0 0 1-3.5 0M5.9 10.1l4.2-4.2" /></>}
      {kind === "indexes" && <><path d="M3 3.5h10M3 8h10M3 12.5h10" /><circle cx="5" cy="3.5" r="1" fill="var(--panel)" /><circle cx="10" cy="8" r="1" fill="var(--panel)" /><circle cx="6.5" cy="12.5" r="1" fill="var(--panel)" /></>}
      {kind === "triggers" && <path d="M8.9 1.8 3.7 8h3.7l-.4 6.2L12.4 7H8.7z" />}
    </svg>
  );
}

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
  icon = "folder",
  initiallyOpen = false,
  forceOpen = false,
  filtered = false,
  onConfigureFilter,
  onOpenView,
  create,
  reveal,
  children,
}: {
  label: string;
  count: number;
  icon?: TreeIconKind;
  initiallyOpen?: boolean;
  /** Shown open whatever its state (the quick filter shows what matches inside). */
  forceOpen?: boolean;
  /** The connection's filter hides some of what it holds (ADR-025). */
  filtered?: boolean;
  /** Its right-click menu offers "Configure filter…", as DBeaver's folders do. */
  onConfigureFilter?: () => void;
  /** Double-click opens an overview of this table's metadata. */
  onOpenView?: () => void;
  /** Its right-click menu offers "New …" of what it holds. */
  create?: { label: string; run(): void };
  /** It holds an object the explorer was asked to show: it opens. */
  reveal?: number;
  children: ReactNode;
}) {
  const { t, i18n } = useTranslation("shell");
  const [chosen, setOpen] = useState(initiallyOpen);
  const [menu, setMenu] = useContextMenu();
  const open = chosen || forceOpen;
  useEffect(() => {
    if (reveal !== undefined) setOpen(true);
  }, [reveal]);
  return (
    <div className="node">
      <div
        className="row folder-row"
        onContextMenu={(e) => {
          if (!onConfigureFilter && !create) return;
          e.preventDefault();
          setMenu({ x: e.clientX, y: e.clientY });
        }}
        onClick={() => { if (!onOpenView) setOpen(!open); }}
        onDoubleClick={onOpenView}
        title={onOpenView ? t("tableDetails.openHint") : undefined}
        role="button"
        tabIndex={0}
        aria-expanded={open}
        aria-label={t("toggleFolder", { name: label })}
        onKeyDown={(e) => {
          if (onOpenView && e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            e.stopPropagation();
            onOpenView();
            return;
          }
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            setOpen(!open);
          }
        }}
      >
        <span
          className="twisty"
          onClick={onOpenView ? (e) => { e.stopPropagation(); setOpen(!open); } : undefined}
          onDoubleClick={onOpenView ? (e) => e.stopPropagation() : undefined}
        >
          {open ? "▾" : "▸"}
        </span>
        <TreeIcon kind={icon} />
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
  const explorerTags = useApp((s) => s.explorerTags);
  /** Tags a connection must have to be shown: only while the quick filter is open. */
  const activeTags = filter === null ? [] : explorerTags;
  /** The tags offered in the quick filter: those in use, and any still selected. */
  const filterTags = tagsInUse(connections);
  for (const tag of activeTags) if (!filterTags.some((other) => sameTag(other, tag))) filterTags.push(tag);
  const filtering = filterText !== "" || activeTags.length > 0;
  const explorer = usePreferences((s) => s.explorer);
  const explorerConnected = useApp((s) => s.explorerConnected);
  const tabs = useApp((s) => s.tabs);
  const tabConnected = tabs.filter((tab) => tab.status === "connected").map((tab) => tab.connectionId);
  /** Opened in the explorer or used by a connected tab. */
  const connected = (c: ConnectionItem) => explorerConnected.includes(c.id) || tabConnected.includes(c.id);
  // "Link with editor": the active tab's connection, schema and table, shown in the tree.
  const activeTab = useApp((s) => s.tabs.find((tab) => tab.id === s.activeTab) ?? null);
  const linked = activeTab
    ? [activeTab.id, activeTab.connectionId, activeTab.status, activeTab.run?.running ?? false, activeTab.table?.schema, activeTab.table?.name].join("|")
    : "";
  useEffect(() => {
    const tab = useApp.getState().tabs.find((t) => t.id === useApp.getState().activeTab);
    if (!explorer.linkWithEditor || !tab?.connectionId) return;
    const connectionId = tab.connectionId;
    const reveal = useApp.getState().revealInExplorer;
    if (tab.table) return reveal({ connectionId, schema: tab.table.schema, name: tab.table.name });
    // A query tab: the schema its session is on, asked only while it is idle.
    if (tab.status !== "connected" || tab.run?.running) return reveal({ connectionId });
    let current = true;
    call(commands.activeSchema(tab.id))
      .then((schema) => {
        if (current) reveal(schema ? { connectionId, schema } : { connectionId });
      })
      .catch(() => {
        if (current) reveal({ connectionId });
      });
    return () => {
      current = false;
    };
  }, [explorer.linkWithEditor, linked]);
  /** With a filter, connections whose name matches, and those already expanded; with tags,
   * only connections that have them all (tags describe connections, not what is inside). */
  const shown = (c: ConnectionItem) =>
    (!explorer.connectedOnly || connected(c)) &&
    hasTags(c, activeTags) &&
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

  useEffect(() => {
    void reloadBookmarks().catch(() => undefined);
  }, [workspace?.workspace, connections]);

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
          className={`ghost icon-button${explorer.linkWithEditor ? " active" : ""}`}
          onClick={() => void usePreferences.getState().setExplorer({ ...explorer, linkWithEditor: !explorer.linkWithEditor })}
          aria-label={t("linkWithEditor")}
          aria-pressed={explorer.linkWithEditor}
          title={t("linkWithEditorHint")}
        >
          <LinkIcon />
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
          {filterTags.length > 0 && (
            <div className="tag-filter" role="group" aria-label={t("tagFilter")}>
              {filterTags.map((tag) => {
                const active = activeTags.some((other) => sameTag(other, tag));
                return (
                  <button
                    key={tag}
                    type="button"
                    className={`tag-filter-chip${active ? " active" : ""}`}
                    aria-pressed={active}
                    title={t("tagFilterHint", { tag })}
                    onClick={() => useApp.getState().toggleExplorerTag(tag)}
                  >
                    #{tag}
                  </button>
                );
              })}
            </div>
          )}
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
        <BookmarksSection />
        {connections.length > 0 && filtering && !connections.some(shown) && (
          <div className="empty small">{t("quickFilterNothing")}</div>
        )}
        {connections.length > 0 && !filtering && explorer.connectedOnly && !connections.some(shown) && (
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
  useEffect(() => {
    const refresh = (event: Event) => {
      if ((event as CustomEvent<string>).detail === connection.id && schemas) void refreshAll();
    };
    window.addEventListener(EXPLORER_REFRESH_EVENT, refresh);
    return () => window.removeEventListener(EXPLORER_REFRESH_EVENT, refresh);
  }, [connection.id, schemas]);

  function newQuery() {
    const tab = addTab(connection.id);
    if (schemas) connectTabFromExplorer(tab, session);
  }

  useEffect(() => {
    useApp.getState().setConnectionOpen(connection.id, open);
  }, [connection.id, open]);
  // Asked to show something here: expand if connected in the explorer (connecting could ask
  // for a password), else only show the connection.
  const { row: connectionRow, revealed: connectionRevealed } = useRevealedRow(
    (r) => r.connectionId === connection.id && (schemas === null ? !r.connect : r.schema === undefined),
  );
  const reveal = useApp((s) => s.explorerReveal);
  useEffect(() => {
    if (reveal?.connectionId !== connection.id) return;
    if (schemas !== null) setOpen(true);
    // A bookmark was opened: connecting is what the user asked for.
    else if (reveal.connect) void expand();
  }, [reveal, connection.id, schemas]);
  useEffect(
    () => () => {
      useApp.getState().setConnectionOpen(connection.id, false);
      useApp.getState().setExplorerConnected(connection.id, false);
    },
    [connection.id],
  );

  async function toggle() {
    if (open) return setOpen(false);
    await expand();
  }

  /** Opens the connection in the tree, connecting first if it is not yet. */
  async function expand() {
    setOpen(true);
    if (schemas || busy) return;
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
        ref={connectionRow}
        className={`row connection-row${connectionRevealed}`}
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
          title={connectionTooltip(connection)}
        >
          {connection.name}
        </span>
        {connection.tags.length > 0 && (
          // A few tags, quiet; the rest in the tooltip. A click filters the explorer by it.
          <span className="connection-tags" title={connection.tags.join(", ")}>
            {connection.tags.slice(0, VISIBLE_TAGS).map((tag) => (
              <button
                key={tag}
                type="button"
                className="connection-tag"
                aria-label={t("tagFilterHint", { tag })}
                onClick={() => useApp.getState().toggleExplorerTag(tag)}
              >
                #{tag}
              </button>
            ))}
            {connection.tags.length > VISIBLE_TAGS && (
              <span className="connection-tag-more">+{connection.tags.length - VISIBLE_TAGS}</span>
            )}
          </span>
        )}
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
          {schemas && (
            <button
              role="menuitem"
              title="Ctrl+Shift+D"
              onClick={() => {
                setRowMenu(null);
                useApp.getState().setOpenObject({ connectionId: connection.id });
              }}
            >
              {t("openByName.menu")}
            </button>
          )}
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
  /** The editor tab "Set as the editor's schema" applies to: the active query tab, connected to
   *  this connection and idle. */
  const editorTab = useApp((s) => {
    const tab = s.tabs.find((t) => t.id === s.activeTab);
    return tab && !tab.table && tab.connectionId === connectionId && tab.status === "connected" && !tab.run?.running
      ? tab.id
      : null;
  });

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

  const reveal = useApp((s) => s.explorerReveal);
  const revealHere = reveal !== null && reveal.connectionId === connectionId && reveal.schema === schema;
  const { row: schemaRow, revealed: schemaRevealed } = useRevealedRow(
    (r) => r.connectionId === connectionId && r.schema === schema && r.name === undefined,
  );
  useEffect(() => {
    if (!revealHere || !reveal) return;
    if (!open) setOpen(true);
    if (objects === null) {
      void load();
      return;
    }
    // Not here (filtered out, or gone): nothing more to show.
    if (reveal.name !== undefined && !objects.some((o) => o.name === reveal.name) && !others.some((o) => o.name === reveal.name)) {
      useApp.getState().revealDone(reveal.nonce);
    }
    // Only when asked, and once the objects are read.
  }, [reveal?.nonce, revealHere, objects]);
  /** The folder holding the object to show, which opens. */
  const revealFolder =
    revealHere && reveal?.name !== undefined && objects
      ? (() => {
          const relation = objects.find((o) => o.name === reveal.name);
          if (relation) return relationFolder(relation.kind);
          const other = others.find((o) => o.name === reveal.name);
          return other ? schemaObjectFolder(other.kind) : undefined;
        })()
      : undefined;

  // A schema whose name matches the quick filter shows everything in it; otherwise only what
  // matches, and it is hidden if it is closed and nothing loaded in it matches.
  const inner = matchesFilter(schema, filter) ? "" : filter;
  const triggersByTable = new Map<string, SchemaObjectItem[]>();
  for (const item of others) {
    if (item.kind !== "trigger" || item.detail === null) continue;
    const group = triggersByTable.get(item.detail) ?? [];
    group.push(item);
    triggersByTable.set(item.detail, group);
  }
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
        ref={schemaRow}
        className={`row${schemaRevealed}`}
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
        <TreeIcon kind="folder" />
        <span className="label">{schema}</span>
        <BookmarkStar bookmark={{ connection: connectionId, schema, name: "", kind: "schema", detail: "" }} />
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
          <BookmarkMenuItem
            bookmark={{ connection: connectionId, schema, name: "", kind: "schema", detail: "" }}
            close={() => setMenu(null)}
            onError={setError}
          />
          {actions?.activeSchema && (
            <button
              role="menuitem"
              disabled={editorTab === null}
              title={editorTab === null ? t("activeSchemaHint") : undefined}
              onClick={() => {
                setMenu(null);
                if (editorTab) {
                  window.dispatchEvent(new CustomEvent(SET_ACTIVE_SCHEMA_EVENT, { detail: { tabId: editorTab, schema } }));
                }
              }}
            >
              {t("setActiveSchema")}
            </button>
          )}
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
                    reveal={revealFolder === folder ? reveal?.nonce : undefined}
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
                        triggers={triggersByTable.get(o.name) ?? []}
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

function ObjectNode({
  session,
  connectionId,
  object,
  triggers,
  onSchemaChanged,
  generation,
  columnFilter,
  onConfigureFilter,
  onFilterName,
}: {
  session: string;
  connectionId: string;
  object: ObjectItem;
  triggers: SchemaObjectItem[];
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
  const [loadingDetails, setLoadingDetails] = useState(false);
  const [menu, setMenu] = useContextMenu();
  const sort = useSortByName();
  /** Exact count asked from the menu ("counting" while it runs). */
  const [count, setCount] = useState<number | "counting" | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [dropping, setDropping] = useState(false);
  const [generatedSql, setGeneratedSql] = useState<string | null>(null);
  const openDetails = (kind: TableDetailKind) => {
    const app = useApp.getState();
    const existing = app.tabs.some((tab) => tab.connectionId === connectionId && tab.tableDetails?.schema === object.schema && tab.tableDetails.name === object.name && tab.tableDetails.kind === kind);
    const tab = app.addTableDetailsTab(connectionId, object.schema, object.name, kind);
    if (!existing) {
      connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
    }
  };
  const actions = useObjectActions(session);
  const { row: objectRow, revealed: objectRevealed } = useRevealedRow(
    (r) => r.connectionId === connectionId && r.schema === object.schema && r.name === object.name,
  );
  /** Columns chosen with Ctrl+click, in the order chosen: a new index takes them so. */
  const [chosenColumns, setChosenColumns] = useState<string[]>([]);
  const chooseColumn = (name: string, add: boolean) =>
    setChosenColumns((list) => (!add ? [] : list.includes(name) ? list.filter((n) => n !== name) : [...list, name]));

  async function loadDetails() {
    setLoadingDetails(true);
    setError(null);
    try {
      setDetails(await call(commands.describeTable(session, object.schema, object.name, columnFilter)));
    } catch (e) {
      setDetails(null);
      setError(errorText(e));
    } finally {
      setLoadingDetails(false);
    }
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
    if (open) void loadDetails();
    else setDetails(null);
  }
  useOnChange(generation, refresh);

  /** Preview the core's SQL; Open as SQL still opens an editor tab. */
  async function openSql(template: SqlTemplateItem, inEditor = false) {
    setMenu(null);
    try {
      setError(null);
      const sql = await call(commands.tableSql(session, object.schema, object.name, template));
      if (inEditor) {
        const tab = addTab(connectionId, sql);
        connectTabFromExplorer(tab, session, explorerDatabase(connectionId, object.schema));
      } else setGeneratedSql(sql);
    } catch (e) {
      setError(errorText(e));
    }
  }
  /** Tables and views chosen with Ctrl+click in this session, this one among them or not. */
  const selection = useApp((s) => s.explorerSelection).filter((o) => o.session === session);
  const chosen = selection.some((o) => o.schema === object.schema && o.name === object.name);
  /** A SELECT joining the chosen tables on their foreign keys. */
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
      setGeneratedSql(sql);
    } catch (e) {
      setError(errorText(e));
    }
  }
  /** "Generate DDL": the driver writes the CREATE statement. */
  async function openDdl() {
    setMenu(null);
    try {
      setError(null);
      setGeneratedSql(await call(commands.relationDefinition(session, object.schema, object.name)));
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
  const bookmark: BookmarkItem = { connection: connectionId, schema: object.schema, name: object.name, kind: object.kind, detail: "" };
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
        ref={objectRow}
        className={`row${chosen ? " selected" : ""}${objectRevealed}`}
        aria-selected={chosen}
        onClick={(e) => {
          if (e.ctrlKey || e.metaKey) {
            useApp.getState().toggleExplorerSelection({ session, schema: object.schema, name: object.name });
            return;
          }
          useApp.getState().clearExplorerSelection();
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
        <span
          className="twisty"
          aria-hidden="true"
          onClick={(e) => {
            e.stopPropagation();
            void toggle();
          }}
          onDoubleClick={(e) => e.stopPropagation()}
        >
          {open ? "▾" : "▸"}
        </span>
        <span title={t(`kinds.${object.kind}`)}><TreeIcon kind={view ? "view" : "table"} /></span>
        <span className="label">{object.name}</span>
        <BookmarkStar bookmark={bookmark} />
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
          className="grid-menu object-menu explorer-relation-menu"
          role="menu"
          style={{
            position: "fixed",
            left: Math.max(8, Math.min(menu.x, window.innerWidth - 300)),
            top: Math.max(8, Math.min(menu.y, window.innerHeight - 250)),
            maxHeight: Math.min(window.innerHeight - Math.max(8, Math.min(menu.y, window.innerHeight - 250)) - 8, 480),
          }}
        >
          <button role="menuitem" onClick={preview}>
            {t("sql.preview")}
          </button>
          <button role="menuitem" onClick={() => void openSql("preview", true)}>
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
          <BookmarkMenuItem bookmark={bookmark} close={() => setMenu(null)} onError={setError} />
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
          <GenerateMenu>
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
          </GenerateMenu>
        </div>
      )}
      {error && (
        <p className="error small" role="alert">
          {error}
        </p>
      )}
      {open && loadingDetails && <div className="children muted small" role="status">{t("loadingDetails")}</div>}
      {generatedSql !== null && <GeneratedSqlDialog initialSql={generatedSql} onClose={() => setGeneratedSql(null)} />}
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
            icon="columns"
            onOpenView={() => openDetails("columns")}
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
                chosenColumns={chosenColumns}
                onChoose={(add) => chooseColumn(c.name, add)}
                hasPrimaryKey={details.primaryKey.length > 0}
                primary={details.primaryKey.includes(c.name)}
                onFilterName={onFilterName}
              />
            ))}
          </Folder>
          <Folder label={t("folders.constraints")} count={details.constraints.length} icon="constraints" onOpenView={() => openDetails("constraints")}>
            {sort(details.constraints, (c) => c.name ?? "").map((c, i) => (
              <LeafRow
                key={c.name ?? `${c.kind}:${i}`}
                session={session}
                connectionId={connectionId}
                schema={object.schema}
                label={c.name ?? t(`constraintKinds.${c.kind}`)}
                detail={c.definition}
                icon="constraints"
                target={
                  c.name != null && actions?.dropConstraints
                    ? { kind: "constraint", schema: object.schema, table: object.name, name: c.name, constraint: c.kind }
                    : null
                }
                onChanged={refresh}
              />
            ))}
          </Folder>
          <Folder label={t("folders.foreignKeys")} count={details.foreignKeys.length} icon="foreignKeys" onOpenView={() => openDetails("foreignKeys")}>
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
                  icon="foreignKeys"
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
          <Folder label={t("folders.indexes")} count={details.indexes.length} icon="indexes" onOpenView={() => openDetails("indexes")}>
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
          <Folder label={t("folders.triggers")} count={triggers.length} icon="triggers" onOpenView={() => openDetails("triggers")}>
            {sort(triggers, (trigger) => trigger.name).map((trigger) => (
              <SchemaObjectNode
                key={`${trigger.name}:${trigger.detail ?? ""}`}
                session={session}
                connectionId={connectionId}
                object={trigger}
                onSchemaChanged={onSchemaChanged}
                onFilterName={onFilterName}
              />
            ))}
          </Folder>
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
  chosenColumns,
  onChoose,
  hasPrimaryKey,
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
  /** The table's columns chosen with Ctrl+click, in order. */
  chosenColumns: string[];
  /** Ctrl+click adds or removes this column; a plain click forgets the choice. */
  onChoose(add: boolean): void;
  hasPrimaryKey: boolean;
  primary: boolean;
  onFilterName: FilterName;
}) {
  const { t } = useTranslation("shell");
  const [menu, setMenu] = useContextMenu();
  const [error, setError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState(false);
  const [dropping, setDropping] = useState(false);
  const actions = useObjectActions(session);
  const paths = [{ schema, names: [table, column.name] }];
  const chosen = chosenColumns.includes(column.name);
  /** What a new index takes: the chosen columns if this is one of them, else this one. */
  const indexColumns = chosen ? chosenColumns : [column.name];
  async function openNewIndex(kind: NewIndexKindItem) {
    setMenu(null);
    setError(null);
    try {
      openSqlTab(connectionId, session, schema, await call(commands.newIndexSql(session, schema, table, indexColumns, kind)));
    } catch (e) {
      setError(errorText(e));
    }
  }
  return (
    <div className="node">
      <div
        className={`row leaf-row${chosen ? " selected" : ""}`}
        aria-selected={chosen}
        onClick={(e) => onChoose(e.ctrlKey || e.metaKey)}
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
        <TreeIcon kind={primary ? "constraints" : "column"} />
        <span className="label">
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
          {actions && actions.newIndex.length > 0 && <hr />}
          {actions?.newIndex
            .filter((kind) => kind !== "primaryKey" || !hasPrimaryKey)
            .map((kind) => (
              <button key={kind} role="menuitem" onClick={() => void openNewIndex(kind)}>
                {t(`newIndex.${kind}`, { columns: indexColumns.join(", ") })}
              </button>
            ))}
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
        <TreeIcon kind="indexes" />
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
  const { row, revealed } = useRevealedRow(
    (r) => r.connectionId === connectionId && r.schema === object.schema && r.name === object.name,
  );
  const bookmark: BookmarkItem = {
    connection: connectionId,
    schema: object.schema,
    name: object.name,
    kind: object.kind,
    detail: object.detail ?? "",
  };

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
        ref={row}
        className={`row leaf-row${revealed}`}
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
        <TreeIcon kind={object.kind === "trigger" ? "triggers" : "folder"} />
        <span className="label">
          {object.name}
          {routine && object.detail !== null && (
            <span className="muted">({object.detail})</span>
          )}
        </span>
        <BookmarkStar bookmark={bookmark} />
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
          <BookmarkMenuItem bookmark={bookmark} close={() => setMenu(null)} onError={setError} />
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
