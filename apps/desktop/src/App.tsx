import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ExportDialog } from "./components/ExportDialog";
import { BackupDialog } from "./components/BackupDialog";
import { ImportDialog } from "./components/ImportDialog";
import { RunFileDialog } from "./components/RunFileDialog";
import { ActivityBar } from "./components/ActivityBar";
import { McpPage } from "./components/McpPage";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { MoveConnection } from "./components/MoveConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { PlanFileTab } from "./components/PlanViewer";
import { openSavedPlan } from "./planFiles";
import { TabBar } from "./components/TabBar";
import { HistoryPanel } from "./components/HistoryPanel";
import { ProcessMonitor, ProcessSidebar } from "./components/ProcessMonitor";
import { SearchPanel } from "./components/SearchPanel";
import { SettingsDialog } from "./components/Settings";
import { watchPlugins } from "./plugins";
import { ReleaseNotesDialog } from "./components/ReleaseNotesDialog";
import { SnippetsPanel } from "./components/SnippetsPanel";
import { Sidebar, refreshConnections } from "./components/Sidebar";
import { CommandPalette } from "./components/CommandPalette";
import { OpenObjectDialog } from "./components/OpenObjectDialog";
import { TableDetailsView } from "./components/TableDetailsDialog";
import { ErDiagramView } from "./components/ErDiagram";
import { EXPLORER_SESSION_CLOSED_EVENT, answerQuitRequest } from "./connect";
import { commandSpec, globalCommandFor, withShortcut, type CommandId } from "./commandRegistry";
import { activeContext, isCommandId, runCommand } from "./commands";
import { commands, events } from "./api";
import { usePreferences } from "./preferences";
import { isInstalling, manualUpdateKey, useUpdate } from "./update";
import { UpdateInstallButton, UpdateProblem } from "./components/UpdateInstall";
import { useApp, useDialogs } from "./store";
import { sessionLost } from "./tabState";
import { topDialog, useModalFocus } from "./modalFocus";
import { restoreTabs } from "./tabPersistence";
import { ScreenBoundary } from "./components/ScreenBoundary";
import { openLaunchRequest } from "./launch";

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
  const movingConnection = useApp((s) => s.movingConnection);
  // Save errors are shown next to the setting; only startup load errors need a banner.
  const preferenceError = usePreferences((s) => (s.error?.phase === "load" ? s.error : null));
  const launchNotice = useApp((s) => s.launchNotice);
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
    void loadConnections().finally(async () => {
      await restoreTabs();
      const path = await commands.startupPlanPath();
      if (path) await openSavedPlan(path);
      // What this app's command line asked to open besides its folder (SDB-397).
      const launch = await commands.takeLaunchRequest();
      if (launch) await openLaunchRequest(launch);
    });
    // Which engines need a plugin that is not installed (ADR-055), for the picker and failures.
    watchPlugins();
    const unlisten = events.interactionRequest.listen((e) =>
      useDialogs.getState().pushInteraction({ id: e.payload.id, request: e.payload.request }),
    );
    // Nobody waits for that answer any more (a stopped connection test, ADR-032).
    const unlistenClosed = events.interactionClosed.listen((e) => useDialogs.getState().popInteraction(e.payload.id));
    // A tab whose session died, or was closed for being idle (ADR-029), shows it; nothing
    // reconnects or re-runs on its own (ADR-028). The explorer's own session collapses.
    const unlistenLost = events.sessionLost.listen((e) => {
      const { session, reason, idleSeconds, openTransaction } = e.payload;
      useApp.getState().setConnection(session, sessionLost(reason, idleSeconds, openTransaction));
      if (session.startsWith("meta:")) {
        window.dispatchEvent(new CustomEvent(EXPLORER_SESSION_CLOSED_EVENT, { detail: { connectionId: session.slice("meta:".length), idleSeconds } }));
      }
    });
    // The core keeps each editor tab's transaction (ADR-031); the tab only shows it.
    const unlistenTransaction = events.transactionChanged.listen((e) => {
      useApp.getState().setTransaction(e.payload.session, e.payload.status);
    });
    // The core keeps each editor tab's run state (SDB-370); the tab only shows it.
    const unlistenRun = events.runChanged.listen((e) => {
      useApp.getState().updateTab(e.payload.session, { runState: e.payload.state });
    });
    // Closing the window with sessions open: the core held it; the UI quits or asks (SDB-140).
    const unlistenQuit = events.quitRequested.listen(() => void answerQuitRequest());
    // A second launch (or Finder) asked this window to open something (SDB-397).
    const unlistenLaunch = events.launchRequested.listen((e) => void openLaunchRequest(e.payload.request));
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
      void unlistenRun.then((f) => f());
      void unlistenQuit.then((f) => f());
      void unlistenLaunch.then((f) => f());
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  return (
    <div className="app" style={{ gridTemplateColumns: `${ACTIVITY_WIDTH}px ${sidebarOpen ? shownSidebarWidth : 0}px 0px minmax(0, 1fr)` }}>
      <ActivityBar />
      <ScreenBoundary screen="explorer" hidden={sidebarSection !== "connections"} className="sidebar">
        <Sidebar hidden={sidebarSection !== "connections"} />
      </ScreenBoundary>
      {sidebarSection === "processes" && <ProcessSidebar selected={processConnection} onSelect={setProcessConnection} />}
      {/* Always mounted: a data search goes on while another panel is shown. */}
      <SearchPanel hidden={sidebarSection !== "search"} />
      <div
        className="sidebar-resizer"
        hidden={!sidebarOpen}
        role="separator"
        tabIndex={0}
        aria-label={t("resizeSidebar")}
        title={t("resizeSidebarHint")}
        aria-controls={sidebarSection === "processes" ? "processes-sidebar" : sidebarSection === "search" ? "search-sidebar" : "connections-sidebar"}
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
        {launchNotice && (
          <div className="banner launch-notice" role="alert">
            <span>{launchNotice}</span>
            <button onClick={() => useApp.getState().setLaunchNotice(null)}>{t("shell:launch.dismiss")}</button>
          </div>
        )}
        <UpdateNotice />
        <ReleaseNotesDialog />
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
                  <ScreenBoundary screen={t.plan ? "plan" : t.tableDetails ? "table details" : t.diagram ? "er diagram" : t.table ? "table data" : "editor"}>
                    {t.plan ? <PlanFileTab tab={t} /> : t.tableDetails ? <TableDetailsView tab={t} /> : t.diagram ? <ErDiagramView tab={t} /> : <EditorTab tab={t} />}
                  </ScreenBoundary>
                </div>
              ))
            )}
          </div>
          {sidebarSection === "processes" && (selectedProcessConnection
            ? <ScreenBoundary screen="process monitor"><ProcessMonitor key={selectedProcessConnection.id} connection={selectedProcessConnection} onOpenEditor={() => setSidebarSection("connections")} /></ScreenBoundary>
            : <div className="process-welcome"><h2>{t("shell:processMonitor.title")}</h2><p>{t("shell:processMonitor.choose")}</p></div>)}
          {sidebarSection === "mcp" && <ScreenBoundary screen="mcp"><McpPage /></ScreenBoundary>}
          {historyOpen && <HistoryPanel />}
          {snippetsOpen && <SnippetsPanel />}
        </div>
      </main>
      {connectionDialog && (
        <AddConnection key={connectionDialog.kind === "edit" ? connectionDialog.id : connectionDialog.openImportMenu ? "import" : "add"} dialog={connectionDialog} />
      )}
      {deletingConnection && <DeleteConnection key={deletingConnection.id} connection={deletingConnection} />}
      {movingConnection && <MoveConnection key={movingConnection.id} connection={movingConnection} />}
      {settingsOpen && <SettingsDialog onClose={closeSettings} />}
      {openObject && (
        <OpenObjectDialog initialConnection={openObject.connectionId} onClose={() => useApp.getState().setOpenObject(null)} />
      )}
      {paletteOpen && <CommandPalette onClose={() => useApp.getState().setPaletteOpen(false)} />}
      <ExportDialog />
      <ImportDialog />
      <BackupDialog />
      <RunFileDialog />
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
  const installing = isInstalling(status);
  const dismiss = useUpdate((s) => s.dismiss);
  const showNotes = useUpdate((s) => s.showNotes);
  const openReleasesPage = useUpdate((s) => s.openReleasesPage);
  if (status?.kind !== "available" || dismissed) return null;
  // Only a copy that can replace itself offers to install (the core refuses the others too).
  const manual = manualUpdateKey(status.method);
  return (
    <div className="banner update-notice" role="status">
      <span>
        {t("update.available", { version: status.version, current: status.currentVersion })}
        {manual && ` ${t(`update.${manual}`)}`}
      </span>
      <UpdateProblem namespace="app" group="update" className="error" />
      <div className="banner-actions">
        <button disabled={installing} onClick={showNotes}>{t("update.notesAction")}</button>
        {manual ? (
          <button className="primary" onClick={() => void openReleasesPage()}>{t("update.openReleases")}</button>
        ) : (
          <UpdateInstallButton namespace="app" group="update" />
        )}
        <button disabled={installing} onClick={dismiss}>
          {t("update.later")}
        </button>
      </div>
    </div>
  );
}
