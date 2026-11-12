// When the cell panel's map fits its shapes (src/mapFit.ts, SDB-280), run by `npm run build`
// with `node --test`. The map used to fit only new shapes and its first size: a size change
// after that (a line appearing above the map, the panel resized) kept a view fitted for the old
// size, so 0 then gave a different view. Now an automatic fit follows the map's size until the
// user pans or zooms.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import { fitted, moved, shouldFit, sizeKey } from "../src/mapFit.ts";

const box = { minX: 0, minY: 0, maxX: 1, maxY: 1 };
const otherBox = { minX: 0, minY: 0, maxX: 2, maxY: 2 };

test("the first size and new shapes are fitted", () => {
  assert.equal(shouldFit(null, box, 0, 0), false, "no size yet");
  assert.equal(shouldFit(null, box, 300, 0), false);
  assert.equal(shouldFit(null, box, 300, 200), true, "the first size");
  const memory = fitted(box, 300, 200);
  assert.equal(shouldFit(memory, box, 300, 200), false, "nothing changed");
  assert.equal(shouldFit(memory, otherBox, 300, 200), true, "new shapes");
  // New shapes are fitted even after the user moved the view.
  assert.equal(shouldFit(moved(memory), otherBox, 300, 200), true);
});

test("a size change fits again while the view is still the automatic fit", () => {
  // The map took its first size, then a line appeared above it: 20 px shorter.
  let memory = fitted(box, 300, 200);
  assert.equal(shouldFit(memory, box, 300, 180), true, "the fit follows the map's real size");
  memory = fitted(box, 300, 180);
  assert.equal(memory.size, sizeKey(300, 180));
  assert.equal(shouldFit(memory, box, 300, 180), false, "and then stays");
  // Wider panel, same shapes: fitted again too.
  assert.equal(shouldFit(memory, box, 420, 180), true);
});

test("once the user pans or zooms, a size change keeps their view", () => {
  const memory = moved(fitted(box, 300, 200));
  assert.equal(memory.following, false);
  assert.equal(shouldFit(memory, box, 300, 180), false);
  assert.equal(shouldFit(memory, box, 640, 480), false);
  // 0 (or the fit button) fits again, and the fit follows the size from then on.
  const refit = fitted(box, 640, 480);
  assert.equal(shouldFit(refit, box, 600, 480), true);
  assert.equal(moved(null), null);
});

test("the map's fit goes through that policy, and every gesture leaves it", () => {
  const map = readFileSync(resolve(import.meta.dirname, "../src/components/MapView.tsx"), "utf8");
  // The fit effect asks the policy; nothing else decides when to fit.
  assert.match(map, /useEffect\(\(\) => \{\s*if \(shouldFit\(fitted\.current, box, size\.width, size\.height\)\) fit\(\);/);
  assert.doesNotMatch(map, /ready: boolean/, "the old first-size-only memory is gone");
  // Wheel and +/− (zoomAt), the arrows (pan) and a drag all make the view the user's.
  assert.match(map, /const zoomAt = useCallback\(\s*\(factor: number, at\?: XY\) => \{\s*leaveFit\(\);/);
  assert.match(map, /const pan = \(dx: number, dy: number\) => \{\s*leaveFit\(\);/);
  assert.match(map, /d\.moved = true;\s*leaveFit\(\);/);
  // The smoke test reads the fitted size against the map's own.
  assert.match(map, /data-size=\{sizeKey\(size\.width, size\.height\)\}/);
  assert.match(map, /data-fit=\{autoFit \?\? undefined\}/);
});
