import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { events } from "./api";
import { AddConnection } from "./components/AddConnection";
import { DeleteConnection } from "./components/DeleteConnection";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { HistoryPanel } from "./components/HistoryPanel";
import { SettingsTab } from "./components/SettingsTab";
import { Sidebar, refreshConnections } from "./components/Sidebar";
import { closeQueryTab } from "./connect";
import { usePreferences } from "./preferences";
import { SETTINGS_TAB, useApp, useDialogs } from "./store";

export default function App() {
  const { t } = useTranslation(["app", "shell", "settings"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
  const addTab = useApp((s) => s.addTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const historyOpen = useApp((s) => s.historyOpen);
  const settingsOpen = useApp((s) => s.settingsOpen);
  const closeSettings = useApp((s) => s.closeSettings);
  const connectionDialog = useApp((s) => s.connectionDialog);
  const deletingConnection = useApp((s) => s.deletingConnection);
  // Save errors are shown next to the setting; only startup load errors need a banner.
  const preferenceError = usePreferences((s) => (s.error?.phase === "load" ? s.error : null));
  const [loadError, setLoadError] = useState<string | null>(null);

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
      } else if (mod && key === "w") {
        e.preventDefault();
        if (app.activeTab === SETTINGS_TAB) app.closeSettings();
        else if (app.activeTab) closeQueryTab(app.activeTab);
      } else if (mod && (e.key === "Tab" || e.key === "PageDown" || e.key === "PageUp")) {
        // Next or previous tab, the settings tab included, wrapping around.
        const order = [...app.tabs.map((t) => t.id), ...(app.settingsOpen ? [SETTINGS_TAB] : [])];
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
    <div className="app">
      <Sidebar />
      <main className="main">
        {loadError && <div className="banner" role="alert">{loadError}<button onClick={() => void loadConnections()}>{t("retryConnections")}</button></div>}
        {preferenceError && <div className="banner" role="alert">{t("preferenceLoadError")} {preferenceError.detail}</div>}
        <div className="tab-strip">
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
              <span>{t("shell:queryTab", { number: tab.number })}</span>
              <button
                className="close"
                aria-label={t("closeTab", { name: t("shell:queryTab", { number: tab.number }) })}
                onClick={(e) => {
                  e.stopPropagation();
                  closeQueryTab(tab.id);
                }}
              >
                ×
              </button>
            </div>
          ))}
          {settingsOpen && (
            <div
              role="tab"
              aria-selected={activeTab === SETTINGS_TAB}
              className={`tab settings-tab ${activeTab === SETTINGS_TAB ? "active" : ""}`}
              onClick={() => setActiveTab(SETTINGS_TAB)}
            >
              <span aria-hidden="true">⚙</span>
              <span>{t("settings:title")}</span>
              <button
                className="close"
                aria-label={t("closeTab", { name: t("settings:title") })}
                onClick={(e) => {
                  e.stopPropagation();
                  closeSettings();
                }}
              >
                ×
              </button>
            </div>
          )}
          <button className="new-tab" onClick={() => addTab(null)} title={t("newQueryShortcut")}>
            +
          </button>
        </div>
        </div>
        <div className="workspace-content">
          <div className="editor-workspace">
            {settingsOpen && (
              <div className="tab-panel" hidden={activeTab !== SETTINGS_TAB}>
                <SettingsTab />
              </div>
            )}
            {tabs.length === 0 && !settingsOpen ? (
              <div className="welcome">
                <h1>SpelunkDB</h1>
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
          {historyOpen && <HistoryPanel />}
        </div>
      </main>
      {connectionDialog && (
        <AddConnection key={connectionDialog.kind === "edit" ? connectionDialog.id : "add"} dialog={connectionDialog} />
      )}
      {deletingConnection && <DeleteConnection key={deletingConnection.id} connection={deletingConnection} />}
      <Dialogs />
    </div>
  );
}
