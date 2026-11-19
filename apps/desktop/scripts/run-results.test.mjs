// How a run's events end its results (src/runResults.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { afterRun, endResult, markRefreshable, sameResultColumns, withoutResults, withStartedResult } from "../src/runResults.ts";
import { ended, hasEnded, partialKey, partialReason, partialText } from "../src/resultEnd.ts";

test("refresh does not reuse column positions when result columns change", () => {
  const col = { name: "name", logical: "text", nativeType: "text", masked: false };
  assert.equal(sameResultColumns([col], [{ ...col }]), true);
  for (const changed of [{ ...col, name: "other" }, { ...col, logical: "int" }, { ...col, nativeType: "varchar" }, { ...col, masked: true }]) {
    assert.equal(sameResultColumns([col], [changed]), false);
  }
  assert.equal(sameResultColumns([col], []), false);
});

test("only successful single-statement runs offer grid refresh", () => {
  const results = [{ id: 1, run: 1 }, { id: 2, run: 2 }];
  const good = { statements: 1, executed: 1, failed: 0, stop: null };
  assert.deepEqual(markRefreshable(results, 2, good), [results[0], { ...results[1], refreshable: true }]);
  for (const summary of [{ ...good, statements: 2 }, { ...good, failed: 1 }, { ...good, executed: 0 }, { ...good, stop: "cancelled" }]) {
    assert.equal(markRefreshable(results, 2, summary)[1].refreshable, false);
  }
  assert.equal(markRefreshable([...results, { id: 3, run: 2 }], 2, good)[1].refreshable, false, "a multi-result batch cannot be refreshed into one result tab");
});

const LOADING = { kind: "loading" };
const WHOLE = { kind: "whole" };
const partial = (reason) => ({ kind: "partial", reason });
const started = (statement, rows, paging) => ({ id: 10 + statement, statement, rows, end: LOADING, ...(paging ? { paging } : {}) });

test("a result ends as the core says: whole, or cut short and why (SDB-367)", () => {
  // `select pg_terminate_backend(pg_backend_pid())`: its column arrives, then it fails.
  assert.deepEqual(endResult([started(0, 0)], 10, partial("connectionLost")), [{ id: 10, statement: 0, rows: 0, end: partial("connectionLost") }]);
  assert.deepEqual(endResult([started(0, 5)], 10, WHOLE), [{ id: 10, statement: 0, rows: 5, end: WHOLE }]);
});

test("a cancelled or timed-out result keeps the rows that arrived and says it is partial", () => {
  for (const reason of ["cancelled", "timedOut", "failed"]) {
    const [ended] = endResult([started(0, 1234)], 10, partial(reason));
    assert.deepEqual(ended.end, partial(reason));
    assert.equal(ended.rows, 1234);
    assert.equal(hasEnded(ended.end), true);
    assert.equal(partialReason(ended.end), reason);
  }
});

test("only the result that ended is touched, whatever its statement or run", () => {
  const results = [{ ...started(0, 3), end: WHOLE }, started(1, 2), started(2, 0)];
  const ended = endResult(results, 11, partial("failed"));
  assert.deepEqual(ended.map((r) => r.end.kind), ["whole", "partial", "loading"]);
  assert.equal(ended[0], results[0]);
  assert.equal(ended[2], results[2]);
});

test("a full page may have another after it only when it is whole", () => {
  const page = { sql: "select * from t", pageSize: 100, hasMore: false, loading: false };
  assert.equal(endResult([started(0, 100, page)], 10, WHOLE)[0].paging.hasMore, true);
  assert.equal(endResult([started(0, 40, page)], 10, WHOLE)[0].paging.hasMore, false);
  assert.equal(endResult([started(0, 100, page)], 10, partial("cancelled"))[0].paging.hasMore, false);
});

test("the strip words a partial result by its reason and rows, and says nothing otherwise", () => {
  const numbers = new Intl.NumberFormat("en");
  const said = [];
  const t = (key, values) => {
    said.push([key, values.count]);
    return `${key}:${values.formattedCount}`;
  };
  assert.equal(partialText(partial("cancelled"), 120000, t, numbers), "results.partial.cancelled:120,000");
  assert.equal(partialText(partial("connectionLost"), 1, t, numbers), "results.partial.connectionLost:1");
  assert.equal(partialText(WHOLE, 5, t, numbers), null);
  assert.equal(partialText(LOADING, 5, t, numbers), null);
  assert.deepEqual(said, [["results.partial.cancelled", 120000], ["results.partial.connectionLost", 1]]);
  assert.equal(hasEnded(LOADING), false);
  assert.equal(ended(undefined), false);
  assert.equal(ended({ end: WHOLE }), true);
});

test("every reason has its words in English and Spanish", async () => {
  const { readFile } = await import("node:fs/promises");
  for (const language of ["en", "es"]) {
    const texts = JSON.parse(await readFile(new URL(`../src/locales/${language}/editor.json`, import.meta.url), "utf8"));
    for (const reason of ["cancelled", "failed", "timedOut", "connectionLost"]) {
      const key = partialKey(reason).split(".").pop();
      assert.ok(texts.results.partial[`${key}_one`] && texts.results.partial[`${key}_other`], `${language}: ${reason}`);
    }
  }
});

// Results kept from earlier runs (SDB-378): the core says what a run replaces and where each
// new result goes; these only draw it.
const kept = (id, run, more = {}) => ({ id, run, statement: 0, rows: 1, end: WHOLE, ...more });
const ids = (state) => state.results.map((r) => r.id);

test("a result that ends does not touch the results kept from other runs", () => {
  const page = { sql: "select * from t", pageSize: 100, hasMore: true, loading: false };
  const pinned = kept(1, 1, { rows: 100, paging: page, pinned: true });
  const running = { id: 2, run: 2, statement: 0, rows: 7, end: LOADING };
  const ended = endResult([pinned, running], 2, partial("failed"));
  assert.equal(ended[0], pinned, "same statement number, another run");
  assert.equal(ended[0].paging.hasMore, true);
  assert.deepEqual(ended[1].end, partial("failed"));
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
