// The editor area's split (src/splitView.ts, SDB-30): its transitions, run by `npm run build`
// with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { MAX_SHARE, MIN_SHARE, SINGLE, clampShare, otherPaneTab, paneOf, resized, show, split, survivingPane, withoutTabs } from "../src/splitView.ts";

const pair = (first, second, direction = "right", share = 0.5) => ({ kind: "split", direction, first, second, share });

test("splitting puts the focused tab first and the other beside it, never one tab twice", () => {
  assert.deepEqual(split(SINGLE, "a", "b", "right"), pair("a", "b"));
  assert.deepEqual(split(SINGLE, "a", "b", "down"), pair("a", "b", "down"));
  assert.equal(split(SINGLE, "a", "a", "right"), SINGLE);
  // Already split: the focused tab keeps its pane, and the size stays.
  assert.deepEqual(split(pair("a", "b", "right", 0.3), "b", "c", "down"), pair("c", "b", "down", 0.3));
  assert.deepEqual(split(pair("a", "b", "right", 0.3), "a", "c", "right"), pair("a", "c", "right", 0.3));
});

test("a tab already in a pane only takes the focus; another replaces the focused pane", () => {
  const layout = pair("a", "b");
  assert.equal(show(layout, "a", "b"), layout);
  assert.equal(show(layout, "b", "a"), layout);
  assert.deepEqual(show(layout, "a", "c"), pair("c", "b"));
  assert.deepEqual(show(layout, "b", "c"), pair("a", "c"));
  assert.equal(show(SINGLE, "a", "c"), SINGLE);
});

test("closing a pane's tab ends the split, and the other pane's tab takes the focus", () => {
  const layout = pair("a", "b");
  assert.equal(withoutTabs(layout, new Set(["c"])), layout);
  assert.equal(withoutTabs(layout, new Set(["a"])), SINGLE);
  assert.equal(withoutTabs(layout, new Set(["b", "c"])), SINGLE);
  assert.equal(survivingPane(layout, new Set(["a"])), "b");
  assert.equal(survivingPane(layout, new Set(["b"])), "a");
  assert.equal(survivingPane(layout, new Set(["a", "b"])), null);
  assert.equal(survivingPane(layout, new Set(["c"])), null);
  assert.equal(survivingPane(SINGLE, new Set(["a"])), null);
});

test("panes are found by tab, and the size stays within its limits", () => {
  const layout = pair("a", "b");
  assert.equal(paneOf(layout, "a"), "first");
  assert.equal(paneOf(layout, "b"), "second");
  assert.equal(paneOf(layout, "c"), null);
  assert.equal(paneOf(SINGLE, "a"), null);
  assert.equal(otherPaneTab(layout, "a"), "b");
  assert.equal(otherPaneTab(layout, "b"), "a");
  assert.equal(otherPaneTab(layout, "c"), null);
  assert.equal(resized(layout, 0.05).share, MIN_SHARE);
  assert.equal(resized(layout, 0.99).share, MAX_SHARE);
  assert.equal(resized(layout, 0.5), layout);
  assert.equal(resized(SINGLE, 0.3), SINGLE);
  assert.equal(clampShare(Number.NaN), 0.5);
});

test("a layout the module does not know is refused, not read as one it knows", () => {
  assert.throws(() => paneOf({ kind: "grid" }, "a"), /unknown split layout/);
  assert.throws(() => show({ kind: "grid" }, "a", "b"), /unknown split layout/);
});
