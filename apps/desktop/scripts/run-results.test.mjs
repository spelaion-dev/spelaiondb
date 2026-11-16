// How a run's events end its results (src/runResults.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { endStatementResults } from "../src/runResults.ts";

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
