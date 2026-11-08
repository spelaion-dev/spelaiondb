// The explorer's connection search (src/connectionSearch.ts), run by `npm run build` with
// `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { connectionMatch, editDistance } from "../src/connectionSearch.ts";

const orders = {
  name: "Orders",
  description: "The shop's orders, refunds and invoices. Ask the payments team before writing.",
  tags: ["billing", "eu"],
  host: "orders-db.internal.example.com",
  typeName: "Production",
};
const analytics = {
  name: "Analytics warehouse",
  description: null,
  tags: ["reporting"],
  host: null,
  typeName: "Test",
};
const local = { name: "local", tags: [] };
const all = [orders, analytics, local];
const found = (query) => all.filter((c) => connectionMatch(query, c) !== null).map((c) => c.name);

test("an empty query finds every connection", () => {
  assert.deepEqual(found(""), ["Orders", "Analytics warehouse", "local"]);
  assert.deepEqual(found("   "), ["Orders", "Analytics warehouse", "local"]);
});

test("the name, with letters in order, ignoring case", () => {
  assert.deepEqual(found("ord"), ["Orders"]);
  assert.deepEqual(found("ANWH"), ["Analytics warehouse"]);
  assert.deepEqual(found("LOCAL"), ["local"]);
});

test("a typo in the name still finds it", () => {
  assert.deepEqual(found("ordres"), ["Orders"]);
  assert.deepEqual(found("oredrs"), ["Orders"]);
  assert.deepEqual(found("analitics"), ["Analytics warehouse"]);
  assert.deepEqual(found("warehuose"), ["Analytics warehouse"]);
  // A word still being typed, with a typo.
  assert.deepEqual(found("analyt1"), ["Analytics warehouse"]);
  // Short words must be exact: three letters with a typo would match almost anything.
  assert.deepEqual(found("lcl"), ["local"]);
  assert.deepEqual(found("xoc"), []);
});

test("a tag, the type, the host or the description", () => {
  assert.deepEqual(found("billing"), ["Orders"]);
  assert.deepEqual(found("biling"), ["Orders"]);
  assert.deepEqual(found("reporting"), ["Analytics warehouse"]);
  assert.deepEqual(found("production"), ["Orders"]);
  assert.deepEqual(found("test"), ["Analytics warehouse"]);
  assert.deepEqual(found("internal.example"), ["Orders"]);
  assert.deepEqual(found("refunds"), ["Orders"]);
  assert.deepEqual(found("invioces"), ["Orders"]);
});

test("every word must match, in any order and any field", () => {
  assert.deepEqual(found("eu orders"), ["Orders"]);
  assert.deepEqual(found("production billing"), ["Orders"]);
  assert.deepEqual(found("orders reporting"), []);
});

test("letters in order must start at a word of the field", () => {
  const docker = { name: "Postgres (Docker)", tags: [] };
  assert.equal(connectionMatch("ord", docker), null);
  assert.ok(connectionMatch("pdo", docker) !== null, "from the start of the name");
  assert.ok(connectionMatch("dock", docker) !== null, "from the start of a word");
  assert.ok(connectionMatch("ord", { name: "myOrders", tags: [] }) !== null, "a capital starts a word");
});

test("letters spread over the description do not match", () => {
  // s, p and t appear in that order in the description, but "spt" is not a word there.
  assert.equal(connectionMatch("spt", { name: "x", description: orders.description, tags: [] }), null);
  assert.equal(connectionMatch("zzzz", orders), null);
});

test("the name counts more than the description", () => {
  const byName = { name: "Invoices", tags: [] };
  const byDescription = { name: "Ledger", description: "Holds the invoices", tags: [] };
  assert.ok(connectionMatch("invoices", byName) > connectionMatch("invoices", byDescription));
});

test("edit distance counts a swap of neighbours as one edit and gives up past the limit", () => {
  assert.equal(editDistance("orders", "orders", 1), 0);
  assert.equal(editDistance("ordres", "orders", 1), 1);
  assert.equal(editDistance("ordrs", "orders", 1), 1);
  assert.equal(editDistance("orderss", "orders", 1), 1);
  assert.equal(editDistance("abcdef", "orders", 2), 3);
  assert.equal(editDistance("a", "abcdef", 1), 2);
});
