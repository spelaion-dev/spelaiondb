import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ExportDialog } from "./components/ExportDialog";
import { ActivityBar } from "./components/ActivityBar";
import { McpPage } from "./components/McpPage";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { TabBar } from "./components/TabBar";
import { HistoryPanel } from "./components/HistoryPanel";
import { ProcessMonitor, ProcessSidebar } from "./components/ProcessMonitor";
import { SettingsDialog } from "./components/Settings";
import { SnippetsPanel } from "./components/SnippetsPanel";
import { Sidebar, refreshConnections } from "./components/Sidebar";
import { CommandPalette } from "./components/CommandPalette";
import { OpenObjectDialog } from "./components/OpenObjectDialog";
import { TableDetailsView } from "./components/TableDetailsDialog";
import { EXPLORER_SESSION_CLOSED_EVENT, tabName } from "./connect";
import { commandSpec, globalCommandFor, withShortcut, type CommandId } from "./commandRegistry";
import { activeContext, isCommandId, runCommand } from "./commands";
import { call, commands, events } from "./api";
import { usePreferences } from "./preferences";
import { useUpdate } from "./update";
import { useApp, useDialogs } from "./store";
import { topDialog, useModalFocus } from "./modalFocus";
import { restoreTabs } from "./tabPersistence";

const SIDEBAR_DEFAULT = 290;
const SIDEBAR_MIN = 210;
const SIDEBAR_MAX = 700;
const EDITOR_MIN = 360;
const ACTIVITY_WIDTH = 48;

export default function App() {
  useModalFocus();
  const { t } = useTranslation(["app", "shell", "settings", "editor"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
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
  const sidebarSection = useApp((s) => s.sidebarSection);
  const setSidebarSection = useApp((s) => s.setSidebarSection);
  const paletteOpen = useApp((s) => s.paletteOpen);
  const [processConnection, setProcessConnection] = useState<string | null>(null);
  const connections = useApp((s) => s.connections);
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  // The MCP server page fills the window: it has no sidebar.
  const sidebarOpen = sidebarSection !== null && sidebarSection !== "mcp";
  // Opening or switching a query tab (from history, a shortcut…) leaves the MCP page.
  useEffect(() => {
    setSidebarSection((section) => (section === "mcp" ? "connections" : section));
  }, [activeTab, tabs.length]);
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
    // The open folder's tabs come back disconnected, with their text (ADR-036).
    void loadConnections().finally(() => void restoreTabs());
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
    // Global shortcuts: the registry's (commandRegistry.ts), the same commands as the menus and
    // the command palette. Ctrl+Tab is the tab bar's switcher (TabBar.tsx).
    const onKey = (e: KeyboardEvent) => {
      const command = globalCommandFor(e);
      if (!command || !isCommandId(command.id)) return;
      e.preventDefault();
      // A modal dialog keeps the app's shortcuts from acting behind it; Ctrl+W closes Settings.
      const modal = topDialog();
      if (modal) {
        if (command.id === "tab.close" && modal.classList.contains("settings-dialog")) useApp.getState().closeSettings();
        return;
      }
      runCommand(command.id, activeContext("key", e.target));
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
      <ActivityBar />
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
        <TabBar hidden={sidebarSection === "processes" || sidebarSection === "mcp"} />
        <div className="workspace-content">
          <div className="editor-workspace" hidden={sidebarSection === "processes" || sidebarSection === "mcp"}>
            {tabs.length === 0 ? (
              <div className="welcome">
                <p className="welcome-eyebrow">{t("welcomeEyebrow")}</p>
                <h1>SpelaionDB</h1>
                <p>{t("welcome")}</p>
                <div className="welcome-actions">
                  <WelcomeButton command="tab.new" primary />
                  <WelcomeButton command="connection.add" />
                  <WelcomeButton command="file.open" />
                </div>
                <p className="welcome-shortcut">{t("newQueryShortcut")}</p>
                <p className="welcome-shortcut">{t("commands.paletteHint", { shortcut: commandSpec("palette.show").shortcut })}</p>
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
          {sidebarSection === "mcp" && <McpPage />}
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
      {paletteOpen && <CommandPalette onClose={() => useApp.getState().setPaletteOpen(false)} />}
      <ExportDialog />
      <Dialogs />
    </div>
  );
}

/** A welcome screen action: a command of the registry, named as in the palette. */
function WelcomeButton({ command, primary = false }: { command: CommandId; primary?: boolean }) {
  const { t } = useTranslation();
  const spec = commandSpec(command);
  return (
    <button className={primary ? "primary" : undefined} data-command={command} title={withShortcut(t(spec.label), spec)} onClick={() => runCommand(command, activeContext("menu"))}>
      {t(spec.label)}
    </button>
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
