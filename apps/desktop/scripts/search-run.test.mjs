// The search panel's data search (src/searchRun.ts): one phase, and Cancel (SDB-422) applied to
// the run as it is, so a search that has already ended stays ended instead of going back to
// "Cancelling…" forever.
import assert from "node:assert/strict";
import test from "node:test";
import { ended, failed, markCancelling, searching, startedRun, withEvent } from "../src/searchRun.ts";

const running = startedRun(7, "c1", 2);
const summary = { kind: "done", result: { tables: 2, hits: 0 } };

test("a running search is marked as cancelling", () => {
  assert.deepEqual(markCancelling(running, 7), { ...running, phase: { kind: "cancelling" } });
  assert.equal(searching(markCancelling(running, 7)), true);
});

test("a search that ended before the cancel is applied stays ended", () => {
  // The click handler saw `running`; the search ended before React applied the cancel.
  const done = ended(running, 7, summary);
  assert.equal(markCancelling(done, 7), done);
  assert.equal(searching(done), false);
});

test("another search, no search, and a second click change nothing", () => {
  const next = { ...running, id: 8 };
  assert.equal(markCancelling(next, 7), next);
  assert.equal(markCancelling(null, 7), null);
  const cancelling = markCancelling(running, 7);
  assert.equal(markCancelling(cancelling, 7), cancelling);
});

test("a search reads its tables and ends one way", () => {
  let run = withEvent(running, 7, { kind: "started", index: 0, total: 2, schema: "s", table: "a" });
  assert.deepEqual(run.phase, { kind: "running", current: { index: 0, schema: "s", table: "a" } });
  run = withEvent(run, 7, { kind: "finished", schema: "s", table: "a", status: { kind: "noColumns" } });
  assert.equal(run.tables.length, 1);
  // Events and endings of another search are not this one's.
  assert.equal(withEvent(run, 8, { kind: "finished", schema: "s", table: "b", status: { kind: "noColumns" } }), run);
  assert.equal(ended(run, 8, summary), run);
  // A cancelled search still counts its tables, and the core's ending replaces "cancelling".
  const cancelling = withEvent(markCancelling(run, 7), 7, { kind: "started", index: 1, total: 2, schema: "s", table: "b" });
  assert.deepEqual(cancelling.phase, { kind: "cancelling" });
  assert.deepEqual(ended(cancelling, 7, summary).phase, { kind: "ended", outcome: summary });
  assert.deepEqual(failed(run, 7, "refused").phase, { kind: "failed", message: "refused" });
});
