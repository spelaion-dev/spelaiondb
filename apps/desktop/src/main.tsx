import { createRoot } from "react-dom/client";
import App from "./App";
import "./i18n";
import { usePreferences } from "./preferences";
import "./styles.css";
import { useUpdate } from "./update";

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
