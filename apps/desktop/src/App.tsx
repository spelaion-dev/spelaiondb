import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { events } from "./api";
import { ActivityBar } from "./components/ActivityBar";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { HistoryPanel } from "./components/HistoryPanel";
import { ProcessMonitor, ProcessSidebar } from "./components/ProcessMonitor";
import { SettingsDialog } from "./components/Settings";
import { SnippetsPanel } from "./components/SnippetsPanel";
import { Sidebar, refreshConnections } from "./components/Sidebar";
import { closeQueryTab } from "./connect";
import { usePreferences } from "./preferences";
import { useUpdate } from "./update";
import { SAVE_QUERY_EVENT, useApp, useDialogs } from "./store";

const SIDEBAR_DEFAULT = 290;
const SIDEBAR_MIN = 210;
const SIDEBAR_MAX = 700;
const EDITOR_MIN = 360;
const ACTIVITY_WIDTH = 48;

export default function App() {
  const { t } = useTranslation(["app", "shell", "settings"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
  const addTab = useApp((s) => s.addTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const historyOpen = useApp((s) => s.historyOpen);
  const snippetsOpen = useApp((s) => s.snippetsOpen);
  const settingsOpen = useApp((s) => s.settingsOpen);
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
    const onKey = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      const key = e.key.toLowerCase();
      const app = useApp.getState();
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
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      void unlisten.then((f) => f());
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
          {tabs.map((tab) => (
            <div
              key={tab.id}
              role="tab"
              aria-selected={tab.id === activeTab}
              className={`tab ${tab.id === activeTab ? "active" : ""}`}
              onClick={() => setActiveTab(tab.id)}
            >
              <span className={`dot dot-${tab.status}`} />
              <span>{tab.table ? tab.table.name : t("shell:queryTab", { number: tab.number })}</span>
              <button
                className="close"
                aria-label={t("closeTab", { name: t("shell:queryTab", { number: tab.number }) })}
                onClick={(e) => {
                  e.stopPropagation();
                  void closeQueryTab(tab.id);
                }}
              >
                ×
              </button>
            </div>
          ))}
          <button className="new-tab" onClick={() => addTab(null)} title={t("newQueryShortcut")}>
            +
          </button>
        </div>
        </div>
        <div className="workspace-content">
          <div className="editor-workspace" hidden={sidebarSection === "processes"}>
            {tabs.length === 0 ? (
              <div className="welcome">
                <h1>SpelaionDB</h1>
                <p>{t("welcome")}</p>
                <button className="primary" onClick={() => addTab(null)}>
                  {t("newQuery")}
                </button>
              </div>
            ) : (
              tabs.map((t) => (
                // All mounted (the editor keeps its state); only the active one is visible.
                <div key={t.id} className="tab-panel" hidden={t.id !== activeTab}>
                  <EditorTab tab={t} />
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
