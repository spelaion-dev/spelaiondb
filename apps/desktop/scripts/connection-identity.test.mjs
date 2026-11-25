// A connection's identity in the page (src/connectionIdentity.ts, ADR-075, SDB-495): two
// folders' connections of the same id are two connections in every key the page builds. Run
// by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import {
  connectionOfTag,
  connectionTag,
  holdsConnection,
  keyOf,
  listedConnection,
  sameConnection,
  tabOnConnection,
  withConnection,
} from "../src/connectionIdentity.ts";

const shop = { folder: "/work/shop", id: "prod/orders" };
const crm = { folder: "/work/crm", id: "prod/orders" };
const personal = { folder: null, id: "~/local" };

test("two folders' connections of the same id are different connections", () => {
  assert.equal(sameConnection(shop, crm), false);
  assert.equal(sameConnection(shop, { ...shop }), true);
  assert.notEqual(connectionTag(shop), connectionTag(crm));
  assert.equal(holdsConnection([shop], crm), false);
});

test("a personal connection has no folder, however the folder is left out", () => {
  assert.equal(sameConnection(personal, { id: "~/local" }), true);
  assert.equal(sameConnection(personal, { folder: undefined, id: "~/local" }), true);
  assert.equal(connectionTag(personal), connectionTag({ folder: undefined, id: "~/local" }));
  assert.equal(sameConnection(personal, { folder: "/work/shop", id: "~/local" }), false);
});

test("a tag reads back as the pair it was written from, and nothing else reads as one", () => {
  for (const key of [shop, crm, personal, { folder: "a\"b", id: "c\u0000d" }]) {
    assert.deepEqual(connectionOfTag(connectionTag(key)), key);
  }
  for (const text of ["prod/orders", "", "[1,2]", "[null]", "[null,\"a\",\"b\"]", "{}", "[\"a\",3]"]) {
    assert.equal(connectionOfTag(text), null, text);
  }
});

test("no folder and id give the tag of another pair", () => {
  // A separator in either half cannot move text from one half to the other.
  assert.notEqual(connectionTag({ folder: "a", id: "b/c" }), connectionTag({ folder: "a/b", id: "c" }));
  assert.notEqual(connectionTag({ folder: "a\",\"b", id: "c" }), connectionTag({ folder: "a", id: "b\",\"c" }));
});

test("a set of identities adds a connection once and takes out only that one", () => {
  const both = withConnection(withConnection([], shop, true), crm, true);
  assert.equal(both.length, 2);
  assert.equal(withConnection(both, { ...shop }, true), both, "already there: unchanged");
  assert.deepEqual(withConnection(both, shop, false), [crm]);
});

test("the listed connection is found by its folder and its id", () => {
  const connections = [
    { ...shop, name: "Shop orders" },
    { ...crm, name: "CRM orders" },
  ];
  assert.equal(listedConnection(connections, crm)?.name, "CRM orders");
  assert.equal(listedConnection(connections, { folder: "/work/other", id: "prod/orders" }), undefined);
  assert.deepEqual(keyOf(connections[0]), shop);
});

test("a tab names its connection by its id while one folder is open", () => {
  assert.equal(tabOnConnection("prod/orders", shop), true);
  assert.equal(tabOnConnection(null, shop), false);
  assert.equal(tabOnConnection("prod/customers", shop), false);
});
