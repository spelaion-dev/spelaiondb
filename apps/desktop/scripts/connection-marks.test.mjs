// Which of a connection's marks fit after its name in the explorer (src/connectionMarks.ts), run
// by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { fitMarks } from "../src/connectionMarks.ts";

// The demo's "Postgres behind bastions" with Noto Sans at the default panel width: 55 px left.
const dot = { width: 6, gap: 4, required: true };
const production = { width: 64, gap: 5 };
const ssh = { width: 28, gap: 4 };

test("everything is shown when there is room", () => {
  assert.deepEqual(fitMarks(200, [dot, production, ssh]), [true, true, true]);
});

test("a type's name that does not fit does not hide the SSH mark after it", () => {
  assert.deepEqual(fitMarks(55, [dot, production, ssh]), [true, false, true]);
});

test("the type's dot stays even with no room at all", () => {
  assert.deepEqual(fitMarks(0, [dot, production, ssh]), [true, false, false]);
  assert.deepEqual(fitMarks(6, [dot, production, ssh]), [true, false, false]);
});

test("marks are taken in order: an earlier one that fits wins over a later one", () => {
  // 6 + 5 + 64 = 75: the type's name fits, and then there is no room for SSH.
  assert.deepEqual(fitMarks(80, [dot, production, ssh]), [true, true, false]);
});

test("a mark that fits exactly is shown, and the gap only counts after a shown mark", () => {
  assert.deepEqual(fitMarks(28, [ssh, { width: 20, gap: 4 }]), [true, false]);
  assert.deepEqual(fitMarks(52, [ssh, { width: 20, gap: 4 }]), [true, true]);
  assert.deepEqual(fitMarks(20, [{ width: 30, gap: 4 }, { width: 20, gap: 4 }]), [false, true]);
});

test("without a type, nothing is required", () => {
  assert.deepEqual(fitMarks(0, [ssh]), [false]);
});
