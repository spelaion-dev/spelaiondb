// Isolated native history/saved-query regression and screenshot evidence (SDB-17).
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, rmSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const profile = mkdtempSync(join(tmpdir(), "spelaion-library-"));
const shots = resolve(process.argv[2] ?? join(root, "docs/screenshots/sdb-17"));
const baseline = process.env.SPELAION_LIBRARY_BASELINE === "1";
mkdirSync(shots, { recursive: true });
const app = spawn(process.env.SPELAION_E2E_EXE ?? join(root, "target/debug/spelaion-desktop.exe"), [], { env: {
  ...process.env, SPELAION_USER_DIR: profile, SPELAION_NO_UPDATES: "1",
  WEBVIEW2_USER_DATA_FOLDER: join(profile, "webview"), WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: "--remote-debugging-port=9341",
  SPELAION_MCP_CLIENTS_HOME: join(profile, "clients"), KUBECONFIG: join(profile, "none"),
}, stdio: "inherit" });
let browser, page;
try {
  for (let n = 0; n < 60; n++) {
    try { browser = await chromium.connectOverCDP("http://127.0.0.1:9341"); break; }
    catch { await new Promise((done) => setTimeout(done, 500)); }
  }
  assert.ok(browser, "native app opens its CDP port");
  page = browser.contexts()[0].pages()[0]; page.setDefaultTimeout(30_000);
  await page.locator(".activity-bar .settings-button").waitFor();
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("set_language", { language: "en" }));
  const db = new DatabaseSync(join(profile, "db/state.db"));
  const historyColumns = db.prepare("pragma table_info(history)").all().map((c) => c.name);
  const now = new Date(2026, 10, 8, 12).getTime();
  db.exec("begin");
  const insert = db.prepare(`insert into history(at_ms,workspace,connection,statement,duration_ms,rows,error,cancelled${historyColumns.includes("database") ? ",database" : ""}) values(?, '', ?, ?, ?, ?, ?, ?${historyColumns.includes("database") ? ",?" : ""})`);
  for (let n = 0; n < 10_000; n++) {
    const failed = n % 7 === 0, cancelled = n % 11 === 0;
    const args = [now - n * 60_000, n % 2 ? "user:analytics" : "user:shop", `SELECT customer_id, SUM(total) AS revenue\nFROM orders WHERE region = 'North'\nGROUP BY customer_id; -- report ${n}`, 12 + n % 90, failed ? 0 : 124, cancelled ? "Query cancelled" : failed ? "Table orders is not available" : null, +cancelled];
    if (historyColumns.includes("database")) args.push(n % 2 ? "analytics" : "shop");
    insert.run(...args);
  }
  const snippet = db.prepare("insert into snippets(workspace,name,statement,connection,updated_ms) values('', ?, ?, 'user:shop', ?)");
  for (const [name, sql] of [["Daily revenue", "SELECT region, SUM(total) FROM orders GROUP BY region;"], ["Customer lookup", "SELECT * FROM customer WHERE email = :email;"], ["Recent orders", "SELECT * FROM orders ORDER BY created_at DESC LIMIT 100;"]]) snippet.run(name, sql, now);
  db.exec("commit"); db.close();
  await page.reload(); await page.locator(".activity-bar .settings-button").waitFor();
  async function settled(panel) {
    await page.waitForTimeout(60); // Let React commit the filter's loading state before observing it.
    await panel.locator('.history-entries[aria-busy="false"]').waitFor();
  }
  if (!baseline) {
    await page.locator('[aria-controls="query-history"]').click();
    const history = page.locator("#query-history"); await settled(history);
    assert.equal(await history.locator(".history-entry").count(), 50, "history renders just one page");
    const firstSql = await history.locator(".history-sql").first().innerText();
    await history.getByRole("button", { name: "Next", exact: true }).click();
    await history.getByText("Page 2", { exact: true }).waitFor(); await settled(history);
    assert.equal(await history.locator(".history-entry").count(), 50);
    assert.notEqual(await history.locator(".history-sql").first().innerText(), firstSql);
    await history.getByRole("button", { name: "Previous", exact: true }).click();
    await history.getByText("Page 1", { exact: true }).waitFor(); await settled(history);
    assert.equal(await history.locator(".history-sql").first().innerText(), firstSql);
    await history.getByLabel("Status", { exact: true }).selectOption("cancelled"); await settled(history);
    assert.ok((await history.locator(".library-status").allTextContents()).every((text) => text === "Cancelled"));
    await history.getByLabel("Database", { exact: true }).selectOption("db:analytics"); await settled(history);
    assert.ok((await history.locator(".history-meta").allTextContents()).every((text) => text.includes("analytics")));
    await history.getByRole("button", { name: "Pin", exact: true }).first().click();
    await history.getByRole("button", { name: "Unpin", exact: true }).waitFor();
    await history.getByText("Time range and pins", { exact: true }).click();
    await history.getByLabel("Pinned entries only").check(); await settled(history);
    assert.equal(await history.locator(".history-entry").count(), 1);
    const exportedSql = await history.locator(".history-sql").innerText();
    const exportPath = join(profile, "history.sql");
    await page.evaluate((path) => { window.__spelaionSavePath = path; }, exportPath);
    await history.getByRole("button", { name: "Export matching SQL…", exact: true }).click();
    await history.getByText(/Exported 1 statement to/).waitFor();
    assert.ok(readFileSync(exportPath, "utf8").includes(exportedSql));
    await history.getByRole("button", { name: "Unpin", exact: true }).click();
    await history.getByText("No matching queries.", { exact: true }).waitFor();
    await page.screenshot({ path: join(shots, "empty-pins.png") });
    assert.ok(await history.getByRole("button", { name: "Export matching SQL…", exact: true }).isDisabled());
    await history.getByLabel("Pinned entries only").uncheck();
    await history.getByLabel("From date", { exact: true }).fill("2026-11-09");
    await history.getByLabel("Before date", { exact: true }).fill("2026-11-08");
    await history.getByText("The end date must follow the start date.", { exact: true }).waitFor();
    await history.getByLabel("From date", { exact: true }).fill(""); await history.getByLabel("Before date", { exact: true }).fill("");
    await history.getByLabel("Status", { exact: true }).selectOption(""); await history.getByLabel("Database", { exact: true }).selectOption("all"); await settled(history);
    await history.getByRole("button", { name: "Close query history" }).click();

    await page.locator('[aria-controls="saved-queries"]').click();
    const saved = page.locator("#saved-queries"); await settled(saved);
    for (const name of ["Reports", "Archive"]) {
      const trigger = saved.getByRole("button", { name: "New folder…", exact: true });
      await trigger.click(); const dialog = page.getByRole("dialog", { name: "New saved-query folder" });
      await dialog.getByLabel("Folder name").waitFor();
      assert.ok(await dialog.getByLabel("Folder name").evaluate((input) => document.activeElement === input));
      await page.keyboard.press("Shift+Tab"); assert.ok(await dialog.getByRole("button", { name: "Cancel", exact: true }).evaluate((button) => document.activeElement === button));
      await page.keyboard.press("Tab"); await dialog.getByLabel("Folder name").fill(name); await page.keyboard.press("Enter"); await dialog.waitFor({ state: "detached" });
      await page.waitForFunction(() => document.activeElement?.textContent === "New folder…");
    }
    await saved.getByRole("button", { name: "New folder…", exact: true }).click();
    const duplicate = page.getByRole("dialog", { name: "New saved-query folder" });
    await duplicate.getByLabel("Folder name").fill("Reports"); await page.keyboard.press("Enter");
    await duplicate.getByRole("alert").waitFor();
    assert.equal(await duplicate.getByRole("alert").innerText(), "local store: a saved-query folder with this name already exists; choose another name");
    assert.equal(await duplicate.getByLabel("Folder name").inputValue(), "Reports", "a folder error preserves the typed name");
    const duplicateFocus = await duplicate.evaluate((dialog) => ({ tag: document.activeElement?.tagName, inside: dialog.contains(document.activeElement), name: document.activeElement?.getAttribute("aria-label") }));
    console.log("Duplicate-folder focus after failure:", duplicateFocus);
    assert.ok(duplicateFocus.inside, "a folder failure keeps keyboard focus inside its dialog");
    assert.ok(await duplicate.getByLabel("Folder name").evaluate((input) => document.activeElement === input), "a failed folder creation restores focus for editing the name");
    await page.keyboard.press("Escape"); await duplicate.waitFor({ state: "detached" });
    const daily = saved.locator(".snippet-entry", { hasText: "Daily revenue" });
    await daily.getByText("Organize", { exact: true }).click();
    await daily.getByLabel("Move Daily revenue to folder", { exact: true }).selectOption({ label: "Reports" }); await settled(saved);
    await daily.getByText("Organize", { exact: true }).click();
    await daily.getByLabel("Move Daily revenue to folder", { exact: true }).selectOption({ label: "Archive" }); await settled(saved);
    await saved.getByLabel("Folder", { exact: true }).selectOption({ label: "Archive · personal" }); await settled(saved);
    assert.equal(await saved.locator(".snippet-entry").count(), 1);
    await saved.getByLabel("Search saved queries").fill("GROUP BY");
    await page.waitForTimeout(250); await settled(saved);
    assert.equal(await saved.locator(".snippet-entry").count(), 1, "search matches SQL even when the name does not match");
    await daily.getByText("Organize", { exact: true }).click(); await daily.getByRole("button", { name: "Delete", exact: true }).click();
    const deletion = page.getByRole("dialog", { name: "Delete “Daily revenue”?" });
    assert.ok(await deletion.getByRole("button", { name: "Cancel", exact: true }).evaluate((button) => document.activeElement === button));
    await page.keyboard.press("Escape"); await deletion.waitFor({ state: "detached" });
    assert.equal(await saved.locator(".snippet-entry").count(), 1, "Escape keeps the query");
    await daily.getByRole("button", { name: "Open", exact: true }).click(); await saved.waitFor({ state: "detached" });
    assert.equal(await page.locator(".tab-panel:not([hidden]) .result-tabs button").count(), 0, "opening a saved query never executes");
    await page.locator('[aria-controls="saved-queries"]').click(); await settled(saved);
    assert.equal(await saved.getByLabel("Folder", { exact: true }).inputValue(), "all");
    await saved.getByRole("button", { name: "Close saved queries" }).click();
    await page.keyboard.press("Control+Shift+w");
  }
  for (const theme of ["light", "dark"]) {
    await page.evaluate((theme) => window.__TAURI_INTERNALS__.invoke("set_theme", { theme }), theme);
    await page.reload(); await page.locator(".activity-bar .settings-button").waitFor();
    for (const [width, height] of [[1400, 900], [900, 600]]) {
      await page.setViewportSize({ width, height });
      for (const [panel, prefix] of [["query-history", "history"], ["saved-queries", "saved"]]) {
        await page.locator(`[aria-controls="${panel}"]`).click();
        await page.locator(`#${panel} .history-entry`).first().waitFor();
        if (!baseline) {
          if (panel === "query-history") {
            assert.equal(await page.locator(`#${panel} .library-status`).count(), await page.locator(`#${panel} .history-entry`).count());
            assert.ok(await page.locator(`#${panel} .library-status`).count() > 0, "contrast check observes real history statuses");
          }
          const ratios = await page.locator(`#${panel} .library-status`).evaluateAll((labels) => labels.map((label) => {
            const rgb = (color) => color.match(/[\d.]+/g).slice(0, 3).map(Number).map((value) => value / 255);
            const luminance = (color) => rgb(color).map((value) => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4).reduce((sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index], 0);
            const style = getComputedStyle(label);
            if (style.opacity !== "1" || [style.color, style.backgroundColor].some((color) => { const channels = color.match(/[\d.]+/g); return channels.length > 3 && Number(channels[3]) !== 1; })) throw Error("Status contrast requires compositing if opacity is introduced");
            const a = luminance(style.color), b = luminance(style.backgroundColor);
            return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
          }));
          assert.ok(ratios.every((ratio) => ratio >= 4.5), `${theme}: status text has at least 4.5:1 contrast`);
          const bottom = await page.locator(`#${panel} .library-paging`).boundingBox();
          assert.ok(bottom && bottom.y + bottom.height <= height, "paging stays reachable at the minimum viewport");
        }
        await page.screenshot({ path: join(shots, `${baseline ? "before" : "after"}-${prefix}-${theme}-${width}.png`) });
        await page.locator(`#${panel} .history-header button`).last().click();
      }
    }
  }
  if (!baseline) {
    // Saved-query paging must also stay bounded when the library is larger than a page.
    const fixture = new DatabaseSync(join(profile, "db/state.db"));
    fixture.exec("begin"); const insertSaved = fixture.prepare("insert into snippets(workspace,name,statement,connection,updated_ms) values('',?,?,'',?)");
    for (let n = 0; n < 120; n++) insertSaved.run(`zz paging ${String(n).padStart(3, "0")}`, `SELECT ${n} AS paging_marker`, now);
    fixture.prepare("insert into history(at_ms,workspace,connection,statement,duration_ms,rows,error,cancelled,database) values(?, '', 'user:shop', 'SELECT empty_error_marker', 1, 0, '', 0, 'shop')").run(now + 1);
    fixture.exec("commit"); fixture.close();
    await page.locator('[aria-controls="saved-queries"]').click(); const saved = page.locator("#saved-queries"); await settled(saved);
    assert.equal(await saved.locator(".snippet-entry").count(), 50);
    const first = await saved.locator(".snippet-name").first().innerText();
    await saved.getByRole("button", { name: "Next", exact: true }).click(); await saved.getByText("Page 2", { exact: true }).waitFor(); await settled(saved);
    assert.equal(await saved.locator(".snippet-entry").count(), 50); assert.notEqual(await saved.locator(".snippet-name").first().innerText(), first);
    await saved.getByLabel("Search saved queries").fill("paging_marker"); await page.waitForTimeout(250); await settled(saved);
    assert.equal(await saved.locator(".snippet-entry").count(), 50); await saved.getByText("Page 1", { exact: true }).waitFor();
    await saved.getByRole("button", { name: "Close saved queries" }).click();

    await page.locator('[aria-controls="query-history"]').click();
    const emptyErrorHistory = page.locator("#query-history"); await settled(emptyErrorHistory);
    await emptyErrorHistory.getByLabel("Status", { exact: true }).selectOption("error");
    await emptyErrorHistory.getByLabel("Search query history").fill("empty_error_marker"); await page.waitForTimeout(250); await settled(emptyErrorHistory);
    assert.equal(await emptyErrorHistory.locator(".history-entry").count(), 1);
    assert.equal(await emptyErrorHistory.locator(".library-status").innerText(), "Error", "an empty error message is still an error");
    await emptyErrorHistory.getByRole("button", { name: "Close query history" }).click();

    // Fault only the command's arguments at the actual immutable native transport. A real
    // core validation error exercises the panel's banner and Retry without faking its data.
    const session = await page.context().newCDPSession(page); await session.send("Debugger.enable");
    const { result } = await session.send("Runtime.evaluate", { expression: "window.__TAURI_INTERNALS__.invoke" });
    let failHistory = true;
    session.on("Debugger.paused", async ({ callFrames }) => {
      try {
        const callFrameId = callFrames[0].callFrameId;
        await session.send("Debugger.evaluateOnCallFrame", { callFrameId, expression: failHistory ? "if (cmd === 'history_page') { payload.filter.fromMs = 2; payload.filter.untilMs = 1; }" : "void 0" });
      } finally { await session.send("Debugger.resume").catch(() => undefined); }
    });
    const { breakpointId } = await session.send("Debugger.setBreakpointOnFunctionCall", { objectId: result.objectId });
    await page.locator('[aria-controls="query-history"]').click(); const history = page.locator("#query-history");
    await history.getByRole("alert").waitFor(); await page.screenshot({ path: join(shots, "load-error-dark-900.png") });
    assert.match(await history.getByRole("alert").innerText(), /^Could not load history:/);
    assert.match(await history.getByRole("alert").innerText(), /end of the time range must follow its start/);
    failHistory = false; await history.getByRole("button", { name: "Retry", exact: true }).click(); await settled(history);
    assert.equal(await history.locator(".history-entry").count(), 50);
    await session.send("Debugger.removeBreakpoint", { breakpointId }); await session.send("Debugger.disable"); await session.detach();
    console.log("Native library regression passed: 10k history, bounded history/saved pages, status/database/date filters, pins, SQL export, SQL search, folder moves, modal focus/Escape, disconnected reopen, native-error Retry, both themes/sizes.");
  }
  console.log(`${baseline ? "Baseline" : "Current"} history/saved-query captures passed with 10,000 fixture executions.`);
} catch (error) {
  await page?.screenshot({ path: join(shots, "failure.png") }).catch(() => undefined);
  throw error;
} finally {
  await browser?.close();
  // Stop only this test's known app process tree, including its WebView profile holders.
  if (process.platform === "win32" && app.pid && app.exitCode === null) spawnSync("taskkill", ["/pid", String(app.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" });
  else app.kill();
  if (app.exitCode === null) await Promise.race([new Promise((done) => app.once("exit", done)), new Promise((done) => setTimeout(done, 2_000))]);
  try { rmSync(profile, { recursive: true, force: true }); } catch { console.log(`Temporary profile retained: ${profile}`); }
}
