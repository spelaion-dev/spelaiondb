// Focused check of the real Windows app's search-engine cards. With
// SPELAION_E2E_SEARCH_ENABLED=1 it also queries the four isolated test servers.
// User configuration is never opened. Build with CONTRIBUTING.md's Tauri command,
// then run `node e2e/search-catalog.mjs [screenshots-dir]`. SPELAION_E2E_EXE can point at
// a current debug build made with `tauri build --debug --no-bundle`.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { rm } from "node:fs/promises";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "playwright-core";
import { webviewPage } from "./cdpPage.mjs";
import { requestClose } from "./processes.mjs";

if (process.platform !== "win32") {
  console.error("search-catalog.mjs drives WebView2 over CDP and requires Windows");
  process.exit(2);
}
const root = resolve(import.meta.dirname, "../../..");
const shots = resolve(process.argv[2] ?? join(root, "docs/screenshots/sdb-183"));
mkdirSync(shots, { recursive: true });
const personal = mkdtempSync(join(tmpdir(), "spelaion-search-catalog-"));
const workspace = join(personal, "workspace");
mkdirSync(workspace);
if (process.env.SPELAION_E2E_SEARCH_ENABLED === "1") {
  mkdirSync(join(personal, "connections"));
  for (const [engine, port] of [["elasticsearch",19200],["opensearch",19201],["solr",18983],["meilisearch",17700]]) {
    writeFileSync(join(personal, "connections", `${engine}.toml`), `schema_version = 1\nname = "Search ${engine}"\nengine = "${engine}"\nhost = "127.0.0.1"\nport = ${port}\n[tls]\nmode = "disable"\n`);
  }
}
const server = createServer();
server.listen(0, "127.0.0.1");
await once(server, "listening");
const port = server.address().port;
server.close();
await once(server, "close");
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/release/spelaion-desktop.exe");
const app = spawn(exe, [workspace], {
  windowsHide: true,
  stdio: "inherit",
  env: {
    ...process.env,
    SPELAION_USER_DIR: personal,
    SPELAION_NO_UPDATES: "1",
    SPELAION_E2E_OFFSCREEN: "1",
    SPELAION_MCP_CLIENTS_HOME: join(personal, "clients"),
    KUBECONFIG: join(personal, "no-kubeconfig"),
    WEBVIEW2_USER_DATA_FOLDER: join(personal, "webview2"),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port} --disable-features=CalculateNativeWinOcclusion --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling`,
  },
});
let browser;
try {
  for (let attempt = 0; attempt < 60; attempt++) {
    try { browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`); break; }
    catch { await new Promise((resolve) => setTimeout(resolve, 500)); }
  }
  assert.ok(browser, "the current Tauri app must expose its isolated debugging port");
  const page = await webviewPage(browser);
  page.setDefaultTimeout(10_000);
  await (await page.context().newCDPSession(page)).send("Emulation.setFocusEmulationEnabled", { enabled: true });
  await page.locator(".activity-bar .settings-button").waitFor();
  await page.evaluate(() => {
    Object.defineProperty(navigator, "languages", { configurable: true, value: ["en"] });
    window.dispatchEvent(new Event("languagechange"));
  });
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  const wizard = page.getByRole("dialog", { name: "Add connection" });
  const category = wizard.locator('section[aria-labelledby="engines-nosql"]');
  for (const [width, height] of [[1280, 800], [900, 600]]) {
    await page.setViewportSize({ width, height });
    for (const theme of ["light", "dark"]) {
      await page.evaluate((value) => { document.documentElement.dataset.theme = value; }, theme);
      for (const name of ["Elasticsearch", "OpenSearch", "Solr", "Meilisearch"]) {
        const card = category.locator(".engine-card", { hasText: name });
        assert.ok(!(await card.isDisabled()), `${name} has a verified official plugin pin`);
      }
      assert.equal(await wizard.locator(".engine-card", { hasText: "Open Distro" }).count(), 0);
      await category.scrollIntoViewIfNeeded();
      await page.screenshot({ path: join(shots, `picker-${theme}-${width}.png`) });
    }
  }
  const search = wizard.getByRole("searchbox");
  for (const name of ["Elasticsearch", "OpenSearch", "Solr", "Meilisearch"]) {
    await search.fill(name);
    await page.waitForFunction(() => document.querySelectorAll(".engine-card").length === 1);
    assert.ok(!(await wizard.locator(".engine-card").isDisabled()));
  }
  if (process.env.SPELAION_E2E_SEARCH_ENABLED === "1") {
    await search.fill("");
    await page.keyboard.press("Escape");
    for (const [engine, request] of [
      ["elasticsearch", 'GET /orders/_search\n{"size":200}'],
      ["opensearch", 'GET /orders/_search\n{"size":200}'],
      ["solr", 'GET /solr/orders/select?q=*%3A*&rows=200&wt=json'],
      ["meilisearch", 'POST /indexes/orders/search\n{"limit":200}'],
    ]) {
      await page.locator(`.connection-row:has-text("Search ${engine}") > .label`).dblclick();
      const panel = page.locator(".tab-panel:not([hidden])");
      await panel.locator(".status-connecting").waitFor({state:"detached"});
      await panel.locator(".cm-content").click();
      await page.keyboard.insertText(request);
      await page.keyboard.press("Control+Shift+Enter");
      await panel.locator(".statusline", {hasText:/[1-9]\d* rows?/}).waitFor();
      await panel.locator(".cm-content .sql-keyword").first().waitFor();
      if (engine === "elasticsearch") {
        await page.keyboard.press("Shift+Alt+F");
        await page.waitForFunction(() => document.querySelector(".tab-panel:not([hidden]) .cm-content")?.textContent.includes('  "size": 200'));
        for (const [width,height] of [[1280,800],[900,600]]) {
          await page.setViewportSize({width,height});
          for (const theme of ["light","dark"]) {
            await page.evaluate(async (value)=>{document.documentElement.dataset.theme=value;window.dispatchEvent(new Event("spelaion-themechange"));await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));},theme);
            await page.screenshot({path:join(shots,`http-${theme}-${width}.png`)});
          }
        }
      }
    }
  }
  console.log("PASS: four search engines are available and searchable in the real Tauri app, in both themes at 1280x800 and 900x600.");
} finally {
  await browser?.close();
  if (app.exitCode === null) {
    const stopped = once(app, "exit");
    requestClose(app.pid);
    await stopped;
  }
  await rm(personal, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
}
