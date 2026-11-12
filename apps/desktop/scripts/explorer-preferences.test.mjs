// The explorer's buttons that change a preference (src/components/Sidebar.tsx), run by
// `npm run build` with `node --test` (SDB-273). `setExplorer` ignores a change while the previous
// one is being saved, so each of those buttons is disabled meanwhile: a click that came too soon
// was otherwise dropped without a sign (the "connected only" button kept its first state).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

const sidebar = readFileSync(resolve(import.meta.dirname, "../src/components/Sidebar.tsx"), "utf8");

test("every explorer button that saves a preference is disabled while one is saved", () => {
  const calls = [...sidebar.matchAll(/setExplorer\(/g)].map((m) => m.index);
  assert.ok(calls.length >= 3, `found ${calls.length} setExplorer calls`);
  for (const at of calls) {
    const start = sidebar.lastIndexOf("<button", at);
    const end = sidebar.indexOf("</button>", at);
    assert.ok(start >= 0 && end > at, `a button around the setExplorer call at ${at}`);
    const button = sidebar.slice(start, end);
    assert.ok(
      button.includes("disabled={savingPreference}"),
      `this button saves a preference but is not disabled while one is saved:\n${button.slice(0, 300)}`,
    );
  }
});
