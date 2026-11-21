// A tab's connection as one state and the selectors every component reads it through
// (src/tabState.ts, ADR-060, SDB-371), run by `npm run build` with `node --test`. The last tests
// read the sources: a component that reads a tab's raw connection, run or edits fields, or asks
// "is a question open?" with its own partial expression, fails the build.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { relative, resolve } from "node:path";
import test from "node:test";
import {
  CONNECTING,
  DISCONNECTED,
  canStartRun,
  cancelling,
  connectFailed,
  connectFailure,
  connectedWith,
  connectionBusy,
  connectionInUse,
  connectionOpen,
  explorerSessionOpen,
  hasPendingEdits,
  hasSession,
  hasUnsavedEdits,
  inTransaction,
  isBusy,
  isConnected,
  isConnecting,
  isRunning,
  lostSession,
  questionOpen,
  savingEdits,
  serverVersion,
  sessionLost,
  sessionReadOnly,
  statusName,
  transactionOf,
  withReadOnly,
  withTransaction,
} from "../src/tabState.ts";

const src = resolve(import.meta.dirname, "../src");

/** A tab with only what the selectors read. */
function tab(fields = {}) {
  return { id: "tab-1", connectionId: "pg", connectionState: DISCONNECTED, run: null, ...fields };
}

const running = { running: true, results: [], activeResult: null, messages: [], error: null, startedAt: 1, elapsedMs: null };
const followed = (open) => ({ kind: "followed", commit: "auto", transaction: { kind: open ? "open" : "none" }, smartCommit: false, inTransaction: open, autoCommitLocked: false, endActionsShown: open });

test("each connection state shows as one of three words; lost and idle-closed as disconnected", () => {
  assert.equal(statusName(tab()), "disconnected");
  assert.equal(statusName(tab({ connectionState: connectFailed("refused") })), "disconnected");
  assert.equal(statusName(tab({ connectionState: CONNECTING })), "connecting");
  assert.equal(statusName(tab({ connectionState: connectedWith({ version: "16.4" }) })), "connected");
  assert.equal(statusName(tab({ connectionState: sessionLost("reset", null, false) })), "disconnected");
  assert.equal(statusName(tab({ connectionState: sessionLost("idle", 600, false) })), "disconnected");
  assert.throws(() => statusName(tab({ connectionState: { kind: "sleeping" } })), /unknown tab connection/);
});

test("a lost session carries its reason; closed for being idle, its time and no transaction", () => {
  const lost = tab({ connectionState: sessionLost("connection reset", null, true) });
  assert.deepEqual(lostSession(lost), { reason: "connection reset", idleSeconds: null, transactionOpen: true });
  const idle = tab({ connectionState: sessionLost("closed after 600 s", 600, false) });
  assert.deepEqual(lostSession(idle), { reason: "closed after 600 s", idleSeconds: 600, transactionOpen: false });
  for (const state of [DISCONNECTED, CONNECTING, connectedWith({ version: "16" }), connectFailed("x")]) {
    assert.equal(lostSession(tab({ connectionState: state })), null);
  }
  // An event that leaves the optional fields out is a plain loss.
  assert.deepEqual(lostSession(tab({ connectionState: sessionLost("gone", undefined, undefined) })), { reason: "gone", idleSeconds: null, transactionOpen: false });
  // Neither has a session: nothing runs on it until the user reconnects (ADR-028).
  assert.equal(hasSession(lost), false);
  assert.equal(hasSession(idle), false);
});

test("a failure to connect is said only while the tab stays disconnected after it", () => {
  assert.equal(connectFailure(tab({ connectionState: connectFailed("password authentication failed") })), "password authentication failed");
  assert.equal(connectFailure(tab()), null);
  assert.equal(connectFailure(tab({ connectionState: CONNECTING })), null);
});

test("the version, the transaction and read-only for this session exist only while connected", () => {
  const connected = tab({ connectionState: connectedWith({ version: "8.0.36", transaction: followed(true) }) });
  assert.equal(serverVersion(connected), "8.0.36");
  assert.equal(inTransaction(connected), true);
  assert.equal(transactionOf(connected)?.kind, "followed");
  for (const state of [DISCONNECTED, CONNECTING, sessionLost("reset", null, true)]) {
    const other = tab({ connectionState: state });
    assert.equal(serverVersion(other), null);
    assert.equal(transactionOf(other), undefined);
    assert.equal(inTransaction(other), false);
    assert.equal(sessionReadOnly(other), false);
  }
});

test("the core's word on a transaction or read-only reaches only a connected tab", () => {
  const connected = connectedWith({ version: "16" });
  const changed = withTransaction(connected, followed(true));
  assert.equal(changed.kind === "connected" && changed.transaction?.kind, "followed");
  // A late event for a tab whose session is gone changes nothing, not even the object.
  const lost = sessionLost("reset", null, false);
  assert.equal(withTransaction(lost, followed(true)), lost);
  assert.equal(withTransaction(CONNECTING, followed(true)), CONNECTING);
  const readOnly = withReadOnly(connected, true);
  assert.equal(sessionReadOnly(tab({ connectionState: readOnly })), true);
  assert.equal(sessionReadOnly(tab({ connectionState: withReadOnly(readOnly, false) })), false);
  assert.equal(withReadOnly(DISCONNECTED, true), DISCONNECTED);
  // A new session starts as its connection says: the mark goes with the old one.
  assert.equal(sessionReadOnly(tab({ connectionState: connectedWith({ version: "16" }) })), false);
});

test("busy is running or connecting; Run can start on a disconnected tab, as its button does", () => {
  const idle = tab();
  assert.equal(isBusy(idle), false);
  assert.equal(canStartRun(idle), true);
  assert.equal(canStartRun(tab({ connectionState: connectedWith({ version: "16" }) })), true);
  assert.equal(canStartRun(tab({ connectionState: connectFailed("x") })), true);
  // A lost tab can start one: Run says the session was lost, and does not reconnect.
  assert.equal(canStartRun(tab({ connectionState: sessionLost("reset", null, false) })), true);
  const connecting = tab({ connectionState: CONNECTING });
  assert.equal(isConnecting(connecting), true);
  assert.equal(isBusy(connecting), true);
  assert.equal(canStartRun(connecting), false);
  const runningTab = tab({ connectionState: connectedWith({ version: "16" }), run: running });
  assert.equal(isRunning(runningTab), true);
  assert.equal(isBusy(runningTab), true);
  assert.equal(canStartRun(runningTab), false);
  assert.equal(isRunning(undefined), false);
});

test("Cancel is not offered again while the core says a cancel is on its way", () => {
  const connected = connectedWith({ version: "16" });
  assert.equal(cancelling(tab({ connectionState: connected, run: running, runState: { kind: "cancelling", statement: 0 } })), true);
  assert.equal(cancelling(tab({ connectionState: connected, run: running, runState: { kind: "running", statement: 0 } })), false);
  // The run already drawn to its end: nothing to cancel.
  assert.equal(cancelling(tab({ connectionState: connected, runState: { kind: "cancelling", statement: 0 } })), false);
});

test("unsaved edits are pending ones or ones being applied", () => {
  assert.equal(hasUnsavedEdits(tab({ pendingEdits: true })), true);
  assert.equal(hasUnsavedEdits(tab({ savingEdits: true })), true);
  assert.equal(hasUnsavedEdits(tab()), false);
  assert.equal(hasPendingEdits(tab({ savingEdits: true })), false);
  assert.equal(savingEdits(tab({ savingEdits: true })), true);
});

test("over every tab: busy, in use and open answer for one connection", () => {
  const tabs = [
    tab({ id: "a", connectionId: "pg", connectionState: CONNECTING }),
    tab({ id: "b", connectionId: "my", connectionState: connectedWith({ version: "8" }) }),
    tab({ id: "c", connectionId: "lite", connectionState: sessionLost("reset", null, false) }),
    tab({ id: "d", connectionId: "ms", run: running }),
  ];
  assert.equal(connectionBusy(tabs, "pg"), true);
  assert.equal(connectionBusy(tabs, "my"), false);
  assert.equal(connectionBusy(tabs, "ms"), true);
  assert.equal(connectionInUse(tabs, "pg"), true);
  assert.equal(connectionInUse(tabs, "my"), true);
  assert.equal(connectionInUse(tabs, "lite"), false);
  assert.equal(connectionInUse(tabs, "ms"), true);
  assert.equal(connectionOpen(tabs, [], "my"), true);
  assert.equal(connectionOpen(tabs, [], "pg"), false);
  assert.equal(connectionOpen(tabs, ["pg"], "pg"), true);
  assert.equal(isConnected(tabs[1]), true);
  assert.equal(explorerSessionOpen(["pg"], "pg"), true);
  assert.equal(explorerSessionOpen(["pg"], "my"), false);
});

/** The kinds of question the dialog store queues, read from its `Requests` interface. */
function questionKinds() {
  const code = readFileSync(resolve(src, "dialogStore.ts"), "utf8");
  const body = code.match(/interface Requests \{([^}]*)\}/);
  assert.ok(body, "dialogStore.ts's Requests");
  return [...body[1].matchAll(/^\s*(\w+):/gm)].map((m) => m[1]);
}

test("a question is open for every kind the dialog store queues, and for the core's", () => {
  const kinds = questionKinds();
  assert.ok(kinds.length >= 11, `${kinds.length} kinds`);
  const none = Object.fromEntries([...kinds.map((kind) => [kind, null]), ["interactions", []]]);
  assert.equal(questionOpen(none), false);
  for (const kind of kinds) assert.equal(questionOpen({ ...none, [kind]: {} }), true, kind);
  assert.equal(questionOpen({ ...none, interactions: [{ id: 1 }] }), true);
});

/** Every source file of the page, but the module that owns the fields and the store. */
function sources() {
  const files = [];
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = resolve(dir, entry.name);
      if (entry.isDirectory()) walk(path);
      else if (/\.(ts|tsx)$/.test(entry.name) && entry.name !== "bindings.ts") files.push(path);
    }
  };
  walk(src);
  return files.map((path) => ({ path: relative(src, path).replaceAll("\\", "/"), code: readFileSync(path, "utf8") }));
}

/** What only tabState.ts reads, and what the store may also touch to keep it. */
const RAW = [
  { pattern: /\.connectionState\b/, what: "a tab's connection (use tabState.ts's selectors)" },
  { pattern: /connectionState:\s*\{/, what: "a connection state written by hand (use tabState.ts's transitions)" },
  { pattern: /\.runState\b/, what: "a tab's run state (use runStateOf / cancelling)" },
  { pattern: /\brun\??\.running\b/, what: "a tab's run.running (use isRunning)" },
  { pattern: /(?<!commands)\.savingEdits\b/, what: "a tab's savingEdits (use savingEdits / hasUnsavedEdits)" },
  { pattern: /(?<!commands)\.pendingEdits\b/, what: "a tab's pendingEdits (use hasPendingEdits / hasUnsavedEdits)" },
  { pattern: /!dialogs\.(trust|password)\b|\bdialogs\.(trust|password)\s*(\|\||&&)/, what: "a partial \"a question is open\" (use questionOpen)" },
];
const OWNERS = new Set(["tabState.ts", "store.ts"]);

test("no component reads a tab's raw connection, run or edits fields, or asks for questions its own way", () => {
  const found = [];
  for (const { path, code } of sources()) {
    if (OWNERS.has(path)) continue;
    code.split("\n").forEach((line, index) => {
      for (const { pattern, what } of RAW) {
        if (pattern.test(line)) found.push(`${path}:${index + 1}: ${what}`);
      }
    });
  }
  assert.deepEqual(found, []);
});

test("the check sees a raw read", () => {
  const line = "if (tab.connectionState.kind === \"connected\" && !tab.run?.running) {}";
  assert.ok(RAW[0].pattern.test(line));
  assert.ok(RAW[3].pattern.test(line));
  assert.ok(!RAW[5].pattern.test("call(commands.pendingEdits(result.id))"));
  assert.ok(RAW[6].pattern.test("if (!dialogs.trust && !dialogs.password) close();"));
});
