// How an editor tab words its session's run state (src/runState.ts, SDB-370) and when it sums
// a run up (src/scriptRun.ts), run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { isCancelling, runStatus } from "../src/runState.ts";
import { showsSummary, summaryMessages } from "../src/scriptRun.ts";

const locale = (language) => JSON.parse(readFileSync(new URL(`../src/locales/${language}/editor.json`, import.meta.url), "utf8"));

test("the status says the statement a run is paused at, and that a cancel is on its way", () => {
  assert.deepEqual(runStatus({ kind: "running", statement: 2 }), { key: "results.running" });
  assert.deepEqual(runStatus({ kind: "paused", statement: 2, ticket: 7, remaining: 1 }), { key: "results.paused", values: { index: 3 } });
  assert.deepEqual(runStatus({ kind: "cancelling", statement: 0 }), { key: "results.cancelling" });
  // Before the core's first word, and once idle while the page still draws the run's end.
  assert.deepEqual(runStatus(undefined), { key: "results.running" });
  assert.deepEqual(runStatus({ kind: "idle" }), { key: "results.running" });
  assert.throws(() => runStatus({ kind: "sleeping" }), /unknown run state/);
});

test("Cancel is offered again only once the core no longer says it is cancelling", () => {
  assert.equal(isCancelling({ kind: "cancelling", statement: 1 }), true);
  for (const state of [undefined, { kind: "idle" }, { kind: "running", statement: 1 }, { kind: "paused", statement: 0, ticket: 1, remaining: 0 }]) {
    assert.equal(isCancelling(state), false, JSON.stringify(state));
  }
});

test("a single statement that failed is not summed up again; one that never ran is", () => {
  const one = { statements: 1, executed: 1, succeeded: 0, failed: 1, notRun: 0, stop: { kind: "failed", index: 0 }, executionMs: 1, fetchMs: 0 };
  assert.equal(showsSummary(one), false);
  assert.equal(showsSummary({ ...one, executed: 0, failed: 0, notRun: 1, stop: { kind: "cancelled", index: 0 } }), true);
  assert.equal(showsSummary({ ...one, statements: 2, notRun: 1 }), true);
});

test("a lost session and a closed one say different things, in both languages", () => {
  const lost = summaryMessages({ statements: 3, executed: 2, succeeded: 1, failed: 1, notRun: 1, stop: { kind: "sessionLost", index: 1 }, executionMs: 1, fetchMs: 0 });
  assert.deepEqual(lost[1].translation, { key: "messages.stop.sessionLost", values: { index: 2 } });
  for (const language of ["en", "es"]) {
    const texts = locale(language);
    assert.ok(texts.results.paused.includes("{{index}}"), language);
    assert.ok(texts.messages.stop.sessionLost.includes("{{index}}"), language);
    assert.notEqual(texts.messages.stop.sessionLost, texts.messages.stop.closed, language);
  }
});
