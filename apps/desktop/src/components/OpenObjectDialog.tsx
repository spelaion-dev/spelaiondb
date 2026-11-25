// "Open object by name" (Ctrl+Shift+D, DBeaver's Open Database Object): searches the
// connections connected in the explorer and opens what is chosen, showing it in the tree.

import { useEffect, useRef, useState } from "react";
import { explorerSession } from "../windowContext";
import { connectionKey, connectionOfTag, connectionTag, holdsConnection, keyOf, listedConnection, tabKey, type ConnectionKey } from "../connectionKey";
import { useTranslation } from "react-i18next";
import { call, commands, type FoundObjectItem } from "../api";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { usePreferences } from "../preferences";
import { topDialog } from "../modalFocus";
import { useApp } from "../store";
import { keyText } from "../shortcuts";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Opens what was found: a table or view in a table data tab, anything else as its DDL. */
export async function openFound(connection: ConnectionKey, found: FoundObjectItem) {
  const session = explorerSession(connection);
  const app = useApp.getState();
  const { schema, name } = found.object;
  const database = explorerDatabase(connection, schema);
  const sql = listedConnection(app.connections, connection)?.sql ?? true;
  const connectionId = tabKey(connection);
  if (found.kind === "relation" && (found.object.kind === "collection" || !sql)) {
    // An engine whose language is not SQL (ADR-056): the core's statement for it, run in a tab.
    // Its tables too (CQL's): the table data tab writes SQL.
    const statement = await call(commands.browseStatement(session, schema, name, found.object.kind, found.object.comment ?? null));
    if (statement !== null) {
      const tab = app.addTab(connectionId, statement);
      app.updateTab(tab, { runOnOpen: true });
      connectTabFromExplorer(tab, session, database);
    }
  } else if (found.kind === "relation") {
    connectTabFromExplorer(app.addTableTab(connectionId, schema, name), session, database);
  } else {
    const sql = await call(commands.objectDefinition(session, found.object));
    connectTabFromExplorer(app.addTab(connectionId, sql), session, database);
  }
  app.revealInExplorer({ connection, schema, name });
}

export function OpenObjectDialog({ initialConnection, onClose }: { initialConnection: ConnectionKey | null; onClose(): void }) {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  const connected = useApp((s) => s.explorerConnected);
  const activeConnection = useApp((s) => s.tabs.find((tab) => tab.id === s.activeTab)?.connectionId ?? null);
  const choices = connections.filter((c) => holdsConnection(connected, keyOf(c)));
  const [connection, setConnection] = useState<ConnectionKey | null>(() => {
    const active = activeConnection === null ? null : connectionKey(activeConnection);
    for (const key of [initialConnection, active]) if (key && holdsConnection(connected, key)) return key;
    return choices[0] ? keyOf(choices[0]) : null;
  });
  const [text, setText] = useState("");
  const [found, setFound] = useState<FoundObjectItem[]>([]);
  const [active, setActive] = useState(0);
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const list = useRef<HTMLUListElement>(null);

  // Searched as it is typed; an answer to older text is ignored.
  useEffect(() => {
    if (!connection || text.trim() === "") {
      setFound([]);
      setSearching(false);
      return;
    }
    let current = true;
    setSearching(true);
    const timer = window.setTimeout(() => {
      void (async () => {
        try {
          // The tree's own view: its filter and whether it shows system schemas (ADR-025).
          const filter = await call(commands.getExplorerFilter(connection));
          const showSystem = usePreferences.getState().explorer.showSystem;
          const answer = await call(
            commands.searchObjects(explorerSession(connection), text, filter.schemas, filter.objects, showSystem),
          );
          if (!current) return;
          setFound(answer);
          setActive(0);
          setError(null);
        } catch (e) {
          if (current) setError(errorText(e));
        } finally {
          if (current) setSearching(false);
        }
      })();
    }, 200);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [connection, text]);

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active]);

  function choose(item: FoundObjectItem | undefined) {
    if (!item || !connection) return;
    openFound(connection, item).then(onClose, (e: unknown) => setError(errorText(e)));
  }

  const kindLabel = (item: FoundObjectItem) => t(`kinds.${item.object.kind}`);
  return (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div
        className="modal open-object-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={t("openByName.title")}
        onKeyDown={(e) => {
          if (e.key === "Escape" && topDialog() === e.currentTarget) {
            e.preventDefault();
            e.stopPropagation();
            onClose();
          } else if (e.target instanceof HTMLInputElement && e.key === "ArrowDown") {
            e.preventDefault();
            setActive((i) => Math.min(i + 1, found.length - 1));
          } else if (e.target instanceof HTMLInputElement && e.key === "ArrowUp") {
            e.preventDefault();
            setActive((i) => Math.max(i - 1, 0));
          } else if (e.target instanceof HTMLInputElement && e.key === "Enter") {
            e.preventDefault();
            choose(found[active]);
          }
        }}
      >
        <h2>{t("openByName.title")}</h2>
        {choices.length === 0 ? (
          <>
            <p className="muted small">{t("openByName.noneConnected")}</p>
            <div className="modal-actions">
              <button autoFocus onClick={onClose}>{t("app:tabs.close")}</button>
            </div>
          </>
        ) : (
          <>
            <div className="open-object-search">
              <input
                type="search"
                autoFocus
                spellCheck={false}
                value={text}
                placeholder={t("openByName.placeholder")}
                aria-label={t("openByName.name")}
                aria-controls="open-object-results"
                onChange={(e) => setText(e.target.value)}
              />
              {choices.length > 1 && (
                <select
                  aria-label={t("openByName.connection")}
                  value={connection ? connectionTag(connection) : ""}
                  onChange={(e) => setConnection(connectionOfTag(e.target.value))}
                >
                  {choices.map((c) => (
                    <option key={connectionTag(keyOf(c))} value={connectionTag(keyOf(c))}>
                      {c.name}
                    </option>
                  ))}
                </select>
              )}
            </div>
            {error && (
              <p className="error small" role="alert">
                {error}
              </p>
            )}
            <ul id="open-object-results" className="open-object-results" role="listbox" ref={list} aria-label={t("openByName.results")}>
              {found.map((item, i) => (
                <li
                  key={`${item.kind}:${item.object.schema}:${item.object.name}:${item.kind === "schemaObject" ? `${item.object.kind}:${item.object.detail ?? ""}` : ""}`}
                  role="option"
                  aria-selected={i === active}
                  onMouseEnter={() => setActive(i)}
                  onClick={() => choose(item)}
                >
                  <span className="label">{item.object.name}</span>
                  <span className="muted small">
                    {kindLabel(item)} · {item.object.schema}
                  </span>
                </li>
              ))}
            </ul>
            <p className="muted small" role="status">
              {searching
                ? t("openByName.searching")
                : text.trim() !== "" && found.length === 0 && !error
                  ? t("openByName.nothing")
                  : t("openByName.hint", { enter: keyText("submit") })}
            </p>
          </>
        )}
      </div>
    </div>
  );
}
