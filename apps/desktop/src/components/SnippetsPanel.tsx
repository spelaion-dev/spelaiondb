// Saved queries: named SQL kept in the local store (ADR-011), for the open workspace and
// the personal ones. Opening one never connects or runs anything.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type SnippetItem } from "../api";
import { useApp } from "../store";

export function SnippetsPanel() {
  const { t, i18n } = useTranslation("editor");
  const dates = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "short", timeStyle: "short" }), [i18n.language]);
  const revision = useApp((s) => s.snippetsRevision);
  const workspace = useApp((s) => s.workspace?.workspace ?? null);
  const connections = useApp((s) => s.connections);
  const close = () => useApp.getState().setSnippetsOpen(false);
  const [search, setSearch] = useState("");
  const [items, setItems] = useState<SnippetItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<number | null>(null);
  const generation = useRef(0);

  useEffect(() => {
    const current = ++generation.current;
    const timer = setTimeout(() => {
      call(commands.listSnippets(search))
        .then((list) => current === generation.current && (setItems(list), setError(null)))
        .catch((e: unknown) => current === generation.current && setError(e instanceof Error ? e.message : String(e)));
    }, 150);
    return () => clearTimeout(timer);
  }, [search, revision, workspace]);

  function open(item: SnippetItem) {
    const app = useApp.getState();
    const connection = connections.find((c) => c.id === item.connection);
    const id = app.addTab(connection?.id ?? null, item.statement);
    app.updateTab(id, { snippetName: item.name });
    close();
  }

  async function remove(item: SnippetItem) {
    setConfirming(null);
    try {
      await call(commands.deleteSnippet(item.id));
      useApp.getState().refreshSnippets();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  return (
    <aside className="history-panel snippets-panel" id="saved-queries" aria-label={t("snippets.title")}>
      <div className="history-header">
        <h2>{t("snippets.title")}</h2>
        <button className="ghost" onClick={close} aria-label={t("snippets.close")}>
          ×
        </button>
      </div>
      <div className="history-search">
        <input
          type="text"
          autoFocus
          value={search}
          onChange={(e) => setSearch(e.target.value)}
          placeholder={t("snippets.searchPlaceholder")}
          aria-label={t("snippets.searchLabel")}
          onKeyDown={(e) => e.key === "Escape" && close()}
        />
        <p className="muted small">{t("snippets.help")}</p>
      </div>
      <div className="history-entries">
        {items && items.length === 0 && <div className="empty">{t(search.trim() ? "snippets.noMatches" : "snippets.empty")}</div>}
        {items?.map((item) => {
          const connection = connections.find((c) => c.id === item.connection);
          return (
            <article className="history-entry snippet-entry" key={item.id}>
              <div className="history-entry-heading">
                <strong className="snippet-name">{item.name}</strong>
                {item.personal && <span className="tag">{t("snippets.personal")}</span>}
              </div>
              <div className="history-meta muted small">
                <span>{connection?.name ?? (item.connection ? item.connection : t("snippets.noConnection"))}</span>
                <time dateTime={new Date(item.updatedMs).toISOString()}>{dates.format(item.updatedMs)}</time>
              </div>
              <pre className="history-sql">{item.statement}</pre>
              <div className="snippet-actions">
                <button onClick={() => open(item)}>{t("snippets.open")}</button>
                {confirming === item.id ? (
                  <button className="danger" onClick={() => void remove(item)}>
                    {t("snippets.confirmDelete")}
                  </button>
                ) : (
                  <button className="ghost" onClick={() => setConfirming(item.id)}>
                    {t("snippets.delete")}
                  </button>
                )}
              </div>
            </article>
          );
        })}
        {error && (
          <p className="error" role="alert">
            {error}
          </p>
        )}
      </div>
    </aside>
  );
}
