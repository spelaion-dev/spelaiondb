import { createRoot } from "react-dom/client";
import App from "./App";
import { ScreenBoundary } from "./components/ScreenBoundary";
import "./i18n";
import { usePreferences } from "./preferences";
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
void usePreferences
  .getState()
  .load()
  .finally(() => {
    // A screen that fails shows its own panel (SDB-398); this one catches whatever is left.
    createRoot(root).render(<ScreenBoundary screen="app" className="screen-failed-app"><App /></ScreenBoundary>);
    // The user's snippets and the template of new scripts: read once, before a tab is opened
    // by hand. They are text for the editor; a failure only leaves the built-in snippets.
    void useSnippets.getState().load();
    // After the first render: a slow network never delays the window.
    void useUpdate.getState().start();
  });
