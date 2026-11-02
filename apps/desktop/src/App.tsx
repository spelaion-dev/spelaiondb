import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, events } from "./api";
import { Dialogs } from "./components/Dialogs";
import { EditorTab } from "./components/EditorTab";
import { HistoryPanel } from "./components/HistoryPanel";
import { Sidebar, refreshConnections } from "./components/Sidebar";
import { useApp, useDialogs } from "./store";
import { applyLanguage, type LanguagePreference } from "./i18n";

export default function App() {
  const { t } = useTranslation(["app", "shell"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
  const addTab = useApp((s) => s.addTab);
  const closeTab = useApp((s) => s.closeTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const historyOpen = useApp((s) => s.historyOpen);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [language, setLanguage] = useState<LanguagePreference>("system");
  const [languageBusy, setLanguageBusy] = useState(true);
  const [languageError, setLanguageError] = useState<{ phase: "load" | "save"; detail: string } | null>(null);

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
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "t") {
        e.preventDefault();
        useApp.getState().addTab(null);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      void unlisten.then((f) => f());
      window.removeEventListener("keydown", onKey);
    };
  }, []);

  useEffect(() => {
    let active = true;
    void call(commands.getLanguage()).then(async (saved) => {
      if (!active) return;
      const selected: LanguagePreference = saved === "en" || saved === "es" ? saved : "system";
      setLanguage(selected);
      await applyLanguage(selected);
    }).catch((e: unknown) => {
      if (active) setLanguageError({ phase: "load", detail: e instanceof Error ? e.message : String(e) });
    }).finally(() => { if (active) setLanguageBusy(false); });
    return () => { active = false; };
  }, []);

  useEffect(() => {
    const update = () => { if (language === "system") void applyLanguage("system"); };
    window.addEventListener("languagechange", update);
    return () => window.removeEventListener("languagechange", update);
  }, [language]);

  async function changeLanguage(next: LanguagePreference) {
    setLanguageBusy(true);
    setLanguageError(null);
    try {
      await call(commands.setLanguage(next));
      setLanguage(next);
      await applyLanguage(next);
    } catch (e) {
      setLanguageError({ phase: "save", detail: e instanceof Error ? e.message : String(e) });
    } finally {
      setLanguageBusy(false);
    }
  }

  return (
    <div className="app">
      <Sidebar />
      <main className="main">
        {loadError && <div className="banner" role="alert">{loadError}<button onClick={() => void loadConnections()}>{t("retryConnections")}</button></div>}
        {languageError && <div className="banner" role="alert">{t(languageError.phase === "load" ? "loadLanguageError" : "saveLanguageError")} {languageError.detail}</div>}
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
                  closeTab(tab.id);
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
        <label className="language-setting">
          <span>{t("language")}</span>
          <select aria-label={t("language")} value={language} disabled={languageBusy} onChange={(e) => void changeLanguage(e.target.value as LanguagePreference)}>
            <option value="system">{t("system")}</option>
            <option value="en">English</option>
            <option value="es">Español</option>
          </select>
        </label>
        </div>
        <div className="workspace-content">
          <div className="editor-workspace">
            {tabs.length === 0 ? (
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
      <Dialogs />
    </div>
  );
}
