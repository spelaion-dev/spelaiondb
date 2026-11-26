import { memo, useEffect, useRef, useState, type CSSProperties, type RefObject } from "react";
import { useTranslation } from "react-i18next";
import { useShallow } from "zustand/react/shallow";
import { ExportDialog } from "./components/ExportDialog";
import { BackupDialog } from "./components/BackupDialog";
import { CopyDialog } from "./components/CopyDialog";
import { SchemaCompareDialog } from "./components/SchemaCompareDialog";
import { ImportDialog } from "./components/ImportDialog";
import { RunFileDialog } from "./components/RunFileDialog";
import { ActivityBar } from "./components/ActivityBar";
import { McpPage } from "./components/McpPage";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { MoveConnection } from "./components/MoveConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { NotebookTab } from "./components/NotebookTab";
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
import { ExplorerDialogHost, Sidebar, refreshConnections } from "./components/Sidebar";
import { CommandPalette } from "./components/CommandPalette";
import { OpenObjectDialog } from "./components/OpenObjectDialog";
import { TableDetailsView } from "./components/TableDetailsDialog";
import { ErDiagramView } from "./components/ErDiagram";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { answerQuitRequest, answerWindowCloseRequest, explorerSessionClosed, newQueryOnConnection, tabLabel } from "./connect";
import { connectionTag, keyOf, listedConnection, type ConnectionKey } from "./connectionKey";
import { explorerConnectionOf, windowConnection, windowContext } from "./windowContext";
import { DEFAULT_SHARE, MAX_SHARE, MIN_SHARE, paneOf, resized, type Pane, type SplitLayout } from "./splitView";
import { commandSpec, globalCommandFor, type CommandId } from "./commandRegistry";
import { commandShortcut, commandTitle } from "./shortcuts";
import { activeContext, isCommandId, runCommand } from "./commands";
import { commands, events } from "./api";
import { usePreferences } from "./preferences";
import { isInstalling, manualUpdateKey, useUpdate } from "./update";
import { UpdateInstallButton, UpdateProblem } from "./components/UpdateInstall";
import { isSidePanel, useApp, useDialogs } from "./store";
import { sessionLost } from "./tabState";
import { topDialog, useModalFocus } from "./modalFocus";
import { restoreTabs } from "./tabPersistence";
import { ScreenBoundary } from "./components/ScreenBoundary";
import { openLaunchRequest } from "./launch";

const EDITOR_MIN = 360;
const ACTIVITY_WIDTH = 48;

export default function App() {
  useModalFocus();
  const { t } = useTranslation(["app", "shell", "settings", "editor"]);
  // Only the tabs' ids: a change inside one tab (its run, its connection) re-renders that tab's
  // panel, never the whole window (SDB-110).
  const tabIds = useApp(useShallow((s) => s.tabs.map((tab) => tab.id)));
  const activeTab = useApp((s) => s.activeTab);
  const layout = useApp((s) => s.layout);
  const workspaceRef = useRef<HTMLDivElement>(null);
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
  // The side panel's widths come from the core, with the width and the hidden state it was left
  // with (SDB-451): a personal preference saved as they change.
  const sidePanel = usePreferences((s) => s.sidePanel);
  const preferencesLoading = usePreferences((s) => s.loading);
  const { minWidth: SIDEBAR_MIN, maxWidth: SIDEBAR_MAX, defaultWidth: SIDEBAR_DEFAULT } = sidePanel;
  const [sidebarWidth, setSidebarWidth] = useState(sidePanel.width);
  const sidebarSection = useApp((s) => s.sidebarSection);
  const setSidebarSection = useApp((s) => s.setSidebarSection);
  /** The column was hidden by the user: `null`, not the MCP page (which has no column). */
  const sideHidden = sidebarSection === null;
  const restored = useRef(false);
  // Once the preferences are read: the width, and hidden if it was left hidden.
  useEffect(() => {
    if (preferencesLoading || restored.current) return;
    restored.current = true;
    const { sidePanel: left } = usePreferences.getState();
    setSidebarWidth(left.width);
    if (left.hidden) setSidebarSection((section) => (isSidePanel(section) ? null : section));
  }, [preferencesLoading]);
  // Hiding and showing are remembered; the MCP page says nothing about the column. Read from the
  // store, not this render: the restore above may have just changed it.
  useEffect(() => {
    const section = useApp.getState().sidebarSection;
    if (!restored.current || section === "mcp") return;
    const { sidePanel: saved, setSidePanel } = usePreferences.getState();
    if (saved.hidden !== (section === null)) void setSidePanel(section === null, saved.width);
  }, [sideHidden, sidebarSection]);
  /** Saves the width the separator left, at the end of a drag or a key press. */
  const saveWidth = (width: number) => {
    const { sidePanel: saved, setSidePanel } = usePreferences.getState();
    if (saved.width !== width) void setSidePanel(saved.hidden, width);
  };
  const paletteOpen = useApp((s) => s.paletteOpen);
  const [processConnection, setProcessConnection] = useState<ConnectionKey | null>(null);
  const connections = useApp((s) => s.connections);
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  // The MCP server page fills the window: it has no sidebar.
  const sidebarOpen = sidebarSection !== null && sidebarSection !== "mcp";
  // Opening or switching a query tab (from history, a shortcut…) leaves the MCP page.
  useEffect(() => {
    setSidebarSection((section) => (section === "mcp" ? "connections" : section));
  }, [activeTab, tabIds.length]);
  const selectedProcessConnection = processConnection === null ? undefined : listedConnection(connections, processConnection);
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
    const context = windowContext();
    const ownConnection = windowConnection();
    void loadConnections().finally(async () => {
      // A connection's own window (SDB-30) starts with a new query on it, connected as a
      // double click in the explorer connects one; nothing of the folder's tabs comes with it,
      // and nothing it opens is kept for the next start.
      if (ownConnection !== null) {
        if (listedConnection(useApp.getState().connections, ownConnection)) newQueryOnConnection(ownConnection);
        return;
      }
      // The open folder's tabs come back disconnected, with their text (ADR-036).
      await restoreTabs();
      const path = await commands.startupPlanPath();
      if (path) await openSavedPlan(path);
      // What this app's command line asked to open besides its folder (SDB-397).
      const launch = await commands.takeLaunchRequest();
      if (launch) await openLaunchRequest(launch);
    });
    // Which engines need a plugin that is not installed (ADR-055), for the picker and failures.
    watchPlugins();
    // What the core sends to this window only (SDB-30) is heard on this window: a plain
    // `listen` would also hear what is meant for another one.
    const here = getCurrentWebviewWindow();
    const unlisten = events.interactionRequest(here).listen((e) =>
      useDialogs.getState().pushInteraction({ id: e.payload.id, request: e.payload.request }),
    );
    // Nobody waits for that answer any more (a stopped connection test, ADR-032).
    const unlistenClosed = events.interactionClosed(here).listen((e) => useDialogs.getState().popInteraction(e.payload.id));
    // A tab whose session died, or was closed for being idle (ADR-029), shows it; nothing
    // reconnects or re-runs on its own (ADR-028). The explorer's own session collapses. Session
    // keys never repeat between windows, so another window's sessions change nothing here.
    const unlistenLost = events.sessionLost.listen((e) => {
      const { session, reason, idleSeconds, openTransaction } = e.payload;
      useApp.getState().setConnection(session, sessionLost(reason, idleSeconds, openTransaction));
      const explorerConnection = explorerConnectionOf(session);
      if (explorerConnection !== null) {
        explorerSessionClosed(explorerConnection, idleSeconds);
      }
    });
    // The core keeps each editor tab's transaction (ADR-031); the tab only shows it.
    const unlistenTransaction = events.transactionChanged.listen((e) => {
      useApp.getState().setTransaction(e.payload.session, e.payload.status);
    });
    // The core rolled back a tab's idle transaction (ADR-081): the tab says so once.
    const unlistenIdleRollback = events.idleTransactionRolledBack.listen((e) => {
      useApp.getState().updateTab(e.payload.session, { idleRolledBack: e.payload });
    });
    // The core keeps each editor tab's run state (SDB-370); the tab only shows it.
    const unlistenRun = events.runChanged.listen((e) => {
      useApp.getState().updateTab(e.payload.session, { runState: e.payload.state });
    });
    // Closing the window with sessions open: the core held it; the UI quits or asks (SDB-140).
    // Only the main window quits; a connection window closes itself (SDB-30).
    const unlistenQuit = context.kind === "main"
      ? events.quitRequested(here).listen(() => void answerQuitRequest())
      : events.windowCloseRequested(here).listen(() => void answerWindowCloseRequest());
    // A second launch (or Finder) asked this window to open something (SDB-397).
    const unlistenLaunch = events.launchRequested(here).listen((e) => void openLaunchRequest(e.payload.request));
    // A tab's results window closed (SDB-30): the tab shows its results again.
    const unlistenResults = events.resultsWindowClosed(here).listen((e) => {
      useApp.getState().updateTab(e.payload.tab, { resultsWindow: undefined });
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
      void unlistenIdleRollback.then((f) => f());
      void unlistenRun.then((f) => f());
      void unlistenQuit.then((f) => f());
      void unlistenLaunch.then((f) => f());
      void unlistenResults.then((f) => f());
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
          if (sidebarDrag.current) saveWidth(shownSidebarWidth);
          sidebarDrag.current = null;
          if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
        }}
        onPointerCancel={() => { sidebarDrag.current = null; }}
        onDoubleClick={() => { setSidebarWidth(SIDEBAR_DEFAULT); saveWidth(clampSidebar(SIDEBAR_DEFAULT)); }}
        onKeyDown={(e) => {
          const step = e.shiftKey ? 50 : 10;
          let next: number;
          if (e.key === "ArrowLeft") next = clampSidebar(shownSidebarWidth - step);
          else if (e.key === "ArrowRight") next = clampSidebar(shownSidebarWidth + step);
          else if (e.key === "Home") next = SIDEBAR_MIN;
          else if (e.key === "End") next = maxSidebarWidth;
          else return;
          e.preventDefault();
          setSidebarWidth(next);
          saveWidth(next);
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
          <div
            ref={workspaceRef}
            className={`editor-workspace${layout.kind === "split" ? ` split-${layout.direction}` : ""}`}
            style={splitTemplate(layout)}
            hidden={sidebarSection === "processes" || sidebarSection === "mcp"}
          >
            {layout.kind === "split" && <SplitResizer layout={layout} workspace={workspaceRef} />}
            {tabIds.length === 0 ? (
              <div className="welcome">
                <p className="welcome-eyebrow">{t("welcomeEyebrow")}</p>
                <h1>SpelaionDB</h1>
                <p>{t("welcome")}</p>
                <div className="welcome-actions">
                  <WelcomeButton command="tab.new" primary />
                  <WelcomeButton command="connection.add" />
                  <WelcomeButton command="file.open" />
                </div>
                <p className="welcome-shortcut">{t("newQueryShortcut", { shortcut: commandShortcut("tab.new") })}</p>
                <p className="welcome-shortcut">{t("commands.paletteHint", { shortcut: commandShortcut("palette.show") })}</p>
              </div>
            ) : (
              // All mounted (the editor keeps its state); only the active one is visible, or the
              // two panes of a split (SDB-30).
              tabIds.map((id) => <TabPanel key={id} id={id} active={id === activeTab} pane={paneOf(layout, id)} />)
            )}
          </div>
          {sidebarSection === "processes" && (selectedProcessConnection
            ? <ScreenBoundary screen="process monitor"><ProcessMonitor key={connectionTag(keyOf(selectedProcessConnection))} connection={selectedProcessConnection} onOpenEditor={() => setSidebarSection("connections")} /></ScreenBoundary>
            : <div className="process-welcome"><h2>{t("shell:processMonitor.title")}</h2><p>{t("shell:processMonitor.choose")}</p></div>)}
          {sidebarSection === "mcp" && <ScreenBoundary screen="mcp"><McpPage /></ScreenBoundary>}
          {historyOpen && <HistoryPanel />}
          {snippetsOpen && <SnippetsPanel />}
        </div>
      </main>
      {connectionDialog && (
        <AddConnection key={connectionDialog.kind === "edit" ? connectionTag(connectionDialog.connection) : connectionDialog.imports ? "import" : "add"} dialog={connectionDialog} />
      )}
      {deletingConnection && <DeleteConnection key={deletingConnection.id} connection={deletingConnection} />}
      {movingConnection && <MoveConnection key={movingConnection.id} connection={movingConnection} />}
      {settingsOpen && <SettingsDialog onClose={closeSettings} />}
      {openObject && (
        <OpenObjectDialog initialConnection={openObject.connection} onClose={() => useApp.getState().setOpenObject(null)} />
      )}
      {paletteOpen && <CommandPalette onClose={() => useApp.getState().setPaletteOpen(false)} />}
      <ExplorerDialogHost />
      <ExportDialog />
      <ImportDialog />
      <CopyDialog />
      <BackupDialog />
      <SchemaCompareDialog />
      <RunFileDialog />
      <Dialogs />
    </div>
  );
}

/** The grid tracks of a split: the first pane's share, the separator, the rest. */
function splitTemplate(layout: SplitLayout): CSSProperties | undefined {
  switch (layout.kind) {
    case "single":
      return undefined;
    case "split": {
      const tracks = `minmax(0, ${layout.share}fr) auto minmax(0, ${1 - layout.share}fr)`;
      return layout.direction === "right" ? { gridTemplateColumns: tracks } : { gridTemplateRows: tracks };
    }
    default:
      return unknownSplit(layout);
  }
}

function unknownSplit(layout: never): never {
  throw new Error(`unknown split layout: ${JSON.stringify(layout)}`);
}

/** Between the two panes of a split: dragged, or moved with the arrows (Shift: bigger steps),
 *  Home / End to its limits; a double click makes the panes equal. */
function SplitResizer({ layout, workspace }: { layout: Extract<SplitLayout, { kind: "split" }>; workspace: RefObject<HTMLDivElement | null> }) {
  const { t } = useTranslation("app");
  const across = layout.direction === "right";
  const setShare = (share: number) => {
    const app = useApp.getState();
    app.setLayout(resized(app.layout, share));
  };
  const shareAt = (e: { clientX: number; clientY: number }) => {
    const rect = workspace.current?.getBoundingClientRect();
    if (!rect) return null;
    return across ? (e.clientX - rect.left) / rect.width : (e.clientY - rect.top) / rect.height;
  };
  return (
    <div
      className="split-resizer"
      role="separator"
      tabIndex={0}
      aria-orientation={across ? "vertical" : "horizontal"}
      aria-label={t("split.resize")}
      title={t("split.resizeHint")}
      aria-valuemin={Math.round(MIN_SHARE * 100)}
      aria-valuemax={Math.round(MAX_SHARE * 100)}
      aria-valuenow={Math.round(layout.share * 100)}
      onPointerDown={(e) => {
        if (e.button !== 0) return;
        e.preventDefault();
        e.currentTarget.setPointerCapture(e.pointerId);
      }}
      onPointerMove={(e) => {
        if (!e.currentTarget.hasPointerCapture(e.pointerId)) return;
        const share = shareAt(e);
        if (share !== null) setShare(share);
      }}
      onPointerUp={(e) => {
        if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
      }}
      onDoubleClick={() => setShare(DEFAULT_SHARE)}
      onKeyDown={(e) => {
        const step = e.shiftKey ? 0.1 : 0.02;
        const back = across ? "ArrowLeft" : "ArrowUp";
        const forward = across ? "ArrowRight" : "ArrowDown";
        if (e.key === back) setShare(layout.share - step);
        else if (e.key === forward) setShare(layout.share + step);
        else if (e.key === "Home") setShare(MIN_SHARE);
        else if (e.key === "End") setShare(MAX_SHARE);
        else return;
        e.preventDefault();
      }}
    />
  );
}

/** One tab's panel. It reads its own tab, and is memoized on its id, visibility and pane, so a
 *  change to one tab (a run's progress, its connection) re-renders only that tab (SDB-110). In a
 *  split, a click or the focus inside the other pane makes its tab the active one. */
const TabPanel = memo(function TabPanel({ id, active, pane }: { id: string; active: boolean; pane: Pane | null }) {
  const { t: translate } = useTranslation("app");
  const t = useApp((s) => s.tabs.find((tab) => tab.id === id));
  if (!t) return null;
  const takeFocus = pane !== null && !active ? () => useApp.getState().setActiveTab(id) : undefined;
  return (
    <div
      id={`pane-${id}`}
      className={`tab-panel${pane !== null && active ? " focused-pane" : ""}`}
      data-pane={pane ?? undefined}
      role={pane !== null ? "region" : undefined}
      aria-label={pane !== null ? translate("split.pane", { name: tabLabel(t) }) : undefined}
      tabIndex={pane !== null ? -1 : undefined}
      hidden={!active && pane === null}
      onPointerDownCapture={takeFocus}
      onFocusCapture={takeFocus}
    >
      <ScreenBoundary screen={t.plan ? "plan" : t.tableDetails ? "table details" : t.diagram ? "er diagram" : t.table ? "table data" : t.notebook ? "notebook" : "editor"}>
        {t.plan ? <PlanFileTab tab={t} /> : t.tableDetails ? <TableDetailsView tab={t} /> : t.diagram ? <ErDiagramView tab={t} /> : t.notebook ? <NotebookTab tab={t} /> : <EditorTab tab={t} />}
      </ScreenBoundary>
    </div>
  );
});

/** A welcome screen action: a command of the registry, named as in the palette. */
function WelcomeButton({ command, primary = false }: { command: CommandId; primary?: boolean }) {
  const { t } = useTranslation();
  const spec = commandSpec(command);
  return (
    <button className={primary ? "primary" : undefined} data-command={command} title={commandTitle(t(spec.label), command)} onClick={() => runCommand(command, activeContext("menu"))}>
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
