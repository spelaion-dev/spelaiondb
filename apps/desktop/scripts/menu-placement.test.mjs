// Where context menus open (src/menuPlacement.ts), run by `npm run build` with `node --test`:
// at the click when the menu fits, otherwise moved up or left to stay inside the window, and
// every explorer row menu placed that way (SDB-246: an account at the bottom of a long explorer
// opened its menu below the window).
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import { MENU_MARGIN, fitInWindow } from "../src/menuPlacement.ts";

const VIEW = { width: 1280, height: 800 };
const fit = (x, y, width, height) => fitInWindow(x, y, width, height, VIEW.width, VIEW.height);

test("a menu that fits opens where it was clicked", () => {
  assert.deepEqual(fit(300, 200, 220, 180), { left: 300, top: 200 });
  // Exactly up to the margin still fits.
  assert.deepEqual(fit(VIEW.width - 220 - MENU_MARGIN, VIEW.height - 180 - MENU_MARGIN, 220, 180), {
    left: VIEW.width - 220 - MENU_MARGIN,
    top: VIEW.height - 180 - MENU_MARGIN,
  });
});

test("a menu opened near the bottom moves up, and near the right edge moves left", () => {
  // The account row at the bottom of the explorer: only `top` changes.
  assert.deepEqual(fit(120, 780, 220, 180), { left: 120, top: VIEW.height - 180 - MENU_MARGIN });
  assert.deepEqual(fit(1250, 200, 220, 180), { left: VIEW.width - 220 - MENU_MARGIN, top: 200 });
  assert.deepEqual(fit(1279, 799, 220, 180), {
    left: VIEW.width - 220 - MENU_MARGIN,
    top: VIEW.height - 180 - MENU_MARGIN,
  });
});

test("a menu larger than the window starts at the margin, never above or left of it", () => {
  assert.deepEqual(fit(500, 500, 2000, 1000), { left: MENU_MARGIN, top: MENU_MARGIN });
  assert.deepEqual(fit(-20, -20, 220, 180), { left: MENU_MARGIN, top: MENU_MARGIN });
});

test("every explorer row menu is kept inside the window", () => {
  const sidebar = readFileSync(resolve(import.meta.dirname, "../src/components/Sidebar.tsx"), "utf8");
  // Placed at the raw click point, or clamped with a guessed size, as before SDB-246.
  assert.doesNotMatch(sidebar, /left:\s*menu\.x/);
  assert.doesNotMatch(sidebar, /Math\.min\(menu\.[xy]/);
  // The menus are `ObjectMenu`s, which place themselves with `useMenuInWindow`.
  assert.ok((sidebar.match(/<ObjectMenu at=\{menu\}/g) ?? []).length >= 9);
  assert.match(sidebar, /function ObjectMenu\([^]*?useMenuInWindow\(ref, at\.x, at\.y\)/);
  const raw = sidebar.match(/<div[^>]*className="grid-menu object-menu[^"]*"/g) ?? [];
  assert.deepEqual(
    raw.map((tag) => tag.match(/className="([^"]*)"/)[1]),
    ["grid-menu object-menu explorer-generate-items"],
    "only the Generate SQL submenu (placed next to its item) is a menu of its own",
  );
});
