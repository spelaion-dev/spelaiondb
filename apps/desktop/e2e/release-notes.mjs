// Exercise the real desktop window with the development frontend. Only the updater's result
// and install action are simulated; updates stay switched off and no download can take place.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, rmSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { basename, join, resolve, sep } from "node:path";
import { chromium } from "playwright-core";

async function freePort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const port = server.address().port;
  server.close();
  await once(server, "close");
  return port;
}

const root = resolve(import.meta.dirname, "../../..");
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/release/spelaion-desktop" + (process.platform === "win32" ? ".exe" : ""));
const user = mkdtempSync(join(tmpdir(), "spelaion-notes-"));
const debugPort = await freePort();
let vitePort = await freePort();
while (vitePort === debugPort) vitePort = await freePort();
const vite = spawn(process.execPath, [join(root, "apps/desktop/node_modules/vite/bin/vite.js"), "--host", "127.0.0.1", "--port", String(vitePort), "--strictPort"], { cwd: join(root, "apps/desktop"), windowsHide: true, stdio: "ignore" });
const app = spawn(exe, [], { env: { ...process.env, SPELAION_USER_DIR: user, SPELAION_NO_UPDATES: "1", WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${debugPort}`, WEBVIEW2_USER_DATA_FOLDER: join(user, "webview") }, windowsHide: true, stdio: "inherit" });
let browser;
try {
  for (let i = 0; i < 120 && !browser; i++) {
    try { browser = await chromium.connectOverCDP(`http://127.0.0.1:${debugPort}`); }
    catch { await new Promise((done) => setTimeout(done, 500)); }
  }
  if (!browser) throw new Error("Could not connect to the desktop app");
  for (let i = 0; i < 120; i++) {
    try { if ((await fetch(`http://127.0.0.1:${vitePort}/`)).ok) break; }
    catch { /* Vite is starting. */ }
    if (i === 119) throw new Error("Vite did not start");
    await new Promise((done) => setTimeout(done, 500));
  }
  const page = browser.contexts()[0].pages()[0];
  await page.route("http://tauri.localhost/**", async (route) => {
    const url = new URL(route.request().url());
    const response = await fetch(`http://127.0.0.1:${vitePort}${url.pathname}${url.search}`);
    await route.fulfill({ status: response.status, contentType: response.headers.get("content-type") ?? "text/plain", body: Buffer.from(await response.arrayBuffer()) });
  });
  await page.reload();
  await page.locator('script[src="/src/main.tsx"]').waitFor({ state: "attached" });
  await page.locator(".sidebar").waitFor();
  await page.waitForFunction(async () => {
    const { useUpdate } = await import("/src/update.ts");
    return !useUpdate.getState().checking && useUpdate.getState().status !== null;
  });
  await page.evaluate(async () => {
    const { applyLanguage } = await import("/src/i18n.ts");
    await applyLanguage("en");
    window.__notesInstallCount = 0;
    const { useUpdate } = await import("/src/update.ts");
    useUpdate.setState({ status: { kind: "available", version: "9.9.9", currentVersion: "0.8.0", notes: "## Highlights\n- **Faster** queries\n- [Details](javascript:alert(1))\n\n<img src=x onerror=alert(1)>\n\n" + "Long note. ".repeat(1000) }, dismissed: false, install: async () => { window.__notesInstallCount++; useUpdate.setState({ error: "Test install failure" }); } });
  });
  const notice = page.locator(".update-notice");
  await notice.getByRole("button", { name: "Release notes…" }).click();
  const dialog = page.getByRole("dialog", { name: "What's new in 9.9.9" });
  await dialog.getByRole("heading", { name: "Highlights" }).waitFor();
  await dialog.getByText("Faster", { exact: true }).waitFor();
  assert.equal(await dialog.locator("img, script, a").count(), 0, "untrusted notes cannot create active HTML or links");
  await dialog.getByText("<img src=x onerror=alert(1)>", { exact: true }).waitFor();
  assert.ok(await dialog.locator(".release-notes-body").evaluate((body) => body.scrollHeight > body.clientHeight), "long notes scroll");
  assert.equal(await page.evaluate(() => window.__notesInstallCount), 0);
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "detached" });

  await page.locator(".activity-bar .settings-button").click();
  const settings = page.getByRole("dialog", { name: "Settings" });
  await settings.getByRole("button", { name: "Updates", exact: true }).click();
  await settings.getByRole("button", { name: "Release notes…" }).click();
  await dialog.waitFor();
  await page.keyboard.press("Escape");
  await dialog.waitFor({ state: "detached" });
  await settings.waitFor({ state: "visible" });
  await page.evaluate(async () => {
    const { useUpdate } = await import("/src/update.ts");
    useUpdate.setState({ status: { kind: "available", version: "9.9.9", currentVersion: "0.8.0", notes: null } });
  });
  await settings.getByRole("button", { name: "Release notes…" }).click();
  await dialog.getByText("Release notes are not available for this version.").waitFor();
  await dialog.getByRole("button", { name: "Install and restart" }).click();
  await dialog.getByRole("alert").filter({ hasText: "Test install failure" }).waitFor();
  assert.equal(await page.evaluate(() => window.__notesInstallCount), 1, "install follows only an explicit click");
  await dialog.getByRole("button", { name: "Later" }).click();
  await dialog.waitFor({ state: "detached" });
  await notice.waitFor({ state: "hidden" });
  console.log("Release notes smoke test passed.");
} finally {
  if (browser) await browser.close();
  app.kill();
  vite.kill();
  // WebView2 can release its files shortly after the process exits.
  const target = resolve(user);
  if (target.startsWith(resolve(tmpdir()) + sep) && basename(target).startsWith("spelaion-notes-")) {
    try { rmSync(target, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 }); } catch { /* temporary test data */ }
  }
}
