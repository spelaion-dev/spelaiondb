// The dialogs' shared states (src/dialogState.ts, ADR-060): a value being loaded, "preview, then
// apply", and a job's phase. Run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import {
  errorText,
  initialPreviewApply,
  jobReducer,
  lastPreview,
  loadedValue,
  previewApplyReducer,
  previewFor,
  scriptText,
} from "../src/dialogState.ts";

const run = (actions, state = initialPreviewApply()) => actions.reduce(previewApplyReducer, state);

test("a loaded value is there only when it is ready", () => {
  assert.equal(loadedValue({ kind: "ready", value: 3 }), 3);
  for (const other of [{ kind: "idle" }, { kind: "loading" }, { kind: "failed", message: "no" }]) {
    assert.equal(loadedValue(other), null);
  }
  assert.equal(errorText(new Error("refused")), "refused");
  assert.equal(errorText("refused"), "refused");
});

test("a preview is the form's own: an answer to an earlier form is waited past", () => {
  const answered = run([{ type: "answered", key: "a", value: { sql: "drop a" } }]);
  assert.deepEqual(previewFor(answered, "a"), { kind: "ready", value: { sql: "drop a" } });
  // The user typed on: the script shown is not this form's yet.
  assert.deepEqual(previewFor(answered, "ab"), { kind: "loading" });
  assert.equal(scriptText(previewFor(answered, "ab")), "…");
  // A dialog that keeps the last script shown while it asks reads it whatever the form.
  assert.deepEqual(lastPreview(answered), { kind: "ready", value: { sql: "drop a" } });
  // Nothing to preview, and nothing asked yet.
  assert.deepEqual(previewFor(answered, null), { kind: "idle" });
  assert.deepEqual(previewFor(initialPreviewApply(), "a"), { kind: "loading" });
});

test("a refused preview stays shown until the next answer, and clearing forgets it", () => {
  const refused = run([{ type: "refused", key: "a", message: "unknown type" }]);
  assert.deepEqual(previewFor(refused, "ab"), { kind: "failed", message: "unknown type" });
  assert.equal(scriptText(previewFor(refused, "ab")), "unknown type");
  const next = run([{ type: "answered", key: "ab", value: { sql: "alter" } }], refused);
  assert.equal(scriptText(previewFor(next, "ab")), "alter");
  const cleared = run([{ type: "cleared" }], next);
  assert.equal(cleared.answer, null);
  assert.equal(run([{ type: "cleared" }], cleared), cleared);
});

test("apply runs once at a time and ends one way", () => {
  const applying = run([{ type: "answered", key: "a", value: { sql: "x" } }, { type: "applying" }]);
  assert.deepEqual(applying.apply, { kind: "applying" });
  // The preview is not touched by applying it.
  assert.equal(scriptText(previewFor(applying, "a")), "x");
  const failed = run([{ type: "applyFailed", message: "Could not apply: denied" }], applying);
  assert.deepEqual(failed.apply, { kind: "failed", message: "Could not apply: denied" });
  // Applying again clears the failure while it runs.
  assert.deepEqual(run([{ type: "applying" }], failed).apply, { kind: "applying" });
  const applied = run([{ type: "applying" }, { type: "applied", note: "Granted." }], failed);
  assert.deepEqual(applied.apply, { kind: "applied", note: "Granted." });
  // A note is dropped once the form moves on; a failure is not.
  assert.deepEqual(run([{ type: "noteRead" }], applied).apply, { kind: "idle" });
  assert.equal(run([{ type: "noteRead" }], failed), failed);
});

test("a job runs with its id, takes only its own progress, and ends done or failed", () => {
  const step = (actions, phase = { kind: "idle" }) => actions.reduce(jobReducer, phase);
  const running = step([{ type: "started", id: 7, started: 1000, progress: { done: 0 } }]);
  assert.deepEqual(running, { kind: "running", id: 7, started: 1000, progress: { done: 0 } });
  const moved = step([{ type: "progressed", id: 7, progress: { done: 3 } }], running);
  assert.deepEqual(moved.progress, { done: 3 });
  // Progress of another run, or after the end, changes nothing.
  assert.equal(step([{ type: "progressed", id: 8, progress: { done: 9 } }], moved), moved);
  const done = step([{ type: "done", done: { text: "3 rows" } }], moved);
  assert.deepEqual(done, { kind: "done", text: "3 rows" });
  assert.equal(step([{ type: "progressed", id: 7, progress: { done: 4 } }], done), done);
  assert.deepEqual(step([{ type: "failed", failed: { text: "Cancelled", fallback: null } }], moved), {
    kind: "failed",
    text: "Cancelled",
    fallback: null,
  });
  assert.deepEqual(step([{ type: "reset" }], done), { kind: "idle" });
  // A run that cannot be cancelled has no id, and still takes its progress.
  const local = step([{ type: "started", id: null, started: 0, progress: { done: 0 } }]);
  assert.deepEqual(step([{ type: "progressed", id: null, progress: { done: 1 } }], local).progress, { done: 1 });
});
