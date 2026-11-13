// Where Linux's file dialogs start (src/dialogPaths.ts), run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { folderOf, savePlace, startFolders } from "../src/dialogPaths.ts";

const workspace = "/home/ana/work/shop";

test("a path's folder, none for a bare name", () => {
  assert.equal(folderOf("/home/ana/q/report.sql"), "/home/ana/q");
  assert.equal(folderOf("/report.sql"), "/");
  assert.equal(folderOf("report.sql"), null);
});

test("an open dialog starts in its file's folder, then the workspace", () => {
  assert.deepEqual(startFolders("/home/ana/.ssh/id_ed25519", workspace), ["/home/ana/.ssh", workspace]);
  assert.deepEqual(startFolders(`${workspace}/q.sql`, workspace), [workspace]);
  assert.deepEqual(startFolders(null, workspace), [workspace]);
  assert.deepEqual(startFolders("", null), []);
});

test("a save dialog with a full path starts in its folder and proposes its name", () => {
  assert.deepEqual(savePlace("/home/ana/q/report.sql", workspace), { folders: ["/home/ana/q", workspace], fileName: "report.sql" });
});

test("a save dialog with a bare name starts in the workspace", () => {
  assert.deepEqual(savePlace("chart.png", workspace), { folders: [workspace], fileName: "chart.png" });
  assert.deepEqual(savePlace("chart.png", null), { folders: [], fileName: "chart.png" });
  assert.deepEqual(savePlace(undefined, workspace), { folders: [workspace], fileName: null });
});
