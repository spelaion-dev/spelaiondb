// How a run's events end its results (src/runResults.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { afterRun, endStatementResults, withoutResults, withStartedResult } from "../src/runResults.ts";

const started = (statement, rows, paging) => ({ id: 10 + statement, statement, rows, complete: false, ...(paging ? { paging } : {}) });

test("a statement that failed after its result started leaves it complete, not loading", () => {
  // `select pg_terminate_backend(pg_backend_pid())`: its column arrives, then it fails.
  const results = [started(0, 0)];
  assert.deepEqual(endStatementResults(results, 0, "failed"), [{ id: 10, statement: 0, rows: 0, complete: true }]);
});

test("a cancelled or timed-out statement keeps the rows that arrived", () => {
  // A cancel and a timeout end the statement with a failure too.
  const [ended] = endStatementResults([started(0, 1234)], 0, "failed");
  assert.equal(ended.complete, true);
  assert.equal(ended.rows, 1234);
});

test("only the statement that ended is touched", () => {
  const results = [{ ...started(0, 3), complete: true }, started(1, 2), started(2, 0)];
  const ended = endStatementResults(results, 1, "failed");
  assert.deepEqual(ended.map((r) => r.complete), [true, true, false]);
  assert.equal(ended[0], results[0]);
  assert.equal(ended[2], results[2]);
});

test("every result of a statement ends with it (several result sets of one batch)", () => {
  const ended = endStatementResults([started(0, 1), started(0, 0), started(1, 0)], 0, "failed");
  assert.deepEqual(ended.map((r) => r.complete), [true, true, false]);
});

test("a full page may have another after it only when its statement finished", () => {
  const page = { sql: "select * from t", pageSize: 100, hasMore: false, loading: false };
  assert.equal(endStatementResults([started(0, 100, page)], 0, "finished")[0].paging.hasMore, true);
  assert.equal(endStatementResults([started(0, 40, page)], 0, "finished")[0].paging.hasMore, false);
  assert.equal(endStatementResults([started(0, 100, page)], 0, "failed")[0].paging.hasMore, false);
});

test("a statement that finished completes its results as before", () => {
  assert.deepEqual(endStatementResults([started(0, 5)], 0, "finished"), [{ id: 10, statement: 0, rows: 5, complete: true }]);
});

// Results kept from earlier runs (SDB-378): the core says what a run replaces and where each
// new result goes; these only draw it.
const kept = (id, run, more = {}) => ({ id, run, statement: 0, rows: 1, complete: true, ...more });
const ids = (state) => state.results.map((r) => r.id);

test("a statement that ends does not touch the results kept from other runs", () => {
  const page = { sql: "select * from t", pageSize: 100, hasMore: true, loading: false };
  const pinned = kept(1, 1, { rows: 100, paging: page, pinned: true });
  const running = { id: 2, run: 2, statement: 0, rows: 7, complete: false };
  const ended = endStatementResults([pinned, running], 0, "failed", 2);
  assert.equal(ended[0], pinned, "same statement number, another run");
  assert.equal(ended[0].paging.hasMore, true);
  assert.equal(ended[1].complete, true);
});

test("the results a run replaced go away; a closed active tab gives way to its neighbour", () => {
  const state = { results: [kept(1, 1), kept(2, 1), kept(3, 1)], activeResult: 2 };
  assert.deepEqual(ids(withoutResults(state, [1])), [2, 3]);
  assert.equal(withoutResults(state, [1]).activeResult, 2);
  assert.equal(withoutResults(state, [2]).activeResult, 3);
  assert.equal(withoutResults(state, [2, 3]).activeResult, 1);
  assert.equal(withoutResults(state, [1, 2, 3]).activeResult, null);
  // Nothing of this tab closed: the same state, so nothing is drawn again.
  assert.equal(withoutResults(state, []), state);
  assert.equal(withoutResults(state, [99]), state);
});

test("a run's first result is shown, next to the pinned ones; its later results are not", () => {
  const pinned = kept(1, 1, { pinned: true });
  let state = { results: [pinned], activeResult: 1 };
  state = withStartedResult(state, kept(5, 2), null, []);
  assert.deepEqual(ids(state), [1, 5]);
  assert.equal(state.activeResult, 5);
  state = withStartedResult(state, kept(6, 2), null, []);
  assert.deepEqual(ids(state), [1, 5, 6]);
  assert.equal(state.activeResult, 5);
  assert.equal(state.results[0], pinned, "the pinned result is the same object: nothing of it changes");
});

test("a re-run takes the place of its result, and its other results stay next to it", () => {
  let state = { results: [kept(1, 1), kept(2, 1), kept(3, 1)], activeResult: 3 };
  state = withStartedResult(state, kept(7, 2, { name: "Totals", pinned: true }), 2, []);
  assert.deepEqual(ids(state), [1, 7, 3]);
  assert.equal(state.activeResult, 7, "the first result of the run is shown");
  state = withStartedResult(state, kept(8, 2), null, []);
  assert.deepEqual(ids(state), [1, 7, 8, 3]);
  assert.equal(state.activeResult, 7);
});

test("results the limit closed leave with the result that starts", () => {
  const state = { results: [kept(1, 1), kept(2, 2), kept(3, 3)], activeResult: 1 };
  const next = withStartedResult(state, kept(4, 4), null, [1]);
  assert.deepEqual(ids(next), [2, 3, 4]);
  assert.equal(next.activeResult, 4);
});

test("a run without a result of its own shows its messages; one with results keeps its tab", () => {
  const state = { results: [kept(1, 1, { pinned: true })], activeResult: 1 };
  assert.equal(afterRun(state, 2).activeResult, null);
  assert.equal(afterRun(state, 1), state);
});
