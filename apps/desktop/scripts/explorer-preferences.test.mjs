// The explorer's buttons that change a preference (src/components/Sidebar.tsx), run by
// `npm run build` with `node --test` (SDB-273, SDB-83). Preference saves are queued
// (src/saveQueue.ts): a change made while another is saving is saved after it, never dropped.
// So none of these buttons is disabled while a preference saves (a disabled button would drop
// the quick second click the queue keeps), and each builds its change from the store's state at
// the click, not from the render's, so two clicks before a render both count.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

const sidebar = readFileSync(resolve(import.meta.dirname, "../src/components/Sidebar.tsx"), "utf8");

/** The `<button …>…</button>` around each `setExplorer` call. */
function buttonsCallingSetExplorer() {
  const calls = [...sidebar.matchAll(/setExplorer\(/g)].map((m) => m.index);
  assert.ok(calls.length >= 3, `found ${calls.length} setExplorer calls`);
  return calls.map((at) => {
    const start = sidebar.lastIndexOf("<button", at);
    const end = sidebar.indexOf("</button>", at);
    assert.ok(start >= 0 && end > at, `a button around the setExplorer call at ${at}`);
    return sidebar.slice(start, end);
  });
}

test("no explorer button that saves a preference is disabled while one saves", () => {
  for (const button of buttonsCallingSetExplorer()) {
    assert.ok(
      !/disabled=\{[^}]*saving/i.test(button),
      `this button drops clicks while a preference saves:\n${button.slice(0, 300)}`,
    );
  }
  assert.ok(!sidebar.includes("savingPreference"), "the explorer no longer waits for preference saves");
});

test("each explorer button builds its change from the store's state at the click", () => {
  for (const button of buttonsCallingSetExplorer()) {
    assert.ok(button.includes("usePreferences.getState()"), `reads the store at the click:\n${button.slice(0, 300)}`);
    assert.ok(!/setExplorer\(\{\s*\.\.\.explorer\b/.test(button), `uses the render's explorer:\n${button.slice(0, 300)}`);
  }
});
