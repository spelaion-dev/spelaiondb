// Open a genuine SQL Server plan in the real native app using its private CDP profile.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { once } from "node:events";
import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const executable = process.env.SPELAION_E2E_EXE ?? join(root, "target/debug/spelaion-desktop.exe");
const fixture = join(root, "crates/spelaion-core/tests/fixtures/plans/sqlserver/spill.sqlplan");
const shots = resolve(process.argv[2] ?? join(root, "docs/screenshots/sdb-48"));
const profile = mkdtempSync(join(tmpdir(), "spelaion-plan48-"));
const listener = createServer();
listener.listen(0, "127.0.0.1"); await once(listener, "listening");
const port = listener.address().port; await new Promise(done => listener.close(done));
let browser, app, page;
try {
  mkdirSync(shots, { recursive: true });
  mkdirSync(join(profile, "connections"));
  writeFileSync(join(profile, "connections/sqlserver.toml"), "schema_version=1\nname='Plan fixture (SQL Server)'\nengine='sqlserver'\nenv='dev'\nhost='127.0.0.1'\nport=11433\ndatabase='master'\nuser='sa'\npassword={secret='env:SPELAION_PLAN48_PASSWORD'}\n[tls]\nmode='require'\n");
  app = spawn(executable, ["--plan", fixture], { cwd: profile, env: { ...process.env, SPELAION_USER_DIR: profile, SPELAION_PLAN48_PASSWORD: "Spelaion-test-1", SPELAION_NO_UPDATES: "1", SPELAION_MCP_CLIENTS_HOME: join(profile, "clients"), KUBECONFIG: join(profile, "none"), WEBVIEW2_USER_DATA_FOLDER: join(profile, "webview"), WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` }, windowsHide: true, stdio: "inherit" });
  for (let n = 0; n < 90; n++) { try { browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`); break; } catch { await new Promise(done => setTimeout(done, 500)); } }
  assert.ok(browser, "native app did not expose its private automation port");
  page = browser.contexts()[0].pages()[0]; page.setDefaultTimeout(30_000);
  await page.locator(".activity-bar .settings-button").click();
  const settings = page.locator(".settings-dialog");
  await settings.locator(".settings-nav button").nth(1).click();
  await settings.locator('input[name="language"][value="en"]').click();
  await page.waitForFunction(() => document.documentElement.lang.startsWith("en"));
  await settings.locator(".wizard-header button").click();
  await page.setViewportSize({ width: 1400, height: 900 });
  const plan = page.locator(".plan-viewer");
  await plan.locator(".plan-overview-numbers").waitFor();
  await plan.locator(".plan-tabs button").nth(1).click();
  await plan.locator(".plan-table tbody tr").first().waitFor();
  assert.match(await plan.locator(".plan-table").innerText(), /Sort/);
  await plan.locator(".plan-node-name").first().click();
  await page.screenshot({ path: join(shots, "sqlserver-spill.png") });
  await page.locator(".connection-row").filter({ hasText: "Plan fixture (SQL Server)" }).getByTitle("New query", { exact: true }).click();
  const panel = page.locator(".tab-panel:not([hidden])");
  await panel.locator(".status-connecting").waitFor({ state: "detached" });
  await panel.locator(".cm-content").click(); await page.keyboard.press("Control+A");
  await page.keyboard.insertText("SELECT TOP 5 name FROM sys.objects ORDER BY object_id");
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  const live = panel.getByRole("region", { name: "Execution plan", exact: true });
  await live.getByText("Estimates only", { exact: false }).waitFor();
  await live.getByLabel("ANALYZE", { exact: true }).check();
  await live.getByRole("button", { name: "Back to results", exact: true }).click();
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  await page.getByRole("dialog").getByRole("button", { name: "Execute and analyze", exact: true }).click();
  await live.getByText("Actual execution", { exact: false }).waitFor();
  await live.getByRole("button", { name: "Back to results", exact: true }).click();
  await panel.locator(".cm-content").click(); await page.keyboard.press("Control+A"); await page.keyboard.insertText("SELECT 42");
  await page.keyboard.press("Control+Shift+Enter");
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await panel.locator('[role="grid"]').focus();
  await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) [role="gridcell"]')?.textContent?.includes("42"));
  console.log("SQL Server fixture captured; estimated, analyzed and ordinary SQL succeeded on one session");
} finally {
  await browser?.close();
  if (app?.pid && app.exitCode === null) {
    if (process.platform === "win32") spawnSync("taskkill", ["/PID", String(app.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" }); else app.kill();
    if (app.exitCode === null) await Promise.race([once(app, "exit"), new Promise(done => setTimeout(done, 2000))]);
  }
  assert.ok(resolve(profile).startsWith(resolve(tmpdir(), "spelaion-plan48-")), "temporary profile must stay in the designated directory");
  rmSync(profile, { recursive: true, force: true });
}
