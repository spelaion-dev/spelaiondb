// Which running tabs the close question lists (src/closeQuestion.ts, SDB-253), run by
// `npm run build` with `node --test`: a tab with an open transaction is listed once, under it.
import assert from "node:assert/strict";
import test from "node:test";
import { runningOutsideTransactions } from "../src/closeQuestion.ts";

test("a running tab with an open transaction is listed only under its transaction", () => {
  assert.deepEqual(runningOutsideTransactions(["a"], ["a"]), []);
});

test("running tabs without a transaction stay listed as running, in their order", () => {
  assert.deepEqual(runningOutsideTransactions(["c", "a", "b"], ["a"]), ["c", "b"]);
  assert.deepEqual(runningOutsideTransactions(["a", "b"], []), ["a", "b"]);
});

test("a transaction without a running query adds nothing to the running list", () => {
  assert.deepEqual(runningOutsideTransactions([], ["a", "b"]), []);
});
