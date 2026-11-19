// What the views ask about our plugins (src/pluginStatus.ts, SDB-373): one answer for the
// Plugins page, the "add connection" picker and a connection that could not open. Run by
// `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { installable, installText, needsInstalling, pluginNeeded } from "../src/pluginStatus.ts";

const offered = { version: "0.3.0", engines: ["duckdb"], archiveSize: 3 * 1024 * 1024, address: "https://example.test/x.tar.gz" };

function plugin(overrides) {
  return {
    name: "spelaiondb-plugin-duckdb",
    engines: ["duckdb"],
    availability: "notInstalled",
    install: { kind: "idle" },
    inUse: null,
    refused: [],
    offered,
    update: null,
    missing: null,
    message: null,
    installed: [],
    openSessions: 0,
    ...overrides,
  };
}

/** Keys and values, as i18next would get them. */
const t = (key, values) => (values ? `${key} ${JSON.stringify(values)}` : key);

test("an engine needs its plugin until a build of it is in use, whatever else is here", () => {
  for (const availability of ["notInstalled", "refused", "missing"]) {
    assert.equal(needsInstalling(plugin({ availability })), true, availability);
  }
  assert.equal(needsInstalling(plugin({ availability: "inUse" })), false);
  const overview = { acceptsCatalogue: true, updatesAllowed: true, catalogueSerial: 1, plugins: [plugin({})] };
  assert.equal(pluginNeeded(overview, "duckdb")?.name, "spelaiondb-plugin-duckdb");
  assert.equal(pluginNeeded(overview, "postgres"), null, "another engine");
  assert.equal(pluginNeeded(null, "duckdb"), null, "nothing known yet");
  // Downloading does not make it usable: the engine still needs it until it is installed.
  const downloading = { ...overview, plugins: [plugin({ install: { kind: "downloading", install: 1, received: 10, total: 20 } })] };
  assert.ok(pluginNeeded(downloading, "duckdb"));
  const inUse = { ...overview, plugins: [plugin({ availability: "inUse" })] };
  assert.equal(pluginNeeded(inUse, "duckdb"), null);
});

test("Install fetches the offered build when none is in use, and Update only when it is newer", () => {
  assert.equal(installable(plugin({})), offered);
  assert.equal(installable(plugin({ availability: "refused" })), offered);
  assert.equal(installable(plugin({ availability: "inUse" })), null, "the newest is in use");
  assert.equal(installable(plugin({ availability: "inUse", update: "0.3.0" })), offered);
  assert.equal(installable(plugin({ offered: null })), null, "nothing offered");
});

test("every view says the same about an install: downloading, failed, or nothing", () => {
  assert.equal(installText(plugin({}), t, "en"), null);
  assert.equal(
    installText(plugin({ install: { kind: "downloading", install: 4, received: 1024 * 1024, total: 3 * 1024 * 1024 } }), t, "en"),
    'plugins.downloading {"received":"1.0 MB","total":"3.0 MB"}',
  );
  assert.equal(installText(plugin({ install: { kind: "downloading", install: 4, received: 5, total: 0 } }), t, "en"), "plugins.installing");
  assert.equal(
    installText(plugin({ install: { kind: "failed", why: "installing it was stopped" } }), t, "en"),
    'plugins.installError {"detail":"installing it was stopped"}',
  );
});
