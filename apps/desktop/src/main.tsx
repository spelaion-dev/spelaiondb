import { createRoot } from "react-dom/client";
import App from "./App";
import "./i18n";
import { usePreferences } from "./preferences";
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
    createRoot(root).render(<App />);
    // After the first render: a slow network never delays the window.
    void useUpdate.getState().start();
  });
