// The explorer's sort orders (src/explorerSort.ts), run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { sortByName } from "../src/explorerSort.ts";

const names = ["orders", "Customer", "t10", "b", "t2", "A", "Ñandú", "zeta"];
const sort = (preferences) => sortByName(names, (n) => n, preferences, "es");

test("without alphabetical order, the database's order is kept", () => {
  assert.deepEqual(sort({ sortAlphabetically: false, sortIgnoreCase: true }), names);
});

test("ignoring case, upper and lower case mix and numbers compare as numbers", () => {
  assert.deepEqual(sort({ sortAlphabetically: true, sortIgnoreCase: true }), [
    "A",
    "b",
    "Customer",
    "Ñandú",
    "orders",
    "t2",
    "t10",
    "zeta",
  ]);
});

test("with case, code points decide: upper case first", () => {
  assert.deepEqual(sort({ sortAlphabetically: true, sortIgnoreCase: false }), [
    "A",
    "Customer",
    "b",
    "orders",
    "t10",
    "t2",
    "zeta",
    "Ñandú",
  ]);
});

test("names that compare equal keep their order (function overloads)", () => {
  const overloads = [
    { name: "add", args: "a integer" },
    { name: "Add", args: "x" },
    { name: "add", args: "a integer, b text" },
  ];
  const sorted = sortByName(overloads, (o) => o.name, { sortAlphabetically: true, sortIgnoreCase: true }, "en");
  assert.deepEqual(
    sorted.map((o) => o.args),
    ["x", "a integer", "a integer, b text"],
  );
});

test("the input is not changed", () => {
  const input = ["b", "a"];
  sortByName(input, (n) => n, { sortAlphabetically: true, sortIgnoreCase: true }, "en");
  assert.deepEqual(input, ["b", "a"]);
});
