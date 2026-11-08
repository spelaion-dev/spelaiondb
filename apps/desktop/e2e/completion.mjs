// Native completion regression + screenshot evidence. Uses a temporary personal profile and
// examples/demo only. Run after `npx tauri build --debug --no-bundle` with SPELAION_E2E_EXE.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const profile = mkdtempSync(join(tmpdir(), "spelaion-completion-"));
const shots = resolve(process.argv[2] ?? join(root, "docs/screenshots/sdb-16"));
mkdirSync(shots, { recursive: true });
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/debug/spelaion-desktop.exe");
const app = spawn(exe, [join(root, "examples/demo")], { env: {
  ...process.env, SPELAION_USER_DIR: profile, SPELAION_NO_UPDATES: "1",
  WEBVIEW2_USER_DATA_FOLDER: join(profile, "webview"), WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: "--remote-debugging-port=9340",
  SPELAION_MCP_CLIENTS_HOME: join(profile, "clients"), KUBECONFIG: join(profile, "none"),
}, stdio: "inherit" });
let browser, debuggerSession;
try {
  for (let n = 0; n < 60; n++) {
    try { browser = await chromium.connectOverCDP("http://127.0.0.1:9340"); break; }
    catch { await new Promise((done) => setTimeout(done, 500)); }
  }
  assert.ok(browser, "the native app opened its CDP port");
  const page = browser.contexts()[0].pages()[0]; page.setDefaultTimeout(30_000);
  await page.locator(".activity-bar .settings-button").waitFor();
  await page.locator(".activity-bar .settings-button").click();
  await page.locator(".settings-nav button:nth-child(2)").click();
  await page.locator('input[name="language"][value="en"]').click();
  await page.locator(".settings-dialog .wizard-header button").click();
  await page.getByRole("button", { name: "Review and trust" }).click();
  await page.getByRole("button", { name: "Trust", exact: true }).click();
  await page.locator('.connection-row:has-text("Shop (SQLite)") button[title="New query"]').click();
  const panel = page.locator(".tab-panel:not([hidden])");
  await panel.locator(".status-connecting").waitFor({ state: "detached" });
  const content = panel.locator(".cm-content");
  // A new console connects on its first explicit run; New query itself never runs SQL.
  await content.click(); await page.keyboard.insertText("SELECT 1;"); await page.keyboard.press("Control+Shift+Enter");
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  async function complete(text) {
    await content.click(); await page.keyboard.press("Control+A"); await page.keyboard.insertText(text); await page.keyboard.press("Control+Space"); await page.waitForTimeout(150);
  }
  async function expectCompletion(text, label) {
    for (let n = 0; n < 80; n++) {
      await complete(text);
      if (await page.locator(".cm-tooltip-autocomplete li", { hasText: label }).count()) return;
      await page.keyboard.press("Escape"); await page.waitForTimeout(100);
    }
    throw Error(`Missing completion: ${label}; ${await panel.innerText()}`);
  }
  await expectCompletion("SELECT * FROM orders o WHERE o.", "customer_id");
  await expectCompletion("WITH x AS(SELECT customer_id AS customer FROM orders) SELECT x.", "customer");
  await expectCompletion("SELECT * FROM (SELECT customer_id AS buyer FROM orders) q WHERE q.", "buyer");
  await expectCompletion("SELECT * FROM orders o JOIN customer c ON ", "o.customer_id = c.id");
  const predicate = page.locator(".cm-tooltip-autocomplete li", { hasText: "o.customer_id = c.id" });
  await predicate.click();
  assert.match(await content.innerText(), /ON o\.customer_id = c\.id/);
  assert.equal(await panel.locator(".result-tabs button", { hasText: /^Result / }).count(), 1, "completion never executes the SQL");
  await complete("SELECT * FROM orders WHERE ");
  await page.locator(".cm-tooltip-autocomplete").waitFor();
  assert.ok(!(await page.locator(".cm-tooltip-autocomplete").innerText()).includes("product"));
  await page.keyboard.press("Escape");

  // Tauri freezes invoke. A CDP function-call breakpoint observes the actual calls without
  // replacing the protected function. Record names only, never SQL/passwords/results.
  debuggerSession = await page.context().newCDPSession(page);
  await debuggerSession.send("Debugger.enable");
  const { result: invokeFunction } = await debuggerSession.send("Runtime.evaluate", { expression: "window.__TAURI_INTERNALS__.invoke" });
  let faultMetadata = false;
  const calls = [];
  debuggerSession.on("Debugger.paused", async ({ callFrames }) => {
    try {
      const callFrameId = callFrames[0].callFrameId;
      const { result } = await debuggerSession.send("Debugger.evaluateOnCallFrame", { callFrameId, expression: "cmd", returnByValue: true });
      calls.push(result.value);
      if (faultMetadata && result.value === "describe_table") {
        await debuggerSession.send("Debugger.evaluateOnCallFrame", { callFrameId, expression: "if (payload.session.startsWith('completion-')) payload.session = 'completion-test-missing-session'" });
      }
    } finally { await debuggerSession.send("Debugger.resume").catch(() => undefined); }
  });
  const observe = () => debuggerSession.send("Debugger.setBreakpointOnFunctionCall", { objectId: invokeFunction.objectId });
  let { breakpointId } = await observe();
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("workspace_info"));
  assert.ok(calls.includes("workspace_info"), "the native transport observation actually works"); calls.length = 0;
  await content.click(); await page.keyboard.press("Control+A");
  await page.keyboard.type("SELECT * FROM orders o WHERE o.c", { delay: 35 });
  await page.keyboard.press("Control+Space"); await page.waitForTimeout(500);
  assert.deepEqual(calls.filter((call) => /list_schemas|list_objects|describe_table|active_schema|connect_from_session|run_script/.test(call)), []);
  await page.keyboard.press("Escape");
  await debuggerSession.send("Debugger.removeBreakpoint", { breakpointId });

  for (const [setting, typed, expected] of [["upper", "se", "SELECT"], ["lower", "SE", "select"], ["preserve", "se", "select"]]) {
    await page.locator(".activity-bar .settings-button").click();
    await page.getByRole("button", { name: "Editor", exact: true }).click();
    await page.locator('select[name="editor-keywordCase"]').selectOption(setting);
    await page.locator(".settings-dialog .wizard-header button").click();
    await complete(typed);
    await page.locator(".cm-tooltip-autocomplete .cm-completionLabel", { hasText: new RegExp(`^${expected}$`) }).waitFor();
    await page.keyboard.press("Escape");
  }

  // Theme changes go through Settings and the persisted preference, as a user would.
  for (const theme of ["light", "dark"]) {
    await page.locator(".activity-bar .settings-button").click();
    await page.getByRole("button", { name: "Appearance", exact: true }).click();
    await page.locator(`input[name="theme"][value="${theme}"]`).click();
    await page.locator(".settings-dialog .wizard-header button").click();
    for (const [width, height] of [[1400, 900], [900, 600]]) {
      await page.setViewportSize({ width, height });
      await complete("WITH recent_orders AS (SELECT id, customer_id, total FROM orders)\nSELECT r.\nFROM recent_orders r;");
      await page.keyboard.press("Escape"); await page.keyboard.press("Control+Home"); await page.keyboard.press("ArrowDown"); await page.keyboard.press("End"); await page.keyboard.press("Control+Space");
      await page.locator(".cm-tooltip-autocomplete").waitFor();
      const contrast = await page.locator('.cm-tooltip-autocomplete li[aria-selected="true"]').evaluate((row) => {
        const rgb = (text) => text.match(/[\d.]+/g).map(Number);
        const luminance = (values) => values.slice(0, 3).map((v) => v / 255).map((v) => v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4).reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
        const panel = rgb(getComputedStyle(row.closest(".cm-tooltip")).backgroundColor);
        const selected = rgb(getComputedStyle(row).backgroundColor), alpha = selected[3] ?? 1;
        const background = selected.slice(0, 3).map((v, i) => v * alpha + panel[i] * (1 - alpha));
        return [".cm-completionLabel", ".cm-completionDetail"].map((selector) => {
          const foreground = luminance(rgb(getComputedStyle(row.querySelector(selector)).color)), back = luminance(background);
          return (Math.max(foreground, back) + 0.05) / (Math.min(foreground, back) + 0.05);
        });
      });
      assert.ok(contrast.every((ratio) => ratio >= 4.5), `selected completion text contrast: ${contrast.join(", ")}`);
      await page.screenshot({ path: join(shots, `after-${theme}-${width}.png`) });
      const popup = await page.locator(".cm-tooltip-autocomplete").boundingBox();
      assert.ok(popup.x >= 0 && popup.y >= 0 && popup.x + popup.width <= width && popup.y + popup.height <= height, "completion popup fits the viewport");
      await page.keyboard.press("ArrowDown"); await page.keyboard.press("Enter");
      assert.match(await content.innerText(), /SELECT r\.(id|customer_id|total)/);
      await page.keyboard.press("Control+z"); assert.match(await content.innerText(), /SELECT r\.\n/);
      await page.keyboard.press("Escape");
    }
  }
  const connectionId = await panel.getByLabel("Connection", { exact: true }).inputValue();
  faultMetadata = true;
  ({ breakpointId } = await observe());
  await page.evaluate((id) => {
    window.dispatchEvent(new CustomEvent("spelaion-schemas-refreshed", { detail: id }));
  }, connectionId);
  await panel.getByText(/Could not load completion metadata:/).waitFor();
  await page.screenshot({ path: join(shots, "metadata-error-dark-900.png") });
  faultMetadata = false;
  await debuggerSession.send("Debugger.removeBreakpoint", { breakpointId });
  const row = page.locator('.connection-row:has-text("Shop (SQLite)")');
  await row.locator(".twisty").click();
  await page.getByText("main", { exact: true }).waitFor();
  await row.focus(); await page.keyboard.press("F5");
  await panel.getByText(/Could not load completion metadata:/).waitFor({ state: "detached" });
  await expectCompletion("SELECT * FROM orders o WHERE o.", "customer_id");
  console.log("Native completions passed: CTEs, derived tables, aliases, WHERE, FK insertion, keyboard/undo, zero typing RPCs, both themes and sizes.");
} finally {
  await debuggerSession?.send("Debugger.disable").catch(() => undefined);
  await browser?.close(); app.kill();
}
