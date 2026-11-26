import { createRoot } from "react-dom/client";
import App from "./App";
import { commands } from "./api";
import { ResultsWindow } from "./components/ResultsWindow";
import { setWindowContext, windowContext } from "./windowContext";
import { ScreenBoundary } from "./components/ScreenBoundary";
import "./i18n";
import { followPreferences } from "./preferenceSync";
import { usePreferences } from "./preferences";
import { useShortcuts } from "./shortcutPreferences";
import { useSnippets } from "./snippetStore";
import "./styles.css";
import { useUpdate } from "./update";

// WebKitGTK (Linux) opens the browser's own menu (Back, Reload, Inspect…) on a right click
// anywhere the app has no menu of its own. Text fields and selected text keep it, for copy
// and paste; development builds keep it everywhere, for the inspector.
window.addEventListener("contextmenu", (e) => {
  if (import.meta.env.DEV || e.defaultPrevented) return;
  const target = e.target;
  if (target instanceof HTMLElement && (target.isContentEditable || target.closest("input, textarea"))) return;
  if (window.getSelection()?.toString()) return;
  e.preventDefault();
});

const root = document.getElementById("root");
if (!root) throw new Error("missing #root");
// Theme and language are applied before the first render so the window never flashes the
// wrong ones; a failure is shown in the app instead of blocking it.
// The user's shortcuts too (SDB-447): the first render already shows and takes their keys; a
// failure leaves the defaults.
// What this window shows comes from the core (SDB-30): the main window, a connection's window
// or a tab's results; a failure to ask is the main window, as before there were others.
// Before they are read: a preference saved in another window meanwhile is read again (SDB-535).
followPreferences();
void Promise.all([
  usePreferences.getState().load(),
  useShortcuts.getState().load(),
  commands.windowContext().then(setWindowContext, () => undefined),
])
  .finally(() => {
    const context = windowContext();
    // A screen that fails shows its own panel (SDB-398); this one catches whatever is left.
    createRoot(root).render(
      <ScreenBoundary screen="app" className="screen-failed-app">
        {context.kind === "results" ? <ResultsWindow context={context} /> : <App />}
      </ScreenBoundary>,
    );
    // The user's snippets and the template of new scripts: read once, before a tab is opened
    // by hand. They are text for the editor; a failure only leaves the built-in snippets.
    void useSnippets.getState().load();
    // After the first render: a slow network never delays the window. Once, from the main one.
    if (context.kind === "main") void useUpdate.getState().start();
    // The other windows only read whether to check, for their Settings (SDB-535).
    else void useUpdate.getState().readCheckAtStartup();
  });
