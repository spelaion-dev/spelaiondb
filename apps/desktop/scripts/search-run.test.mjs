// The search panel's Cancel (SDB-422): applied to the run as it is, a search that has already
// ended stays ended instead of going back to "Cancelling…" forever.
import assert from "node:assert/strict";
import test from "node:test";
import { markCancelling } from "../src/searchRun.ts";

const running = { id: 7, running: true, cancelling: false, summary: null };

test("a running search is marked as cancelling", () => {
  assert.deepEqual(markCancelling(running, 7), { ...running, cancelling: true });
});

test("a search that ended before the cancel is applied stays ended", () => {
  // The click handler saw `running`; the search ended before React applied the cancel.
  const ended = { ...running, running: false, summary: { tables: 4, hits: 0, cancelled: false } };
  assert.equal(markCancelling(ended, 7), ended);
});

test("another search, no search, and a second click change nothing", () => {
  const next = { ...running, id: 8 };
  assert.equal(markCancelling(next, 7), next);
  assert.equal(markCancelling(null, 7), null);
  const cancelling = { ...running, cancelling: true };
  assert.equal(markCancelling(cancelling, 7), cancelling);
});
