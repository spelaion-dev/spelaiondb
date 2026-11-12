// The app's questions (src/dialogStore.ts, SDB-139), run by `npm run build` with `node --test`:
// a second question of a kind asked while the first is open waits for its turn, instead of
// replacing the first one, whose flow would then wait forever.
import assert from "node:assert/strict";
import test from "node:test";
import { useDialogs } from "../src/dialogStore.ts";

const store = () => useDialogs.getState();

/** Asks with `ask`, then `ask` again, and answers each with `answer`; checks both settle, in order. */
async function twoInARow(kind, ask, answers) {
  const settled = [];
  const first = ask(1).then((value) => settled.push(["first", value]));
  const second = ask(2).then((value) => settled.push(["second", value]));
  assert.equal(store().queued[kind].length, 2, `${kind}: both wait`);
  const shownFirst = store()[kind];
  assert.ok(shownFirst, `${kind}: one is shown`);
  shownFirst.resolve(answers[0]);
  await first;
  // The second is shown only once the first is answered: never instead of it.
  const shownSecond = store()[kind];
  assert.ok(shownSecond && shownSecond !== shownFirst, `${kind}: the second is shown next`);
  shownSecond.resolve(answers[1]);
  await second;
  assert.deepEqual(settled, [["first", answers[0]], ["second", answers[1]]], kind);
  assert.equal(store()[kind], null, `${kind}: nothing left`);
  assert.equal(store().queued[kind].length, 0, `${kind}: nothing queued`);
}

test("two questions of every kind are asked one after the other, and both settle in order", async () => {
  await twoInARow("password", (n) => store().askPassword(`db ${n}`, true, null), [{ password: "a", remember: false }, null]);
  await twoInARow("trust", (n) => store().askTrust(`/repo/${n}`), [true, false]);
  await twoInARow("change", (n) => store().askChange({ item: `c${n}`, isNew: false, fields: [] }), [false, true]);
  await twoInARow("variables", (n) => store().askVariables([`v${n}`]), [{ v1: "x" }, null]);
  await twoInARow("risk", (n) => store().askRisks(`db ${n}`, [{ sql: "drop table t", risk: "drop" }]), [true, false]);
  await twoInARow("scriptError", (n) => store().askScriptError({ tab: `tab${n}`, statement: n, message: "boom", remaining: 1 }), ["skip", "stop"]);
  await twoInARow("openTransactions", (n) => store().askOpenTransactions([`tab${n}`], "close"), [true, false]);
  await twoInARow("params", (n) => store().askParams([`p${n}`], {}), [{ p1: "1" }, null]);
  await twoInARow("text", (n) => store().askText({ title: `t${n}`, label: "Name", initial: "", confirm: "Save" }), ["a", null]);
  await twoInARow("choice", (n) => store().askChoice({ title: `c${n}`, message: "?", choices: [{ id: "yes", label: "Yes" }] }), ["yes", null]);
});

test("the request a dialog shows keeps its own fields: two connections each name theirs", async () => {
  const answers = [store().askPassword("orders db", false, null), store().askPassword("billing db", true, "wrong password")];
  assert.equal(store().password.connectionName, "orders db");
  store().password.resolve({ password: "one", remember: false });
  assert.equal(store().password.connectionName, "billing db");
  assert.equal(store().password.error, "wrong password");
  store().password.resolve({ password: "two", remember: true });
  assert.deepEqual(await Promise.all(answers), [
    { password: "one", remember: false },
    { password: "two", remember: true },
  ]);
});

test("two runs paused on an error each get their question, and a closing tab's is stopped wherever it waits", async () => {
  const first = store().askScriptError({ tab: "A", statement: 2, message: "first", remaining: 3 });
  const second = store().askScriptError({ tab: "B", statement: 1, message: "second", remaining: 1 });
  const third = store().askScriptError({ tab: "C", statement: 4, message: "third", remaining: 0 });
  assert.equal(store().scriptError.tab, "A");
  // Closing tab B answers its question although it is not the one shown.
  store().stopScriptQuestions(["B"]);
  assert.equal(await second, "stop");
  assert.equal(store().scriptError.tab, "A", "the shown question stays");
  assert.deepEqual(store().queued.scriptError.map((q) => q.tab), ["A", "C"]);
  store().scriptError.resolve("skipAll");
  assert.equal(await first, "skipAll");
  assert.equal(store().scriptError.tab, "C");
  store().scriptError.resolve("skip");
  assert.equal(await third, "skip");
  assert.equal(store().scriptError, null);
});

test("an answer given twice settles once and removes nothing else", async () => {
  const first = store().askChoice({ title: "one", message: "", choices: [] });
  const second = store().askChoice({ title: "two", message: "", choices: [] });
  const shown = store().choice;
  shown.resolve("a");
  shown.resolve("b");
  assert.equal(await first, "a");
  assert.equal(store().choice.title, "two", "the second question is still there");
  store().choice.resolve(null);
  assert.equal(await second, null);
});
