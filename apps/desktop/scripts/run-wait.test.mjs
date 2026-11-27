// A Run pressed while an editor tab cannot start one (src/runWait.ts, SDB-525), run by
// `npm run build` with `node --test`: it waits for what ends on its own and runs once, is said
// when the user is changing the folder, is dropped with a word when the connection fails, and
// never vanishes.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { NOT_WAITING, notRunKey, press, runBlock, settle, waitingKey } from "../src/runWait.ts";

const none = { folder: false, connecting: false, databases: false, history: false, restoring: false };
const block = (cause) => runBlock({ ...none, ...(cause ? { [cause]: true } : {}) });
const pressed = { text: "select 1", from: 0 };

test("the user's own doing comes first, then what ends on its own", () => {
  assert.deepEqual(runBlock(none), { kind: "free" });
  assert.deepEqual(runBlock({ ...none, connecting: true, folder: true }), { kind: "folder" });
  assert.deepEqual(runBlock({ ...none, databases: true, history: true }), { kind: "databases" });
  for (const cause of ["connecting", "databases", "history", "restoring"]) assert.deepEqual(block(cause), { kind: cause });
});

test("a run pressed during the database listing waits, then runs once it is loaded", () => {
  const first = press(block("databases"), NOT_WAITING, pressed, "connected");
  assert.equal(first.kind, "wait");
  let waiting = first.waiting;
  assert.equal(waiting.waitFor, "databases");
  // Pressed again meanwhile: still one run, still said.
  assert.deepEqual(press(block("databases"), waiting, { text: "select 2", from: 0 }, "connected"), { kind: "alreadyWaiting" });
  // Still listing: it keeps waiting.
  const still = settle(waiting, block("databases"), "connected");
  assert.equal(still.kind, "keep");
  waiting = still.waiting;
  // Loaded: it runs, the text as it was when pressed, and nothing waits any more.
  assert.deepEqual(settle(waiting, block(null), "connected"), { kind: "start", run: pressed });
});

test("pressed right after connecting, it follows the session through the database list", () => {
  const first = press(block("connecting"), NOT_WAITING, pressed, "connecting");
  assert.equal(first.kind, "wait");
  const listing = settle(first.waiting, block("databases"), "connected");
  assert.equal(listing.kind, "keep");
  assert.equal(listing.waiting.waitFor, "databases", "the notice says what it waits for now");
  assert.deepEqual(settle(listing.waiting, block(null), "connected"), { kind: "start", run: pressed });
});

test("a connection that fails drops it, with a word; nothing runs in another session", () => {
  const first = press(block("connecting"), NOT_WAITING, pressed, "connecting");
  assert.deepEqual(settle(first.waiting, block(null), "none"), { kind: "drop", because: "connectionFailed" });
  // Lost while the databases were listed: dropped too, never run after a reconnect (ADR-028).
  const listing = press(block("databases"), NOT_WAITING, pressed, "connected");
  assert.deepEqual(settle(listing.waiting, block("databases"), "none"), { kind: "drop", because: "connectionFailed" });
});

test("the folder changing is said, never waited for, and drops a run that waited", () => {
  assert.deepEqual(press(block("folder"), NOT_WAITING, pressed, "connected"), { kind: "refused", because: "folder" });
  const waiting = press(block("history"), NOT_WAITING, pressed, "connected").waiting;
  assert.deepEqual(settle(waiting, block("folder"), "connected"), { kind: "drop", because: "folder" });
});

test("waiting for the history with no session open, it runs once saved, connecting as any Run does", () => {
  const waiting = press(block("history"), NOT_WAITING, pressed, "none").waiting;
  assert.equal(waiting.opened, false);
  assert.deepEqual(settle(waiting, block("history"), "none"), { kind: "keep", waiting });
  assert.deepEqual(settle(waiting, block(null), "none"), { kind: "start", run: pressed });
  // A session opened meanwhile is the one it belongs to: closed again, the run is dropped.
  const opening = settle(waiting, block("connecting"), "connecting");
  assert.equal(opening.waiting.opened, true);
  assert.deepEqual(settle(opening.waiting, block(null), "none"), { kind: "drop", because: "connectionFailed" });
});

test("free, it starts at once, and nothing waiting stays nothing", () => {
  assert.deepEqual(press(block(null), NOT_WAITING, pressed, "connected"), { kind: "start" });
  assert.deepEqual(settle(NOT_WAITING, block(null), "connected"), { kind: "keep", waiting: NOT_WAITING });
  assert.deepEqual(settle(NOT_WAITING, block(null), "none"), { kind: "keep", waiting: NOT_WAITING });
});

test("every notice it can show is written in both languages", () => {
  const keys = [
    ...["connecting", "databases", "history", "restoring"].map(waitingKey),
    notRunKey("connectionFailed", true),
    notRunKey("folder", false),
    notRunKey("folder", true),
    "runWait.dontRun",
  ];
  for (const language of ["en", "es"]) {
    const texts = JSON.parse(readFileSync(new URL(`../src/locales/${language}/editor.json`, import.meta.url), "utf8"));
    for (const key of keys) {
      const text = key.split(".").reduce((at, part) => at?.[part], texts);
      assert.equal(typeof text, "string", `${language}: ${key}`);
      assert.ok(text.length > 0, `${language}: ${key}`);
    }
  }
  assert.throws(() => waitingKey("sleeping"), /unknown run block/);
});
