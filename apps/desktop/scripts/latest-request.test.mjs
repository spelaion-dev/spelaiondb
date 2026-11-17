// Two quick header clicks on a table data tab (SDB-409): each asks the core for its ORDER BY,
// and the answers may come back in either order. Only the last click's answer may change the
// view. `node --test scripts/latest-request.test.mjs`.

import assert from "node:assert/strict";
import { test } from "node:test";
import { latestOnly } from "../src/latestRequest.ts";

/** A request whose answer the test gives when it chooses. */
function held() {
  let answer, fail;
  const promise = new Promise((resolve, reject) => { answer = resolve; fail = reject; });
  return { request: () => promise, answer, fail };
}

/** The header's sort as `sortTable` applies it: what each click's answer does to the view. */
function table() {
  const view = { orderBy: "", applied: [] };
  const latest = latestOnly();
  const click = async (request) => {
    const answered = await latest(request);
    if (!answered.latest) return;
    view.orderBy = answered.value;
    view.applied.push(answered.value);
  };
  return { view, click };
}

test("two clicks before the first answer: the second click's sort wins when its answer comes first", async () => {
  const { view, click } = table();
  const ascending = held(), descending = held();
  const first = click(ascending.request);
  const second = click(descending.request);
  // The newer answer arrives first, the older one after it: applying every answer as it
  // arrives would leave the table ascending.
  descending.answer("id DESC");
  await second;
  ascending.answer("id");
  await first;
  assert.equal(view.orderBy, "id DESC");
  assert.deepEqual(view.applied, ["id DESC"]);
});

test("and when the answers come in the order of the clicks, the older one is not applied in between", async () => {
  const { view, click } = table();
  const ascending = held(), descending = held();
  const first = click(ascending.request);
  const second = click(descending.request);
  ascending.answer("id");
  await first;
  // A page for a sort already replaced would be loaded for nothing, and shown for a moment.
  assert.deepEqual(view.applied, []);
  descending.answer("id DESC");
  await second;
  assert.deepEqual(view.applied, ["id DESC"]);
});

test("three clicks: only the third is applied, whatever the order of the answers", async () => {
  for (const order of [[0, 1, 2], [2, 1, 0], [1, 2, 0], [2, 0, 1], [0, 2, 1], [1, 0, 2]]) {
    const { view, click } = table();
    const requests = [held(), held(), held()];
    const clicks = requests.map((request) => click(request.request));
    const texts = ["id", "id DESC", ""];
    for (const index of order) requests[index].answer(texts[index]);
    await Promise.all(clicks);
    assert.deepEqual(view.applied, [""], `answers in the order ${order}`);
  }
});

test("an older click that fails after a newer one says nothing; the newest one's failure is reported", async () => {
  const latest = latestOnly();
  const older = held(), newer = held();
  const first = latest(older.request);
  const second = latest(newer.request);
  older.fail(new Error("no such column"));
  assert.deepEqual(await first, { latest: false });
  newer.fail(new Error("the column is masked"));
  await assert.rejects(second, /the column is masked/);
  // Alone, a request is the newest: answered, or its error thrown.
  assert.deepEqual(await latest(async () => "id"), { latest: true, value: "id" });
  await assert.rejects(latest(async () => { throw new Error("refused"); }), /refused/);
});
