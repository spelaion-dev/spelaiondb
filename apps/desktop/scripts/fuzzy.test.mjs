// The command palette's fuzzy search (src/fuzzy.ts), run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { fuzzyMatch } from "../src/fuzzy.ts";

const rank = (query, texts) =>
  texts
    .map((text) => ({ text, match: fuzzyMatch(query, text) }))
    .filter((r) => r.match)
    .sort((a, b) => b.match.score - a.match.score)
    .map((r) => r.text);

test("an empty query matches everything with no highlight", () => {
  assert.deepEqual(fuzzyMatch("", "orders"), { score: 0, positions: [] });
  assert.deepEqual(fuzzyMatch("   ", "orders"), { score: 0, positions: [] });
});

test("letters must all appear, in order, ignoring case", () => {
  assert.ok(fuzzyMatch("ORD", "orders"));
  assert.ok(fuzzyMatch("tclo", "Tab: Close"));
  assert.equal(fuzzyMatch("ctab", "Tab: Close"), null);
  assert.equal(fuzzyMatch("sro", "orders"), null);
  assert.equal(fuzzyMatch("ordersx", "orders"), null);
  assert.equal(fuzzyMatch("z", "orders"), null);
});

test("words may come in any order", () => {
  assert.ok(fuzzyMatch("close tab", "Tab: Close others"));
  assert.ok(fuzzyMatch("tab close", "Tab: Close others"));
  assert.equal(fuzzyMatch("tab drop", "Tab: Close others"), null);
});

test("matches mark the best characters: word starts and runs of letters", () => {
  // "Close others": the o that starts a word is worth its longer gap.
  assert.deepEqual(fuzzyMatch("co", "Tab: Close others")?.positions, [5, 11]);
  assert.deepEqual(fuzzyMatch("clo", "Tab: Close others")?.positions, [5, 6, 7]);
  assert.deepEqual(fuzzyMatch("cot", "Tab: Close others")?.positions, [5, 11, 12]);
  assert.deepEqual(fuzzyMatch("bo", "big_orders")?.positions, [0, 4]);
  assert.deepEqual(fuzzyMatch("oi", "orderItems")?.positions, [0, 5]);
});

test("exact, prefix and word-start matches rank first; shorter texts win ties", () => {
  assert.deepEqual(rank("orders", ["big_orders", "orders_archive", "orders", "customer_orders_view"]), [
    "orders",
    "orders_archive",
    "big_orders",
    "customer_orders_view",
  ]);
  assert.deepEqual(rank("ord", ["products", "orders", "big_orders"]), ["orders", "big_orders"]);
  assert.deepEqual(rank("nq", ["General: Open object by name…", "Tab: New query"]), ["Tab: New query"]);
  assert.equal(rank("rs", ["Editor: Run script", "Tab: Close to the right"])[0], "Editor: Run script");
});

test("characters outside the basic plane are counted once", () => {
  assert.deepEqual(fuzzyMatch("t", "🐘 table")?.positions, [2]);
});
