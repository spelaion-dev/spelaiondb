// Side panel: open repository, connections (by folder) and schema explorer.

import { useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { call, commands, type ConnectionItem, type ObjectItem, type SchemaItem, type TableDetails } from "../api";
import { openSession } from "../connect";
import { useApp, useDialogs } from "../store";
import { quoteIdent } from "../text";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export async function refreshConnections() {
  const app = useApp.getState();
  app.setConnections(await call(commands.listConnections()));
  app.setWorkspace(await call(commands.workspaceInfo()));
}

export function Sidebar() {
  const { t, i18n } = useTranslation("shell");
  const workspace = useApp((s) => s.workspace);
  const connections = useApp((s) => s.connections);
  const historyOpen = useApp((s) => s.historyOpen);
  const setHistoryOpen = useApp((s) => s.setHistoryOpen);
  const cannotSwitch = useApp((s) => s.workspaceChanging || s.historySaving.length > 0 || s.tabs.some((t) => t.status === "connecting" || t.run?.running));
  const [showProblems, setShowProblems] = useState(false);
  const [folderError, setFolderError] = useState<string | null>(null);

  async function openFolder() {
    const path = await open({ directory: true, title: t("openFolderTitle") });
    if (typeof path !== "string") return;
    const app = useApp.getState();
    // A connection ID can refer to a different server in the next folder. Keep the SQL,
    // but make the user choose a connection again before executing any existing tab.
    if (app.tabs.some((t) => t.status === "connecting" || t.run?.running) || app.historySaving.length > 0) return;
    app.setWorkspaceChanging(true);
    setFolderError(null);
    try {
      const sessions = [...app.tabs.map((t) => t.id), ...app.connections.map((c) => `meta:${c.id}`)];
      app.clearTabConnections();
      app.setConnections([]);
      await Promise.all(sessions.map((session) => call(commands.disconnect(session))));
      await Promise.all(app.tabs.flatMap((tab) => (tab.run?.results ?? []).map((r) => call(commands.releaseResult(r.id)))));
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
  const groups = new Map<string, ConnectionItem[]>([[JSON.stringify([false]), []]]);
  for (const c of connections) {
    const key = JSON.stringify([c.personal, ...c.folders]);
    groups.set(key, [...(groups.get(key) ?? []), c]);
  }

  const folderName = workspace?.workspace?.split(/[\\/]/).filter(Boolean).pop();
  return (
    <aside className="sidebar">
      <div className="sidebar-header">
        <div className="workspace-name" title={workspace?.workspace ?? ""}>
          {folderName ?? t("noFolder")}
        </div>
        <button className="ghost" onClick={() => void openFolder()} title={t("openAnotherFolder")} disabled={cannotSwitch}>
          {t("openFolder")}
        </button>
      </div>
      {folderError && <div className="banner" role="alert">{folderError}</div>}
      {workspace?.hasWorkspaceConnections && !workspace.trusted && (
        <div className="banner">
          {t("repositoryUntrusted")}
          <button onClick={() => void trust()}>{t("reviewAndTrust")}</button>
        </div>
      )}
      {workspace && workspace.problems.length > 0 && (
        <div className="banner banner-warn">
          <button className="link" onClick={() => setShowProblems(!showProblems)}>
            {t("configurationErrors", { count: workspace.problems.length, displayCount: workspace.problems.length.toLocaleString(i18n.language) })}
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
            <Trans t={t} i18nKey="noConnections" components={{ code: <code /> }} />
          </div>
        )}
        {[...groups.entries()].map(([group, items]) => {
          const first = items[0];
          const label = first ? (first.personal ? [t("personal")] : []).concat(first.folders).join(" / ") : "";
          return (
            <div key={group}>
              {label && <div className="group-title">{label}</div>}
              {items.map((c) => (
                <ConnectionNode key={`${workspace?.workspace ?? ""}:${c.id}`} connection={c} />
              ))}
            </div>
          );
        })}
      </div>
      <div className="sidebar-footer">
        <button aria-pressed={historyOpen} aria-controls="query-history" onClick={() => setHistoryOpen(!historyOpen)}>
          {t("queryHistory")}
        </button>
      </div>
    </aside>
  );
}

function ConnectionNode({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [open, setOpen] = useState(false);
  const [schemas, setSchemas] = useState<SchemaItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const session = `meta:${connection.id}`;

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
      setSchemas((await call(commands.listSchemas(session))).filter((s) => !s.system));
    } catch (e) {
      setError(errorText(e));
    } finally {
      setBusy(false);
    }
  }

  const prod = connection.env?.toLowerCase().startsWith("prod");
  return (
    <div className="node">
      <div className="row connection-row">
        <button className="twisty" onClick={() => void toggle()} aria-expanded={open} aria-label={t("toggleConnection", { connection: connection.name })}>
          {open ? "▾" : "▸"}
        </button>
        <span className={`engine engine-${connection.engine}`}>{connection.engine === "postgres" ? "PG" : "SQ"}</span>
        <span className="label" onDoubleClick={() => addTab(connection.id)} title={connection.id}>
          {connection.name}
        </span>
        {connection.tunnel && <span className="tag" title={t("tunnel", { name: connection.tunnel })}>SSH</span>}
        {prod && <span className="tag tag-prod">prod</span>}
        <button className="ghost small" onClick={() => addTab(connection.id)} title={t("newQuery")}>
          +
        </button>
      </div>
      {open && (
        <div className="children">
          {busy && <div className="muted small">{t("connecting")}</div>}
          {error && <div className="error small">{error}</div>}
          {schemas?.map((s) => (
            <SchemaNode key={s.name} session={session} connectionId={connection.id} schema={s.name} />
          ))}
        </div>
      )}
    </div>
  );
}

function SchemaNode({ session, connectionId, schema }: { session: string; connectionId: string; schema: string }) {
  const { t, i18n } = useTranslation("shell");
  const [open, setOpen] = useState(false);
  const [objects, setObjects] = useState<ObjectItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function toggle() {
    setOpen(!open);
    if (open || objects) return;
    try {
      setObjects(await call(commands.listObjects(session, schema)));
    } catch (e) {
      setError(errorText(e));
    }
  }

  return (
    <div className="node">
      <div className="row" onClick={() => void toggle()} role="button" tabIndex={0} aria-expanded={open} aria-label={t("toggleSchema", { name: schema })} onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          void toggle();
        }
      }}>
        <span className="twisty">{open ? "▾" : "▸"}</span>
        <span className="label">{schema}</span>
        {objects && <span className="muted small">{objects.length.toLocaleString(i18n.language)}</span>}
      </div>
      {open && (
        <div className="children">
          {error && <div className="error small">{error}</div>}
          {objects?.map((o) => (
            <ObjectNode key={o.name} session={session} connectionId={connectionId} object={o} />
          ))}
        </div>
      )}
    </div>
  );
}

function ObjectNode({ session, connectionId, object }: { session: string; connectionId: string; object: ObjectItem }) {
  const { t, i18n } = useTranslation("shell");
  const addTab = useApp((s) => s.addTab);
  const [open, setOpen] = useState(false);
  const [details, setDetails] = useState<TableDetails | null>(null);

  async function toggle() {
    setOpen(!open);
    if (open || details) return;
    setDetails(await call(commands.describeTable(session, object.schema, object.name)).catch(() => null));
  }

  const preview = () =>
    addTab(connectionId, `select *\nfrom ${quoteIdent(object.schema)}.${quoteIdent(object.name)}\nlimit 100;\n`);
  const view = object.kind.includes("view");
  return (
    <div className="node">
      <div className="row" onClick={() => void toggle()} onDoubleClick={preview} title={object.comment ?? t("viewRowsHint")} role="button" tabIndex={0} aria-expanded={open} aria-label={t("toggleObject", { name: object.name })} onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          void toggle();
        }
      }}>
        <span className="twisty">{open ? "▾" : "▸"}</span>
        <span className={`kind ${view ? "kind-view" : "kind-table"}`}>{view ? "V" : "T"}</span>
        <span className="label">{object.name}</span>
        {object.estimatedRows != null && (
          <span className="muted small">~{object.estimatedRows.toLocaleString(i18n.language)}</span>
        )}
      </div>
      {open && details && (
        <div className="children">
          {details.columns.map((c) => (
            <div key={c.name} className="row column-row" title={c.comment ?? ""}>
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
        </div>
      )}
    </div>
  );
}
