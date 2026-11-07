import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ActivityBar } from "./components/ActivityBar";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { HistoryPanel } from "./components/HistoryPanel";
import { ProcessMonitor, ProcessSidebar } from "./components/ProcessMonitor";
import { SettingsDialog } from "./components/Settings";
import { SnippetsPanel } from "./components/SnippetsPanel";
import { FOCUS_EXPLORER_FILTER_EVENT, Sidebar, refreshConnections } from "./components/Sidebar";
import { OpenObjectDialog } from "./components/OpenObjectDialog";
import { TableDetailsView } from "./components/TableDetailsDialog";
import { EXPLORER_SESSION_CLOSED_EVENT, closeQueryTab, tabName } from "./connect";
import { call, commands, events } from "./api";
import { usePreferences } from "./preferences";
import { useUpdate } from "./update";
import { SAVE_QUERY_EVENT, useApp, useDialogs } from "./store";
import { topDialog, useModalFocus } from "./modalFocus";

const SIDEBAR_DEFAULT = 290;
const SIDEBAR_MIN = 210;
const SIDEBAR_MAX = 700;
const EDITOR_MIN = 360;
const ACTIVITY_WIDTH = 48;

/** Distinguishes SQL, table data and table metadata without adding labels to the tab bar. */
function TabIcon({ kind }: { kind: "query" | "table" | "details" }) {
  return (
    <svg className="tab-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {kind === "query" ? <><path d="m3 4 3 3-3 3M8 11h5" /></> :
        kind === "table" ? <><rect x="2.5" y="3" width="11" height="10" rx="1" /><path d="M2.5 6.5h11M6.2 6.5V13M9.8 6.5V13" /></> :
          <><rect x="2.5" y="3" width="11" height="10" rx="1" /><path d="M6 3v10M8 6h3.5M8 9.5h3.5" /></>}
    </svg>
  );
}

export default function App() {
  useModalFocus();
  const { t } = useTranslation(["app", "shell", "settings", "editor"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
  const addTab = useApp((s) => s.addTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const historyOpen = useApp((s) => s.historyOpen);
  const snippetsOpen = useApp((s) => s.snippetsOpen);
  const settingsOpen = useApp((s) => s.settingsOpen);
  const openObject = useApp((s) => s.openObject);
  const closeSettings = useApp((s) => s.closeSettings);
  const connectionDialog = useApp((s) => s.connectionDialog);
  const deletingConnection = useApp((s) => s.deletingConnection);
  // Save errors are shown next to the setting; only startup load errors need a banner.
  const preferenceError = usePreferences((s) => (s.error?.phase === "load" ? s.error : null));
  const [loadError, setLoadError] = useState<string | null>(null);
  const [sidebarWidth, setSidebarWidth] = useState(SIDEBAR_DEFAULT);
  const [sidebarSection, setSidebarSection] = useState<"connections" | "processes" | null>("connections");
  const [processConnection, setProcessConnection] = useState<string | null>(null);
  const connections = useApp((s) => s.connections);
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const sidebarOpen = sidebarSection !== null;
  const selectedProcessConnection = connections.find((connection) => connection.id === processConnection);
  const [windowWidth, setWindowWidth] = useState(window.innerWidth);
  const sidebarDrag = useRef<{ x: number; width: number } | null>(null);
  const maxSidebarWidth = Math.max(SIDEBAR_MIN, Math.min(SIDEBAR_MAX, windowWidth - EDITOR_MIN - ACTIVITY_WIDTH));
  const clampSidebar = (width: number) => Math.round(Math.max(SIDEBAR_MIN, Math.min(maxSidebarWidth, width)));
  const shownSidebarWidth = clampSidebar(sidebarWidth);

  useEffect(() => {
    const resize = () => setWindowWidth(window.innerWidth);
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, []);

  useEffect(() => {
    if (workspaceChanging) setProcessConnection(null);
  }, [workspaceChanging]);

  useEffect(() => {
    if (activeTab) document.getElementById(`workspace-${activeTab}`)?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [activeTab]);

  async function loadConnections() {
    try {
      await refreshConnections();
      setLoadError(null);
    } catch (e) {
      setLoadError(e instanceof Error ? e.message : String(e));
    }
  }

  useEffect(() => {
    void loadConnections();
    const unlisten = events.interactionRequest.listen((e) =>
      useDialogs.getState().pushInteraction({ id: e.payload.id, request: e.payload.request }),
    );
    // Nobody waits for that answer any more (a stopped connection test, ADR-032).
    const unlistenClosed = events.interactionClosed.listen((e) => useDialogs.getState().popInteraction(e.payload.id));
    // A tab whose session died, or was closed for being idle (ADR-029), shows it; nothing
    // reconnects or re-runs on its own (ADR-028). The explorer's own session collapses.
    const unlistenLost = events.sessionLost.listen((e) => {
      const { session, reason, idleSeconds, openTransaction } = e.payload;
      useApp.getState().updateTab(session, { status: "disconnected", lost: reason, idleClosed: idleSeconds ?? undefined, lostTransaction: openTransaction || undefined, transaction: undefined });
      if (session.startsWith("meta:")) {
        window.dispatchEvent(new CustomEvent(EXPLORER_SESSION_CLOSED_EVENT, { detail: { connectionId: session.slice("meta:".length), idleSeconds } }));
      }
    });
    // The core keeps each editor tab's transaction (ADR-031); the tab only shows it.
    const unlistenTransaction = events.transactionChanged.listen((e) => {
      useApp.getState().updateTab(e.payload.session, { transaction: e.payload.status });
    });
    // Quitting with transactions open: the core held the window; the user decides.
    const unlistenQuit = events.quitBlocked.listen((e) => {
      void useDialogs.getState().askOpenTransactions(e.payload.sessions.map(tabName), "quit").then((quit) => {
        if (quit) void call(commands.quitApp()).catch(() => undefined);
      });
    });
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const app = useApp.getState();
      const modal = topDialog();
      if (mod && modal && (key === "t" || key === "s" || key === "w" || e.key === "Tab" || e.key === "PageDown" || e.key === "PageUp" || e.key === "," || (e.shiftKey && (key === "d" || key === "f")))) {
        e.preventDefault();
        if (key === "w" && modal.classList.contains("settings-dialog")) app.closeSettings();
        return;
      }
      if (mod && key === "t") {
        e.preventDefault();
        if (e.shiftKey) app.reopenTab();
        else app.addTab(null);
      } else if (mod && key === "s") {
        e.preventDefault();
        const target = e.target instanceof Element ? e.target : null;
        if (app.activeTab && !app.settingsOpen && target?.closest(".editor-pane")) {
          window.dispatchEvent(new CustomEvent(SAVE_QUERY_EVENT, { detail: app.activeTab }));
        }
      } else if (mod && key === "w") {
        e.preventDefault();
        if (app.settingsOpen) app.closeSettings();
        else if (app.activeTab) void closeQueryTab(app.activeTab);
      } else if (mod && (e.key === "Tab" || e.key === "PageDown" || e.key === "PageUp")) {
        // Next or previous tab, wrapping around.
        const order = app.tabs.map((t) => t.id);
        if (order.length === 0) return;
        e.preventDefault();
        const back = e.key === "PageUp" || (e.key === "Tab" && e.shiftKey);
        const at = Math.max(0, order.indexOf(app.activeTab ?? ""));
        const next = order[(at + (back ? order.length - 1 : 1)) % order.length];
        if (next) app.setActiveTab(next);
      } else if (mod && e.key === ",") {
        e.preventDefault();
        app.openSettings();
      } else if (mod && e.shiftKey && key === "d") {
        // DBeaver's Open Database Object.
        e.preventDefault();
        app.setOpenObject({ connectionId: null });
      } else if (mod && e.shiftKey && key === "f") {
        // Focus the explorer's quick filter, showing the explorer if it is hidden.
        e.preventDefault();
        setSidebarSection("connections");
        if (app.explorerFilter === null) app.setExplorerFilter("");
        window.dispatchEvent(new Event(FOCUS_EXPLORER_FILTER_EVENT));
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      void unlisten.then((f) => f());
      void unlistenClosed.then((f) => f());
      void unlistenLost.then((f) => f());
      void unlistenTransaction.then((f) => f());
      void unlistenQuit.then((f) => f());
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <div className="app" style={{ gridTemplateColumns: `${ACTIVITY_WIDTH}px ${sidebarOpen ? shownSidebarWidth : 0}px 0px minmax(0, 1fr)` }}>
      <ActivityBar connectionsOpen={sidebarSection === "connections"} processesOpen={sidebarSection === "processes"} onToggleConnections={() => setSidebarSection((section) => section === "connections" ? null : "connections")} onToggleProcesses={() => setSidebarSection((section) => section === "processes" ? null : "processes")} />
      <Sidebar hidden={sidebarSection !== "connections"} />
      {sidebarSection === "processes" && <ProcessSidebar selected={processConnection} onSelect={setProcessConnection} />}
      <div
        className="sidebar-resizer"
        hidden={!sidebarOpen}
        role="separator"
        tabIndex={0}
        aria-label={t("resizeSidebar")}
        title={t("resizeSidebarHint")}
        aria-controls={sidebarSection === "processes" ? "processes-sidebar" : "connections-sidebar"}
        aria-orientation="vertical"
        aria-valuemin={SIDEBAR_MIN}
        aria-valuemax={maxSidebarWidth}
        aria-valuenow={shownSidebarWidth}
        onPointerDown={(e) => {
          if (e.button !== 0) return;
          e.preventDefault();
          sidebarDrag.current = { x: e.clientX, width: shownSidebarWidth };
          e.currentTarget.setPointerCapture(e.pointerId);
        }}
        onPointerMove={(e) => {
          if (!sidebarDrag.current) return;
          setSidebarWidth(clampSidebar(sidebarDrag.current.width + e.clientX - sidebarDrag.current.x));
        }}
        onPointerUp={(e) => {
          sidebarDrag.current = null;
          if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
        }}
        onPointerCancel={() => { sidebarDrag.current = null; }}
        onDoubleClick={() => setSidebarWidth(SIDEBAR_DEFAULT)}
        onKeyDown={(e) => {
          const step = e.shiftKey ? 50 : 10;
          if (e.key === "ArrowLeft") setSidebarWidth(clampSidebar(shownSidebarWidth - step));
          else if (e.key === "ArrowRight") setSidebarWidth(clampSidebar(shownSidebarWidth + step));
          else if (e.key === "Home") setSidebarWidth(SIDEBAR_MIN);
          else if (e.key === "End") setSidebarWidth(maxSidebarWidth);
          else return;
          e.preventDefault();
        }}
      />
      <main className="main">
        {loadError && <div className="banner" role="alert">{loadError}<button onClick={() => void loadConnections()}>{t("retryConnections")}</button></div>}
        {preferenceError && <div className="banner" role="alert">{t("preferenceLoadError")} {preferenceError.detail}</div>}
        <UpdateNotice />
        <div className="tab-strip" hidden={sidebarSection === "processes"}>
        <div className="tabbar" role="tablist">
          {tabs.map((tab, index) => (
            <div
              key={tab.id}
              id={`workspace-${tab.id}`}
              role="tab"
              aria-selected={tab.id === activeTab}
              className={`tab ${tab.id === activeTab ? "active" : ""}`}
              tabIndex={tab.id === activeTab ? 0 : -1}
              onClick={() => setActiveTab(tab.id)}
              onKeyDown={(e) => {
                if (e.target !== e.currentTarget || tabs.length === 0) return;
                let next: number;
                if (e.key === "ArrowRight") next = (index + 1) % tabs.length;
                else if (e.key === "ArrowLeft") next = (index + tabs.length - 1) % tabs.length;
                else if (e.key === "Home") next = 0;
                else if (e.key === "End") next = tabs.length - 1;
                else return;
                e.preventDefault();
                const target = tabs[next];
                if (target) {
                  setActiveTab(target.id);
                  document.getElementById(`workspace-${target.id}`)?.focus();
                }
              }}
            >
              <TabIcon kind={tab.tableDetails ? "details" : tab.table ? "table" : "query"} />
              <span className={`dot dot-${tab.status}`} role="img" aria-label={t(`editor:status.${tab.status}`)} title={t(`editor:status.${tab.status}`)} />
              {(tab.transaction?.state === "open" || tab.transaction?.state === "failed") && (
                <span className={`tab-tx${tab.transaction.state === "failed" ? " tab-tx-failed" : ""}`} title={t("editor:transaction.tabBadge")} aria-label={t("editor:transaction.tabBadge")}>TX</span>
              )}
              <span className="tab-label" title={tab.tableDetails ? `${tab.tableDetails.name} · ${t(`shell:folders.${tab.tableDetails.kind}`)}` : tab.table ? tab.table.name : t("shell:queryTab", { number: tab.number })}>{tab.tableDetails ? `${tab.tableDetails.name} · ${t(`shell:folders.${tab.tableDetails.kind}`)}` : tab.table ? tab.table.name : t("shell:queryTab", { number: tab.number })}</span>
              <button
                className="close"
                aria-label={t("closeTab", { name: tab.tableDetails ? `${tab.tableDetails.name} · ${t(`shell:folders.${tab.tableDetails.kind}`)}` : tab.table ? tab.table.name : t("shell:queryTab", { number: tab.number }) })}
                onClick={(e) => {
                  e.stopPropagation();
                  void closeQueryTab(tab.id);
                }}
              >
                ×
              </button>
            </div>
          ))}
        </div>
        <button className="new-tab" type="button" onClick={() => addTab(null)} title={t("newQueryShortcut")} aria-label={t("newQueryShortcut")}>+</button>
        </div>
        <div className="workspace-content">
          <div className="editor-workspace" hidden={sidebarSection === "processes"}>
            {tabs.length === 0 ? (
              <div className="welcome">
                <p className="welcome-eyebrow">{t("welcomeEyebrow")}</p>
                <h1>SpelaionDB</h1>
                <p>{t("welcome")}</p>
                <div className="welcome-actions">
                  <button className="primary" onClick={() => addTab(null)}>
                    {t("newQuery")}
                  </button>
                  <button onClick={() => useApp.getState().setConnectionDialog({ kind: "add" })}>
                    {t("connections:add")}
                  </button>
                </div>
                <p className="welcome-shortcut">{t("newQueryShortcut")}</p>
              </div>
            ) : (
              tabs.map((t) => (
                // All mounted (the editor keeps its state); only the active one is visible.
                <div key={t.id} className="tab-panel" hidden={t.id !== activeTab}>
                  {t.tableDetails ? <TableDetailsView tab={t} /> : <EditorTab tab={t} />}
                </div>
              ))
            )}
          </div>
          {sidebarSection === "processes" && (selectedProcessConnection
            ? <ProcessMonitor key={selectedProcessConnection.id} connection={selectedProcessConnection} onOpenEditor={() => setSidebarSection("connections")} />
            : <div className="process-welcome"><h2>{t("shell:processMonitor.title")}</h2><p>{t("shell:processMonitor.choose")}</p></div>)}
          {historyOpen && <HistoryPanel />}
          {snippetsOpen && <SnippetsPanel />}
        </div>
      </main>
      {connectionDialog && (
        <AddConnection key={connectionDialog.kind === "edit" ? connectionDialog.id : "add"} dialog={connectionDialog} />
      )}
      {deletingConnection && <DeleteConnection key={deletingConnection.id} connection={deletingConnection} />}
      {settingsOpen && <SettingsDialog onClose={closeSettings} />}
      {openObject && (
        <OpenObjectDialog initialConnection={openObject.connectionId} onClose={() => useApp.getState().setOpenObject(null)} />
      )}
      <Dialogs />
    </div>
  );
}

/** A new version is out: installing waits for the user's click (ADR-021). */
function UpdateNotice() {
  const { t } = useTranslation("app");
  const status = useUpdate((s) => s.status);
  const dismissed = useUpdate((s) => s.dismissed);
  const installing = useUpdate((s) => s.installing);
  const error = useUpdate((s) => s.error);
  const install = useUpdate((s) => s.install);
  const dismiss = useUpdate((s) => s.dismiss);
  if (status?.kind !== "available" || dismissed) return null;
  return (
    <div className="banner update-notice" role="status">
      <span>{t("update.available", { version: status.version, current: status.currentVersion })}</span>
      {error && <span className="error">{error}</span>}
      <div className="banner-actions">
        <button className="primary" disabled={installing} onClick={() => void install()}>
          {installing ? t("update.installing") : t("update.install")}
        </button>
        <button disabled={installing} onClick={dismiss}>
          {t("update.later")}
        </button>
      </div>
    </div>
  );
}
