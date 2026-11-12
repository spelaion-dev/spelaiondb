// The result grid's scroll past the browser's height cap (src/grid/scroll.ts, SDB-109), run by
// `npm run build` with `node --test`: below the cap a scroll position is the content position;
// past it the spacer stays under the cap and both ends of its scroll reach the first and the
// last row of results of millions of rows.
import assert from "node:assert/strict";
import test from "node:test";
import { MAX_SPACER_H, isScaled, maxContentTop, maxScrollTop, scrollMap, toContent, toScroll, wheelPixels } from "../src/grid/scroll.ts";

const ROW_H = 24;
const HEADER_H = 34;
const VIEW = 600;
// Chromium's and WebKit's cap on an element's height.
const BROWSER_CAP = 33_554_428;
const content = (rows) => HEADER_H + rows * ROW_H;
// The row at the top of the grid, and the last one fully shown, for a content position.
const firstRow = (top) => Math.floor(top / ROW_H);
const lastRow = (top) => Math.floor((top + VIEW - HEADER_H - 1) / ROW_H);

test("the spacer stays under the browsers' cap, and a million rows are not scaled", () => {
  assert.ok(MAX_SPACER_H < BROWSER_CAP);
  const million = scrollMap(content(1_000_000), VIEW);
  assert.equal(isScaled(million), false);
  assert.equal(million.spacerH, content(1_000_000));
  for (const top of [0, 1, 24 * 12_345, 1234.5, maxScrollTop(million)]) {
    assert.equal(toContent(million, top), top);
    assert.equal(toScroll(million, top), top);
  }
  for (const rows of [5_000_000, 100_000_000, 1_000_000_000]) {
    const map = scrollMap(content(rows), VIEW);
    assert.equal(isScaled(map), true);
    assert.equal(map.spacerH, MAX_SPACER_H);
  }
});

test("both ends of a scaled scroll reach the first and the last row", () => {
  for (const rows of [1_400_000, 5_000_000, 100_000_000, 1_000_000_000]) {
    const map = scrollMap(content(rows), VIEW);
    assert.equal(toContent(map, 0), 0);
    assert.equal(firstRow(toContent(map, 0)), 0);
    const end = toContent(map, maxScrollTop(map));
    assert.equal(end, maxContentTop(map));
    assert.equal(lastRow(end), rows - 1, `${rows} rows: the last row is shown at the end`);
    // Past the end is the end.
    assert.equal(toContent(map, maxScrollTop(map) + 5000), end);
    assert.equal(toScroll(map, end * 2), maxScrollTop(map));
    assert.equal(toScroll(map, -10), 0);
  }
});

test("every row of a scaled result has a content position that maps back to itself", () => {
  const rows = 5_000_000;
  const map = scrollMap(content(rows), VIEW);
  for (const row of [0, 1, 1_398_100, 2_500_000, rows - 25, rows - 1]) {
    const top = Math.min(row * ROW_H, maxContentTop(map));
    const scroll = toScroll(map, top);
    assert.ok(scroll >= 0 && scroll <= maxScrollTop(map));
    assert.ok(Math.abs(toContent(map, scroll) - top) < 1e-6, `row ${row}`);
    assert.ok(firstRow(top) <= row && row <= lastRow(top), `row ${row} is shown`);
  }
  // A container pixel moves the content by the scale, never more than the viewport.
  const scale = maxContentTop(map) / maxScrollTop(map);
  assert.ok(Math.abs(toContent(map, 1) - scale) < 1e-9);
  assert.ok(scale < VIEW);
});

test("a viewport taller than the content scrolls nowhere", () => {
  const map = scrollMap(content(10), VIEW);
  assert.equal(isScaled(map), false);
  assert.equal(maxContentTop(map), 0);
  assert.equal(toContent(map, 100), 0);
  assert.equal(toScroll(map, 100), 0);
});

test("wheel deltas in pixels, lines and pages", () => {
  assert.equal(wheelPixels(100, 0, ROW_H, 500), 100);
  assert.equal(wheelPixels(-3, 1, ROW_H, 500), -72);
  assert.equal(wheelPixels(1, 2, ROW_H, 500), 500);
});
