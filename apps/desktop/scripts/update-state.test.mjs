// What the update's state means for the page (src/updateState.ts, SDB-377), run by
// `npm run build` with `node --test`.
import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { downloadText, installOf, isInstalling, updateProblem } from "../src/updateState.ts";

const available = (install, method = "inApp") => ({ kind: "available", version: "0.13.0", currentVersion: "0.12.0", notes: null, method, install });

test("the page shows a download's progress, by share or by size", () => {
  const downloading = available({ kind: "downloading", received: 4_500_000, total: 10_000_000 });
  assert.equal(isInstalling(downloading), true);
  assert.deepEqual(downloadText(installOf(downloading), "en"), { key: "downloading", values: { percent: "45" } });
  const unknownSize = { kind: "downloading", received: 2_500_000, total: null };
  assert.deepEqual(downloadText(unknownSize, "en"), { key: "downloadingSize", values: { received: "2.5 MB" } });
  assert.equal(downloadText({ kind: "ready" }, "en"), null);
  assert.equal(downloadText({ kind: "failed", reason: "x" }, "en"), null);
});

test("only an update the app installs has an install state", () => {
  assert.equal(installOf({ kind: "unknown" }), null);
  assert.equal(installOf({ kind: "upToDate" }), null);
  assert.equal(installOf({ kind: "unavailable" }), null);
  assert.equal(installOf(available({ kind: "ready" }, "deb")), null);
  assert.deepEqual(installOf(available({ kind: "ready" })), { kind: "ready" });
  assert.equal(isInstalling(available({ kind: "ready" })), false);
});

test("a failed install says why and that it can be tried again; a page request says its own", () => {
  const failed = available({ kind: "failed", reason: "Download request failed with status: 503" });
  assert.deepEqual(updateProblem({ status: failed, request: { kind: "idle" } }), { action: "install", message: "Download request failed with status: 503" });
  assert.equal(isInstalling(failed), false, "Install is offered again");
  const checkFailed = { kind: "failed", action: "check", message: "offline" };
  assert.deepEqual(updateProblem({ status: { kind: "unknown" }, request: checkFailed }), { action: "check", message: "offline" });
  assert.equal(updateProblem({ status: available({ kind: "ready" }), request: { kind: "checking" } }), null);
});

test("every problem and the progress have their words in both languages", async () => {
  for (const language of ["en", "es"]) {
    for (const [file, group] of [["app.json", "update"], ["settings.json", "updates"]]) {
      const texts = JSON.parse(await readFile(new URL(`../src/locales/${language}/${file}`, import.meta.url), "utf8"))[group];
      for (const action of ["check", "install", "openReleasesPage", "setCheckAtStartup"]) assert.ok(texts.failed?.[action], `${language} ${file}: ${action}`);
      assert.ok(texts.downloading && texts.downloadingSize, `${language} ${file}`);
    }
  }
});
