// "Open object by name" (Ctrl+Shift+D, DBeaver's Open Database Object): searches the
// connections connected in the explorer and opens what is chosen, showing it in the tree.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type FoundObjectItem } from "../api";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { usePreferences } from "../preferences";
import { useApp } from "../store";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Opens what was found: a table or view in a table data tab, anything else as its DDL. */
export async function openFound(connectionId: string, found: FoundObjectItem) {
  const session = `meta:${connectionId}`;
  const app = useApp.getState();
  const { schema, name } = found.object;
  const database = explorerDatabase(connectionId, schema);
  if (found.kind === "relation") {
    connectTabFromExplorer(app.addTableTab(connectionId, schema, name), session, database);
  } else {
    const sql = await call(commands.objectDefinition(session, found.object));
    connectTabFromExplorer(app.addTab(connectionId, sql), session, database);
  }
  app.revealInExplorer({ connectionId, schema, name });
}

export function OpenObjectDialog({ initialConnection, onClose }: { initialConnection: string | null; onClose(): void }) {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  const connected = useApp((s) => s.explorerConnected);
  const activeConnection = useApp((s) => s.tabs.find((tab) => tab.id === s.activeTab)?.connectionId ?? null);
  const choices = connections.filter((c) => connected.includes(c.id));
  const [connectionId, setConnectionId] = useState(() => {
    for (const id of [initialConnection, activeConnection]) if (id && connected.includes(id)) return id;
    return choices[0]?.id ?? "";
  });
  const [text, setText] = useState("");
  const [found, setFound] = useState<FoundObjectItem[]>([]);
  const [active, setActive] = useState(0);
  const [searching, setSearching] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const list = useRef<HTMLUListElement>(null);

  // Searched as it is typed; an answer to older text is ignored.
  useEffect(() => {
    if (!connectionId || text.trim() === "") {
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
          const filter = await call(commands.getExplorerFilter(connectionId));
          const showSystem = usePreferences.getState().explorer.showSystem;
          const answer = await call(
            commands.searchObjects(`meta:${connectionId}`, text, filter.schemas, filter.objects, showSystem),
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
  }, [connectionId, text]);

  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active]);

  function choose(item: FoundObjectItem | undefined) {
    if (!item) return;
    openFound(connectionId, item).then(onClose, (e: unknown) => setError(errorText(e)));
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
          if (e.key === "Escape") {
            e.preventDefault();
            onClose();
          } else if (e.key === "ArrowDown") {
            e.preventDefault();
            setActive((i) => Math.min(i + 1, found.length - 1));
          } else if (e.key === "ArrowUp") {
            e.preventDefault();
            setActive((i) => Math.max(i - 1, 0));
          } else if (e.key === "Enter") {
            e.preventDefault();
            choose(found[active]);
          }
        }}
      >
        <h2>{t("openByName.title")}</h2>
        {choices.length === 0 ? (
          <p className="muted small">{t("openByName.noneConnected")}</p>
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
                  value={connectionId}
                  onChange={(e) => setConnectionId(e.target.value)}
                >
                  {choices.map((c) => (
                    <option key={c.id} value={c.id}>
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
                  : t("openByName.hint")}
            </p>
          </>
        )}
      </div>
    </div>
  );
}
