// One SQL-filtered page at a time. Opening a saved query never connects or executes.
import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type SnippetItem } from "../api";
import type { SnippetFolderItem } from "../bindings";
import { useApp } from "../store";
import { Icon } from "./Icons";

const PAGE_SIZE = 50;
const errorText = (reason: unknown) => reason instanceof Error ? reason.message : String(reason);
type Dialog = { kind: "folder" } | { kind: "delete"; item: SnippetItem };

export function SnippetsPanel() {
  const { t } = useTranslation("editor");
  const revision = useApp((s) => s.snippetsRevision);
  const workspace = useApp((s) => s.workspace?.workspace ?? null);
  const connections = useApp((s) => s.connections);
  const close = () => useApp.getState().setSnippetsOpen(false);
  const [search, setSearch] = useState(""); const [query, setQuery] = useState("");
  const [folder, setFolder] = useState("all"); const [folders, setFolders] = useState<SnippetFolderItem[]>([]);
  const [items, setItems] = useState<SnippetItem[]>([]); const [offset, setOffset] = useState(0); const [more, setMore] = useState(false);
  const [loading, setLoading] = useState(true); const [error, setError] = useState<string | null>(null); const [refresh, setRefresh] = useState(0);
  const [moving, setMoving] = useState<number | null>(null); const [dialog, setDialog] = useState<Dialog | null>(null);
  const [name, setName] = useState(""); const [saving, setSaving] = useState(false); const [dialogError, setDialogError] = useState<string | null>(null);
  const folderName = useRef<HTMLInputElement>(null); const dialogCancel = useRef<HTMLButtonElement>(null);
  const generation = useRef(0); const busy = loading || search.trim() !== query;
  useEffect(() => {
    // Disabling the focused control while saving sends focus to the document body.
    // Restore it after a failed action so keyboard retry and Escape stay in the dialog.
    if (dialogError && !saving) (dialog?.kind === "folder" ? folderName.current : dialogCancel.current)?.focus();
  }, [dialogError, saving, dialog]);
  useEffect(() => { const timer = window.setTimeout(() => setQuery(search.trim()), 200); return () => window.clearTimeout(timer); }, [search]);
  useEffect(() => { setFolder("all"); setDialog(null); }, [workspace]);
  useEffect(() => { setOffset(0); }, [query, folder, revision, workspace, refresh]);
  useEffect(() => {
    const current = ++generation.current; setLoading(true); setError(null); setItems([]); setMore(false);
    void Promise.all([call(commands.snippetPage(query, folder === "all" || folder === "unfiled" ? null : Number(folder), folder === "unfiled", PAGE_SIZE + 1, offset)), call(commands.snippetFolders())])
      .then(([page, choices]) => { if (current !== generation.current) return; setItems(page.slice(0, PAGE_SIZE)); setMore(page.length > PAGE_SIZE); setFolders(choices); if (!page.length && offset > 0) setOffset(Math.max(0, offset - PAGE_SIZE)); })
      .catch((reason: unknown) => { if (current === generation.current) setError(t("snippets.loadFailed", { reason: errorText(reason) })); })
      .finally(() => { if (current === generation.current) setLoading(false); });
    return () => { generation.current += 1; };
  }, [query, folder, offset, revision, workspace, refresh]);
  function open(item: SnippetItem) { const app = useApp.getState(); const id = app.addTab(connections.find((connection) => connection.id === item.connection)?.id ?? null, item.statement); app.updateTab(id, { snippetName: item.name }); close(); }
  async function move(item: SnippetItem, folderId: string) {
    setMoving(item.id); setError(null);
    try { await call(commands.moveSnippet(item.id, folderId ? Number(folderId) : null)); setRefresh((value) => value + 1); }
    catch (reason) { setError(t("snippets.moveFailed", { reason: errorText(reason) })); } finally { setMoving(null); }
  }
  async function submit() {
    if (!dialog || saving) return; setSaving(true); setDialogError(null);
    try { if (dialog.kind === "folder") await call(commands.createSnippetFolder(name)); else await call(commands.deleteSnippet(dialog.item.id)); setDialog(null); setRefresh((value) => value + 1); }
    catch (reason) { setDialogError(errorText(reason)); } finally { setSaving(false); }
  }
  return <>
    <aside className="history-panel snippets-panel library-panel" id="saved-queries" aria-label={t("snippets.title")} onKeyDown={(event) => { if (event.key === "Escape" && !dialog) close(); }}>
      <div className="history-header"><h2>{t("snippets.title")}</h2><button className="ghost" onClick={() => { setName(""); setDialogError(null); setDialog({ kind: "folder" }); }}>{t("snippets.newFolder")}</button><button className="ghost icon-button" onClick={close} aria-label={t("snippets.close")} title={t("snippets.close")}><Icon name="x" /></button></div>
      <div className="history-search library-filters"><input type="text" autoFocus value={search} onChange={(event) => setSearch(event.target.value)} placeholder={t("snippets.searchPlaceholder")} aria-label={t("snippets.searchLabel")} /><label>{t("snippets.folder")}<select aria-label={t("snippets.folder")} value={folder} onChange={(event) => setFolder(event.target.value)}><option value="all">{t("snippets.allFolders")}</option><option value="unfiled">{t("snippets.unfiled")}</option>{folders.map((choice) => <option key={choice.id} value={choice.id}>{choice.name}{choice.personal ? ` · ${t("snippets.personal")}` : ""}</option>)}</select></label><p className="muted small">{t("snippets.help")}</p></div>
      {error && <div className="history-fetch-error banner library-alert" role="alert"><p>{error}</p><button onClick={() => setRefresh((value) => value + 1)} disabled={busy}>{t("history.retry")}</button></div>}
      <div className="history-entries" aria-busy={busy}>
        {!busy && !error && items.length === 0 && <div className="empty">{t(query || folder !== "all" ? "snippets.noMatches" : "snippets.empty")}</div>}
        {items.map((item) => <article className="history-entry snippet-entry" key={item.id}>
          <div className="history-entry-heading"><strong className="snippet-name">{item.name}</strong>{item.personal && <span className="tag">{t("snippets.personal")}</span>}</div>
          <div className="history-meta muted small"><span>{connections.find((connection) => connection.id === item.connection)?.name ?? (item.connection || t("snippets.noConnection"))}</span><span>{folders.find((choice) => choice.id === item.folderId)?.name ?? t("snippets.unfiled")}</span></div>
          <pre className="history-sql">{item.statement}</pre><div className="library-row-actions"><button onClick={() => open(item)}>{t("snippets.open")}</button><details className="library-organize"><summary>{t("snippets.organize")}</summary><label>{t("snippets.moveTo")}<select aria-label={t("snippets.moveQuery", { name: item.name })} value={item.folderId ?? ""} disabled={moving !== null || busy} onChange={(event) => void move(item, event.target.value)}><option value="">{t("snippets.unfiled")}</option>{folders.filter((choice) => choice.personal === item.personal).map((choice) => <option key={choice.id} value={choice.id}>{choice.name}</option>)}</select></label><button className="ghost" onClick={() => { setDialogError(null); setDialog({ kind: "delete", item }); }}>{t("snippets.delete")}</button></details></div>
        </article>)}
        {busy && <div className="empty small" role="status">{t("snippets.loading")}</div>}
      </div>
      <div className="library-paging"><button disabled={busy || offset === 0} onClick={() => setOffset(Math.max(0, offset - PAGE_SIZE))}>{t("history.previous")}</button><span className="muted small">{t("history.page", { page: Math.floor(offset / PAGE_SIZE) + 1 })}</span><button disabled={busy || !more} onClick={() => setOffset(offset + PAGE_SIZE)}>{t("history.next")}</button></div>
    </aside>
    {dialog && <div className="modal-backdrop library-backdrop" onKeyDown={(event) => { if (event.key === "Escape" && !saving) { event.stopPropagation(); setDialog(null); } }}><form className="modal library-dialog" role="dialog" aria-modal="true" aria-labelledby="library-dialog-title" onSubmit={(event) => { event.preventDefault(); void submit(); }}>
      <h2 id="library-dialog-title">{t(dialog.kind === "folder" ? "snippets.newFolderTitle" : "snippets.deleteTitle", { name: dialog.kind === "delete" ? dialog.item.name : "" })}</h2>
      {dialog.kind === "folder" ? <label>{t("snippets.folderName")}<input ref={folderName} autoFocus value={name} maxLength={200} onChange={(event) => { setName(event.target.value); setDialogError(null); }} disabled={saving} /></label> : <p>{t("snippets.deleteMessage")}</p>}
      {dialogError && <p role="alert" className="error">{dialogError}</p>}<div className="dialog-actions"><button ref={dialogCancel} autoFocus={dialog.kind === "delete"} type="button" onClick={() => setDialog(null)} disabled={saving}>{t("files.cancel")}</button><button className={dialog.kind === "delete" ? "danger" : "primary"} type="submit" disabled={saving || (dialog.kind === "folder" && !name.trim())}>{t(saving ? "snippets.saving" : dialog.kind === "folder" ? "snippets.createFolder" : "snippets.delete")}</button></div>
    </form></div>}
  </>;
}
