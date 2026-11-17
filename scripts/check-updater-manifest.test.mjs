import { test } from "node:test";
import assert from "node:assert/strict";
import { checkManifest } from "./check-updater-manifest.mjs";

const platforms = Object.fromEntries(["windows-x86_64", "darwin-aarch64", "darwin-x86_64", "linux-x86_64"].map((name) => [name, { signature: "signed", url: "https://example.org/artifact" }]));
const manifest = { version: "0.10.0", notes: "## Changes\n- Update", platforms };

test("accepts the notes and signed artifacts from every platform", () => {
  assert.doesNotThrow(() => checkManifest(manifest, "0.10.0", "## Changes\n- Update\n"));
});
test("rejects empty or different notes and the wrong version", () => {
  for (const notes of ["", "Other notes"]) assert.throws(() => checkManifest({ ...manifest, notes }, "0.10.0", manifest.notes));
  assert.throws(() => checkManifest(manifest, "0.10.1", manifest.notes));
  assert.throws(() => checkManifest(manifest, "0.10.0", ""));
});
test("rejects an incomplete manifest after concurrent platform uploads", () => {
  for (const name of Object.keys(platforms)) {
    const incomplete = { ...platforms };
    delete incomplete[name];
    assert.throws(() => checkManifest({ ...manifest, platforms: incomplete }, "0.10.0", manifest.notes));
    assert.throws(() => checkManifest({ ...manifest, platforms: { ...platforms, [name]: { signature: "", url: "https://example.org" } } }, "0.10.0", manifest.notes));
  }
});
