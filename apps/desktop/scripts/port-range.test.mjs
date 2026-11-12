// The Network settings' port range form (src/portRange.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { initialPortRange, portRangeReady, portRangeReducer, typedRange } from "../src/portRange.ts";

const run = (actions, state = initialPortRange) => actions.reduce(portRangeReducer, state);

test("while the stored range is read the form is not ready; its reply fills it", () => {
  assert.equal(portRangeReady(initialPortRange), false);
  const loaded = run([{ type: "loaded", request: 0, range: { min: 20000, max: 20100 } }]);
  assert.equal(portRangeReady(loaded), true);
  assert.deepEqual([loaded.min, loaded.max], ["20000", "20100"]);
  const none = run([{ type: "loaded", request: 0, range: null }]);
  assert.deepEqual([none.min, none.max, portRangeReady(none)], ["", "", true]);
});

test("a deferred first reply never erases a value typed before it arrived", () => {
  // SDB-96: Lowest typed, the reply arrives, Highest typed.
  const state = run([
    { type: "typed", field: "min", value: "20000" },
    { type: "loaded", request: 0, range: null },
    { type: "typed", field: "max", value: "20100" },
  ]);
  assert.deepEqual([state.min, state.max], ["20000", "20100"]);
  assert.deepEqual(typedRange(state), { min: 20000, max: 20100 });
  // A stored range fills only the fields the user has not typed in.
  const mixed = run([
    { type: "typed", field: "max", value: "30100" },
    { type: "loaded", request: 0, range: { min: 20000, max: 20100 } },
  ]);
  assert.deepEqual([mixed.min, mixed.max], ["20000", "30100"]);
});

test("a failed read says so, keeps what was typed, and Retry reads again", () => {
  const failed = run([
    { type: "typed", field: "min", value: "20000" },
    { type: "loadFailed", request: 0, message: "user.toml: invalid" },
  ]);
  assert.deepEqual(failed.load, { kind: "failed", message: "user.toml: invalid" });
  assert.equal(portRangeReady(failed), true);
  assert.equal(failed.min, "20000");
  const retried = run([{ type: "reload" }], failed);
  assert.equal(portRangeReady(retried), false);
  assert.equal(retried.request, 1);
  const loaded = run([{ type: "loaded", request: 1, range: { min: 1, max: 2 } }], retried);
  assert.deepEqual([loaded.min, loaded.max, loaded.load.kind], ["20000", "2", "ready"]);
});

test("only the latest read's reply counts", () => {
  const retried = run([{ type: "loadFailed", request: 0, message: "busy" }, { type: "reload" }]);
  // The first request answers late, after Retry asked again: ignored either way.
  const late = run([{ type: "loaded", request: 0, range: { min: 1, max: 2 } }], retried);
  assert.equal(late, retried);
  assert.equal(run([{ type: "loadFailed", request: 0, message: "old" }], retried), retried);
  const current = run([{ type: "loaded", request: 1, range: null }], retried);
  assert.equal(current.load.kind, "ready");
});

test("saving shows what is stored; a failed save keeps the typed values", () => {
  const typed = run([
    { type: "loaded", request: 0, range: null },
    { type: "typed", field: "min", value: "20100" },
    { type: "typed", field: "max", value: "20000" },
  ]);
  const refused = run([{ type: "saveFailed", message: "the lowest port must not be above the highest" }], typed);
  assert.deepEqual([refused.min, refused.max], ["20100", "20000"]);
  assert.deepEqual(refused.status, { kind: "error", message: "the lowest port must not be above the highest" });
  // Typing again clears the error.
  assert.deepEqual(run([{ type: "typed", field: "min", value: "19000" }], refused).status, { kind: "idle" });
  const saved = run([{ type: "saved", range: { min: 20000, max: 20100 } }], typed);
  assert.deepEqual([saved.min, saved.max, saved.status.kind], ["20000", "20100", "saved"]);
  assert.deepEqual(saved.edited, { min: false, max: false });
  const cleared = run([{ type: "saved", range: null }], saved);
  assert.deepEqual([cleared.min, cleared.max], ["", ""]);
  assert.equal(typedRange(cleared), null);
});
