// How the page names open folders (src/folderLabels.ts, ADR-075 point 8, SDB-503). Run by
// `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { folderLabels } from "../src/folderLabels.ts";

test("a folder is named by its last part", () => {
  assert.deepEqual(folderLabels(["/work/service-a", "C:\\work\\service-b\\"]), ["service-a", "service-b"]);
});

test("a canonical Windows root loses its \\\\?\\ prefix", () => {
  assert.deepEqual(folderLabels(["\\\\?\\C:\\Users\\me\\orders"]), ["orders"]);
});

test("two folders with the same last part also show their parent", () => {
  assert.deepEqual(folderLabels(["/work/a/api", "/work/b/api", "/work/web"]), ["api (a)", "api (b)", "web"]);
});

test("a root with no parts keeps its text", () => {
  assert.deepEqual(folderLabels(["/"]), ["/"]);
});
