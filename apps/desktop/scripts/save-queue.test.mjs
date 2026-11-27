// Preferences saved one after another (src/saveQueue.ts, SDB-83), run by `npm run build` with
// `node --test`: a change made while another is saving waits for it and is saved, never
// dropped; a toggle clicked twice quickly ends in the second state; and a failed save goes back
// to the last saved value only when no newer change is on its way.
import assert from "node:assert/strict";
import test from "node:test";
import { createSaveQueue, savedSetting } from "../src/saveQueue.ts";

/** A save the test finishes when it wants: `resolve` or `reject`. */
function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** A toggle shown at once and saved through the queue, as preferences.ts does, against a
 *  pretend core whose saves the test finishes one by one. */
function toggleRig(initial) {
  const busy = [];
  const run = createSaveQueue((setting) => busy.push(setting));
  const setting = savedSetting(initial);
  const state = { shown: initial, error: null };
  const core = { value: initial, calls: [] };
  const set = (next) => {
    const ticket = setting.change();
    state.shown = next;
    return run("explorer", async () => {
      const save = deferred();
      core.calls.push({ value: next, save });
      try {
        await save.promise;
        core.value = next;
        setting.succeeded(next);
      } catch (e) {
        const back = setting.failed(ticket);
        if (back !== undefined) state.shown = back;
        state.error = String(e);
      }
    });
  };
  // A click toggles from what the store shows now.
  const click = () => set(!state.shown);
  // Lets the queue start the next save.
  const settle = () => new Promise((resolve) => setTimeout(resolve, 0));
  return { busy, setting, state, core, click, settle };
}

test("a toggle clicked twice while the first save runs ends in the second state", async () => {
  const rig = toggleRig(false);
  const first = rig.click();
  const second = rig.click();
  // Both clicks show at once; only the first save has started.
  assert.equal(rig.state.shown, false);
  await rig.settle();
  assert.equal(rig.core.calls.length, 1);
  assert.equal(rig.core.calls[0].value, true);
  rig.core.calls[0].save.resolve();
  await first;
  await rig.settle();
  // The second waited for the first and is saved too, never dropped.
  assert.equal(rig.core.calls.length, 2);
  assert.equal(rig.core.calls[1].value, false);
  rig.core.calls[1].save.resolve();
  await second;
  assert.equal(rig.core.value, false);
  assert.equal(rig.state.shown, false);
  assert.equal(rig.setting.saved, false);
  // Busy with that setting from the first save to the last, then idle once.
  assert.deepEqual(rig.busy, ["explorer", "explorer", null]);
});

test("three quick clicks are saved in order and end in the third state", async () => {
  const rig = toggleRig(false);
  const done = [rig.click(), rig.click(), rig.click()];
  assert.equal(rig.state.shown, true);
  for (let i = 0; i < 3; i++) {
    await rig.settle();
    assert.equal(rig.core.calls.length, i + 1, "one save at a time");
    rig.core.calls[i].save.resolve();
    await done[i];
  }
  assert.deepEqual(rig.core.calls.map((c) => c.value), [true, false, true]);
  assert.equal(rig.core.value, true);
  assert.equal(rig.state.shown, true);
});

test("a failed save goes back to the saved value, unless a newer change is on its way", async () => {
  const rig = toggleRig(false);
  // Alone: the failure shows the saved value again.
  const lone = rig.click();
  await rig.settle();
  rig.core.calls[0].save.reject(new Error("disk full"));
  await lone;
  assert.equal(rig.state.shown, false);
  assert.match(rig.state.error, /disk full/);
  // With a newer change queued, the failure keeps what that change shows; it is saved next.
  const failing = rig.click();
  const newer = rig.click();
  const newest = rig.click();
  assert.equal(rig.state.shown, true);
  await rig.settle();
  rig.core.calls[1].save.reject(new Error("busy"));
  await failing;
  assert.equal(rig.state.shown, true, "the newer change stays shown");
  await rig.settle();
  rig.core.calls[2].save.resolve();
  await newer;
  await rig.settle();
  // The newest fails too: back to what the core saved last (the second change).
  rig.core.calls[3].save.reject(new Error("busy"));
  await newest;
  assert.equal(rig.core.value, false);
  assert.equal(rig.setting.saved, false);
  assert.equal(rig.state.shown, false);
});

test("saves of different settings share the queue and a failure does not stop it", async () => {
  const busy = [];
  const run = createSaveQueue((setting) => busy.push(setting));
  const order = [];
  const a = run("theme", async () => {
    order.push("theme");
    throw new Error("no");
  });
  const b = run("grid", async () => {
    order.push("grid");
    return 7;
  });
  await assert.rejects(a, /no/);
  assert.equal(await b, 7);
  assert.deepEqual(order, ["theme", "grid"]);
  assert.deepEqual(busy, ["theme", "grid", null]);
  // Idle again: the next save starts at once.
  assert.equal(await run("map", async () => "ok"), "ok");
  assert.deepEqual(busy, ["theme", "grid", null, "map", null]);
});

// SDB-533: a value saved by another window replaces what this window shows, unless a change of
// its own is on its way (that change is saved after it and shows); either way a failed change
// goes back to it, never to the value this window had before.
test("a value saved in another window shows here unless a change of this window is on its way", async () => {
  const rig = toggleRig(true);
  // Nothing pending here: the other window's value shows at once.
  assert.equal(rig.setting.changedElsewhere(false), false);
  rig.state.shown = false;
  assert.equal(rig.setting.saved, false);
  // A change of this window waits for its save: the other window's value does not replace it.
  const mine = rig.click();
  assert.equal(rig.state.shown, true);
  assert.equal(rig.setting.changedElsewhere(false), undefined);
  await rig.settle();
  rig.core.calls[0].save.reject(new Error("refused"));
  await mine;
  // Its save failed: back to the value the other window saved.
  assert.equal(rig.state.shown, false);
  // Settled again: the next value from elsewhere shows.
  assert.equal(rig.setting.changedElsewhere(true), true);
});

// SDB-533: a change that was not shown at once (a form's fields, saved once the core accepts
// them) and is refused still ends: values saved in another window show here afterwards.
test("a refused change that was not shown still lets values from another window show", () => {
  const setting = savedSetting("saved");
  const ticket = setting.change();
  assert.equal(setting.changedElsewhere("elsewhere"), undefined, "pending while it saves");
  // Refused: nothing to show again (it was never shown), but it is no longer pending.
  setting.failed(ticket);
  assert.equal(setting.changedElsewhere("later"), "later");
  assert.equal(setting.saved, "later");
});

// SDB-535: several values saved elsewhere while a change of this window is on its way: that
// change keeps showing, and if it fails it goes back to the newest of them.
test("values saved elsewhere while a change is pending: a failure goes back to the newest", async () => {
  const rig = toggleRig(false);
  const mine = rig.click();
  assert.equal(rig.setting.changedElsewhere(true), undefined);
  assert.equal(rig.setting.changedElsewhere(false), undefined);
  assert.equal(rig.state.shown, true, "this window's change still shows");
  await rig.settle();
  rig.core.calls[0].save.reject(new Error("refused"));
  await mine;
  assert.equal(rig.state.shown, false, "back to the newest value saved elsewhere");
  assert.equal(rig.setting.saved, false);
});
