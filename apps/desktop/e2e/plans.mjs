// Real native visual-plan regression, synthetic PG, MySQL and MariaDB databases, private
// profile/CDP and captures.
import assert from "node:assert/strict";
import { spawn, spawnSync } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { once } from "node:events";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const checkout = spawnSync("git", ["rev-parse", "HEAD"], { cwd:root, encoding:"utf8", windowsHide:true });
assert.equal(checkout.status, 0, checkout.stderr);
const checkoutCommit = checkout.stdout.trim();
assert.match(checkoutCommit, /^[a-f0-9]{40}$/);
const shots = resolve(process.argv[2] ?? join(root, "docs/screenshots/sdb-46"));
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/debug/spelaion-desktop.exe");
const cli = process.env.SPELAION_E2E_CLI ?? join(root, "target/debug/spelaiondb.exe");
const profile = mkdtempSync(join(tmpdir(), "spelaion-plans-"));
const database = `spelaion_plan46_after_${randomUUID().replaceAll("-", "")}`;
assert.match(database, /^spelaion_plan46_after_[a-f0-9]{32}$/);
const container = spawnSync("docker", ["compose", "-f", resolve(root, "../../testing/docker/compose.yaml"), "ps", "-q", "postgres"], { encoding: "utf8", windowsHide: true });
// Worktrees and normal checkouts share the compose project at the main repository.
let containerId = container.stdout.trim();
if (container.status !== 0 || !/^[a-f0-9]{12,64}$/.test(containerId)) {
  const fallback = spawnSync("docker", ["compose", "-f", join(root, "testing/docker/compose.yaml"), "ps", "-q", "postgres"], { encoding: "utf8", windowsHide: true });
  assert.equal(fallback.status, 0, fallback.stderr); containerId = fallback.stdout.trim();
}
assert.match(containerId, /^[a-f0-9]{12,64}$/);
// MySQL and MariaDB get their own disposable database too, with the plan fixtures' data.
const mysqlDatabase = `spelaion_plan47_${randomUUID().replaceAll("-", "")}`;
assert.match(mysqlDatabase, /^spelaion_plan47_[a-f0-9]{32}$/);
function serviceContainer(service) {
  for (const file of [resolve(root, "../../testing/docker/compose.yaml"), join(root, "testing/docker/compose.yaml")]) {
    const found = spawnSync("docker", ["compose", "-f", file, "ps", "-q", service], { encoding: "utf8", windowsHide: true });
    const id = found.stdout.trim();
    if (found.status === 0 && /^[a-f0-9]{12,64}$/.test(id)) return id;
  }
  throw new Error(`no ${service} container`);
}
const mysqlServers = [
  { service: "mysql", client: "mysql", name: "Plan fixture (MySQL)", engine: "mysql", port: 13306 },
  { service: "mariadb", client: "mariadb", name: "Plan fixture (MariaDB)", engine: "mariadb", port: 13307 },
].map(server => ({ ...server, container: serviceContainer(server.service), created: false }));
function mysqlSql(server, text, db) {
  const result = spawnSync("docker", ["exec", "-i", server.container, server.client, "-uroot", "-pspelaion-root", "--batch", "--raw", "--skip-column-names", ...(db ? [db] : [])], { input: text, encoding: "utf8", windowsHide: true });
  assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}
function sql(text, db = database) {
  const result = spawnSync("docker", ["exec", "-i", containerId, "psql", "-X", "-q", "-t", "-A", "-v", "ON_ERROR_STOP=1", "-U", "spelaion", "-d", db], { input: text, encoding: "utf8", windowsHide: true });
  assert.equal(result.status, 0, result.stderr); return result.stdout.trim();
}
mkdirSync(shots, { recursive: true }); mkdirSync(join(profile, "connections"));
for (const [id, readOnly] of [["plan-fixture", false], ["readonly-plan", true]]) writeFileSync(join(profile, "connections", `${id}.toml`), `schema_version=1\nname="${readOnly ? "Read-only plan" : "Plan fixture (PostgreSQL)"}"\nengine="postgres"\nenv="dev"\nhost="127.0.0.1"\nport=15432\ndatabase="${database}"\nuser="spelaion"\npassword={secret="env:SPELAION_PLAN_PASSWORD"}\nread_only=${readOnly}\n[session]\nschema="plan_fixture"\nsetup="SET search_path TO plan_fixture, public; SELECT * FROM plan_fixture.missing_setup_relation;"\nignore_setup_errors=true\n[tls]\nmode="require"\n`);
for (const server of mysqlServers) writeFileSync(join(profile, "connections", `plan-${server.engine}.toml`), `schema_version=1\nname="${server.name}"\nengine="${server.engine}"\nenv="dev"\nhost="127.0.0.1"\nport=${server.port}\ndatabase="${mysqlDatabase}"\nuser="spelaion"\npassword={secret="env:SPELAION_PLAN_PASSWORD"}\n[tls]\nmode="require"\n`);
const portServer = createServer(); portServer.listen(0, "127.0.0.1"); await once(portServer, "listening");
const port = portServer.address().port; await new Promise(done => portServer.close(done));
const query = "SELECT c.region, count(*) AS orders,\n       sum(o.amount) AS revenue\nFROM plan_fixture.orders AS o\nJOIN plan_fixture.customers AS c\n  ON c.id = o.customer_id\nWHERE o.amount > 900\nGROUP BY c.region;";
writeFileSync(join(shots, "query.sql"), query + "\n");
const provenance = { capturedAtUtc: new Date().toISOString(), checkoutCommit, executableSha256: createHash("sha256").update(readFileSync(exe)).digest("hex"), query: "query.sql", setup: "crates/spelaion-core/tests/fixtures/plans/postgres/setup.sql", sessionSetup: "plan_fixture search_path; an intentionally missing setup relation is skipped and shown until dismissed", screenshots: [], success: false, databaseDropped: false, profileRemoved: false };
let app, browser, page, created = false;
async function start(args = []) {
  app = spawn(exe, args, { cwd: profile, env: { ...process.env, SPELAION_USER_DIR: profile, SPELAION_PLAN_PASSWORD: "spelaion", SPELAION_NO_UPDATES: "1", SPELAION_MCP_CLIENTS_HOME: join(profile, "clients"), KUBECONFIG: join(profile, "none"), WEBVIEW2_USER_DATA_FOLDER: join(profile, "webview"), WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}` }, windowsHide: true, stdio: "inherit" });
  for (let n = 0; n < 90; n++) { try { browser = await chromium.connectOverCDP(`http://127.0.0.1:${port}`); break; } catch { await new Promise(done => setTimeout(done, 500)); } }
  assert.ok(browser); page = browser.contexts()[0].pages()[0]; page.setDefaultTimeout(30_000);
  await page.locator(".activity-bar .settings-button").waitFor();
}
async function stop() {
  await browser?.close(); browser = undefined;
  if (app?.pid && app.exitCode === null) {
    if (process.platform === "win32") spawnSync("taskkill", ["/PID", String(app.pid), "/T", "/F"], { windowsHide: true, stdio: "ignore" }); else app.kill();
    if (app.exitCode === null) await Promise.race([once(app, "exit"), new Promise(done => setTimeout(done, 2000))]);
  }
}
async function replace(panel, text) { await panel.locator(".cm-content").click(); await page.keyboard.press("Control+A"); await page.keyboard.insertText(text); }
async function ordinaryResult(panel) {
  // Tauri's invoke is nonwritable. Observe its arguments through the existing native CDP
  // debugger pattern (library.mjs), without changing the bridge, payload or responses.
  const debug = await page.context().newCDPSession(page);
  await debug.send("Debugger.enable");
  const { result: invoke } = await debug.send("Runtime.evaluate", { expression:"window.__TAURI_INTERNALS__.invoke" });
  let resolveHandle, timer; const calls = [];
  const captured = new Promise(resolve => { resolveHandle = resolve; });
  const observer = async ({ callFrames }) => {
    try {
      const { result, exceptionDetails } = await debug.send("Debugger.evaluateOnCallFrame", { callFrameId:callFrames[0].callFrameId, expression:"({command:cmd,result:payload.result ?? null})", returnByValue:true });
      assert.equal(exceptionDetails, undefined, "real transport arguments are observable in their call frame");
      calls.push(result.value.command);
      if (["fetch_window", "editable_query_table"].includes(result.value.command) && Number.isInteger(result.value.result)) resolveHandle(result.value.result);
    } finally { await debug.send("Debugger.resume").catch(() => undefined); }
  };
  debug.on("Debugger.paused", observer);
  const { breakpointId } = await debug.send("Debugger.setBreakpointOnFunctionCall", { objectId:invoke.objectId });
  try {
    await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("workspace_info"));
    assert.ok(calls.includes("workspace_info"), "the real native transport observation works before running SQL");
    await panel.locator(".cm-content").click(); await page.keyboard.press("Control+Shift+Enter");
    await panel.locator(".statusline", { hasText: "8 rows" }).waitFor();
    const id = await Promise.race([captured, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(`ordinary result not observed; actual command names: ${calls.join(", ")}`)), 30_000); })]);
    console.log(`Observed ordinary result handle ${id} through real native calls.`);
    return id;
  } finally {
    clearTimeout(timer);
    await debug.send("Debugger.removeBreakpoint", { breakpointId });
    debug.off("Debugger.paused", observer);
    await debug.send("Debugger.disable"); await debug.detach();
    await page.evaluate(() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done))));
  }
}
async function resultState(id) {
  return page.evaluate(async result => {
    try { await window.__TAURI_INTERNALS__.invoke("fetch_window", { result, start:0, len:1 }); return "valid"; }
    catch (error) { return String(error); }
  }, id);
}
async function capture(name, theme, width, height) {
  await page.evaluate(() => new Promise(done => requestAnimationFrame(() => requestAnimationFrame(done))));
  await page.screenshot({ path: join(shots, name) });
  provenance.screenshots.push({ name, theme, width, height, sha256: createHash("sha256").update(readFileSync(join(shots, name))).digest("hex") });
}
try {
  sql(`CREATE DATABASE "${database}"`, "postgres"); created = true;
  sql(readFileSync(join(root, "crates/spelaion-core/tests/fixtures/plans/postgres/setup.sql"), "utf8"));
  for (const server of mysqlServers) {
    mysqlSql(server, `CREATE DATABASE \`${mysqlDatabase}\`; GRANT ALL ON \`${mysqlDatabase}\`.* TO 'spelaion'@'%';`); server.created = true;
    mysqlSql(server, readFileSync(join(root, "crates/spelaion-core/tests/fixtures/plans/mysql/setup.sql"), "utf8"), mysqlDatabase);
  }
  await start();
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("set_language", { language: "en" }));
  await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("set_theme", { theme: "light" }));
  await page.reload(); await page.locator(".activity-bar .settings-button").waitFor();
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.locator(".connection-row").filter({ hasText: "Plan fixture (PostgreSQL)" }).locator(":scope > .label").dblclick();
  let panel = page.locator(".tab-panel:not([hidden])");
  await panel.locator(".status-connecting").waitFor({ state: "detached" });
  await replace(panel, query);
  const firstResult = await ordinaryResult(panel);
  assert.equal(await resultState(firstResult), "valid");
  await replace(panel, "SET search_path TO public;");
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  await panel.getByRole("alert").filter({ hasText: "Could not explain query" }).waitFor();
  assert.equal(await resultState(firstResult), "valid", "preflight refusal preserves the previous core result");
  await replace(panel, query);
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  let plan = panel.getByRole("region", { name: "Execution plan", exact: true });
  await plan.getByText("Estimates only", { exact: false }).waitFor();
  assert.equal(await resultState(firstResult), "result freed", "accepted Explain releases the ordinary result's Arrow handle");
  const picker = panel.getByRole("combobox", { name: "Schema", exact: true });
  await page.waitForFunction(() => document.querySelector('.tab-panel:not([hidden]) select[name="schema"]')?.value === "plan_fixture");
  await panel.locator(".setup-skipped").waitFor();
  assert.match(await panel.locator(".setup-skipped").innerText(), /missing_setup_relation/);
  await page.setViewportSize({ width: 900, height: 600 });
  await page.keyboard.press("Control+Alt+m");
  assert.equal(await picker.inputValue(), "plan_fixture");
  assert.ok(await panel.locator(".setup-skipped").isVisible());
  const heading = await plan.locator(".plan-heading").boundingBox();
  assert.ok(heading && heading.y + heading.height <= 600, "plan heading stays reachable with schema picker and setup warning at minimum size");
  await panel.locator(".setup-skipped").getByRole("button").click();
  await panel.locator(".setup-skipped").waitFor({ state: "detached" });
  await page.keyboard.press("Control+Alt+m");
  await page.setViewportSize({ width: 1400, height: 900 });
  await plan.getByLabel("ANALYZE", { exact: true }).check();
  await plan.getByRole("button", { name: "Back to results", exact:true }).click();
  const secondResult = await ordinaryResult(panel);
  assert.equal(await resultState(secondResult), "valid");
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  const warning = page.getByRole("dialog"); await warning.getByText("ANALYZE executes your query", { exact: true }).waitFor();
  await capture("after-analyze-warning-light-1400.png", "light", 1400, 900);
  await warning.getByRole("button", { name: "Cancel", exact: true }).click();
  assert.equal(await resultState(secondResult), "valid", "cancelled ANALYZE warning preserves the previous core result");
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  await warning.getByRole("button", { name: "Execute and analyze", exact: true }).click();
  await plan.getByText("Actual execution", { exact: false }).waitFor();
  assert.equal(await resultState(secondResult), "result freed", "accepted ANALYZE releases the previous Arrow handle");
  assert.equal(await picker.inputValue(), "plan_fixture", "EXPLAIN retains the selected source schema");
  for (const theme of ["light", "dark"]) {
    if (theme === "dark") {
      await page.locator(".activity-bar .settings-button").click(); const settings = page.locator(".settings-dialog");
      await settings.locator(".settings-nav button").first().click(); await settings.locator('input[name="theme"][value="dark"]').click();
      await page.waitForFunction(() => document.documentElement.dataset.theme === "dark"); await settings.locator(".wizard-header button").click();
    }
    for (const [width, height] of [[1400, 900], [900, 600]]) {
      await page.setViewportSize({ width, height });
      // Maximize through the existing keyboard command so the plan remains usable at 900×600.
      for (const [view, label] of [["overview", "Overview"], ["tree", "Tree & table"], ["graph", "Graph"], ["raw", "Raw output"]]) {
        const hidden = await panel.locator(".editor-pane").isHidden();
        if ((width === 900 || view === "graph") !== hidden) await page.keyboard.press("Control+Alt+m");
        await plan.getByRole("button", { name: label, exact: true }).click();
        if (view === "tree") { await plan.locator(".plan-table tbody tr").first().waitFor(); await plan.locator(".plan-node-name").first().click(); assert.ok(await plan.locator(".plan-table tbody tr").count() >= 4); assert.match(await plan.locator(".plan-table").innerText(), /plan_fixture\.orders/); }
        if (view === "graph") { await plan.locator(".plan-graph-node").first().waitFor(); assert.ok(await plan.locator(".plan-graph-node").count() >= 4); }
        if (view === "raw") { await plan.locator(".plan-raw pre").filter({ hasText: '"Actual Rows"' }).waitFor(); }
        if (view === "overview") await plan.locator(".plan-overview-numbers").waitFor();
        await capture(`after-plan-${view}-${theme}-${width}.png`, theme, width, height);
      }
    }
  }
  await plan.getByRole("button", { name:"Overview", exact:true }).click();
  await plan.getByRole("button", { name:/Inspect findings/ }).click();
  await plan.locator(".plan-finding-count").first().click();
  assert.ok(await plan.locator(".plan-inspector .plan-findings li").count() > 0);
  await plan.locator(".plan-inspector").evaluate(element => { element.scrollTop = element.scrollHeight; });
  await capture("after-findings-dark-900.png", "dark", 900, 600);
  await plan.getByLabel("With findings only", { exact:true }).uncheck();
  const rawFile = join(profile, "saved-plan.json"), csvFile = join(profile, "plan.csv");
  await page.evaluate(path => { window.__spelaionSavePath = path; }, rawFile);
  await plan.getByRole("button", { name: "Save raw plan…", exact: true }).click();
  await plan.getByRole("status").filter({ hasText: "Saved to" }).waitFor();
  const saved = JSON.parse(readFileSync(rawFile, "utf8")); assert.ok(saved[0].Plan);
  await page.evaluate(path => { window.__spelaionSavePath = path; }, csvFile);
  await plan.getByRole("button", { name: "Export table as CSV…", exact: true }).click();
  await plan.getByRole("status").filter({ hasText: csvFile }).waitFor();
  assert.match(readFileSync(csvFile, "utf8"), /Self cost/);
  const cliResult = spawnSync(cli, ["plan", rawFile, "--limit", "1"], { encoding: "utf8", windowsHide: true });
  assert.equal(cliResult.status, 0, cliResult.stderr); const cliPlan = JSON.parse(cliResult.stdout); assert.equal(cliPlan.page.nodes.length, 1); assert.equal(cliPlan.summary.engine, "postgres");
  await page.evaluate(path => { window.__spelaionOpenPath = path; }, rawFile);
  await plan.getByRole("button", { name: "Open saved plan…", exact: true }).click();
  panel = page.locator(".tab-panel:not([hidden])"); plan = panel.getByRole("region", { name: "Execution plan", exact: true });
  await plan.getByText("Actual execution", { exact: false }).waitFor();
  assert.equal(await panel.locator(".cm-content").count(), 0, "saved plans are local viewers, never executable editors");
  await page.setViewportSize({ width: 1400, height: 900 });
  await page.locator(".connection-row").filter({ hasText: "Read-only plan" }).locator(":scope > .label").dblclick();
  panel = page.locator(".tab-panel:not([hidden])"); await panel.locator(".status-connecting").waitFor({ state: "detached" });
  await replace(panel, "SELECT * FROM plan_fixture.customers WHERE id=1");
  await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
  plan = panel.getByRole("region", { name: "Execution plan", exact: true }); await plan.getByText("Estimates only", { exact: false }).waitFor();
  await panel.locator(".setup-skipped").getByRole("button").click();
  assert.equal(await plan.getByLabel("ANALYZE", { exact: true }).isDisabled(), true);
  const before = sql("SELECT count(*) FROM plan_fixture.customers WHERE region='changed'");
  // Invoke the same public command with the session from the real connect command; the
  // core must reject even a caller bypassing disabled React controls.
  const connections = await page.evaluate(() => window.__TAURI_INTERNALS__.invoke("list_connections"));
  const readId = connections.find(item => item.name === "Read-only plan").id;
  const denied = await page.evaluate(async id => {
    const session = "plan-readonly-adversarial";
    const outcome = await window.__TAURI_INTERNALS__.invoke("connect", { session, connection: id, password:null, remember:false, editor:true });
    if (outcome.kind !== "connected") throw new Error(JSON.stringify(outcome));
    try { await window.__TAURI_INTERNALS__.invoke("explain_preflight", { session, sql: "UPDATE plan_fixture.customers SET region='changed'", options: { analyze:true,buffers:true,analyzeConfirmed:true,confirmed:true,timeoutSeconds:null } }); return false; }
    catch (error) { return String(error).includes("read-only"); }
    finally { await window.__TAURI_INTERNALS__.invoke("disconnect", { session }); }
  }, readId);
  assert.ok(denied, "read-only UPDATE ANALYZE is rejected in core, independently of UI");
  assert.equal(sql("SELECT count(*) FROM plan_fixture.customers WHERE region='changed'"), before);
  await capture("after-readonly-dark-1400.png", "dark", 1400, 900);
  // MySQL and MariaDB: the same viewer over the same core tree, with each engine's own SQL.
  const mysqlQuery = "SELECT c.country, count(*) AS orders, sum(o.total) AS revenue\nFROM orders AS o JOIN customers AS c ON c.id = o.customer_id\nWHERE o.status = 'paid'\nGROUP BY c.country\nORDER BY revenue DESC;";
  let mysqlText = null;
  for (const server of mysqlServers) {
    await page.locator(".connection-row").filter({ hasText: server.name }).locator(":scope > .label").dblclick();
    panel = page.locator(".tab-panel:not([hidden])"); await panel.locator(".status-connecting").waitFor({ state: "detached" });
    await replace(panel, mysqlQuery);
    await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
    plan = panel.getByRole("region", { name: "Execution plan", exact: true });
    await plan.getByText("Estimates only", { exact: false }).waitFor();
    assert.equal(await plan.getByLabel("BUFFERS", { exact: true }).isDisabled(), true, `${server.engine} has no BUFFERS`);
    assert.equal(await plan.getByLabel("BUFFERS", { exact: true }).isChecked(), false, `${server.engine} does not show BUFFERS as applied`);
    await plan.getByRole("button", { name: "Tree & table", exact: true }).click();
    await plan.locator(".plan-table tbody tr").first().waitFor();
    assert.match(await plan.locator(".plan-table").innerText(), /Nested loop/);
    if (server.engine === "mysql") await capture("sdb47-mysql-estimated-tree-dark-1400.png", "dark", 1400, 900);
    await plan.getByLabel("ANALYZE", { exact: true }).check();
    await panel.locator(".editor-actions").getByRole("button", { name: "Explain query", exact: true }).click();
    await page.getByRole("dialog").getByRole("button", { name: "Execute and analyze", exact: true }).click();
    await plan.getByText("Actual execution", { exact: false }).waitFor();
    await plan.getByRole("button", { name: "Tree & table", exact: true }).click();
    await plan.locator(".plan-table tbody tr").first().waitFor();
    const lookup = server.engine === "mysql" ? "Single-row index lookup" : "Index lookup";
    await plan.locator(".plan-node-name").filter({ hasText: lookup }).first().click();
    const inspector = plan.locator(".plan-inspector");
    await inspector.getByRole("heading", { name: "Engine detail", exact: true }).or(inspector.getByRole("heading", { name: "Filter", exact: true })).first().waitFor();
    if (server.engine === "mysql") assert.match(await inspector.innerText(), /Single-row index lookup on c using PRIMARY/);
    await capture(`sdb47-${server.engine}-analyze-tree-dark-1400.png`, "dark", 1400, 900);
    await plan.getByRole("button", { name: "Overview", exact: true }).click();
    await plan.locator(".plan-capabilities summary").click();
    assert.match(await plan.locator(".plan-capabilities").innerText(), server.engine === "mysql" ? /MySQL/ : /MariaDB/);
    await plan.locator(".plan-capabilities").scrollIntoViewIfNeeded();
    await capture(`sdb47-${server.engine}-analyze-overview-dark-1400.png`, "dark", 1400, 900);
    if (server.engine === "mysql") {
      const textFile = join(profile, "saved-mysql-plan.txt");
      await page.evaluate(path => { window.__spelaionSavePath = path; }, textFile);
      await plan.getByRole("button", { name: "Save raw plan…", exact: true }).click();
      await plan.getByRole("status").filter({ hasText: "Saved to" }).waitFor();
      mysqlText = readFileSync(textFile, "utf8"); assert.match(mysqlText, /^-> /);
      const cliMysql = spawnSync(cli, ["plan", textFile, "--limit", "100"], { encoding: "utf8", windowsHide: true });
      assert.equal(cliMysql.status, 0, cliMysql.stderr); const parsed = JSON.parse(cliMysql.stdout);
      assert.equal(parsed.summary.engine, "mysql"); assert.equal(parsed.summary.analyzed, true); assert.equal(parsed.summary.rawFormat, "text");
    }
  }
  assert.ok(mysqlText, "a MySQL ANALYZE plan was saved as text");
  await stop();
  await start(["--plan", rawFile]);
  await page.getByRole("region", { name:"Execution plan", exact:true }).getByText("Actual execution", { exact:false }).waitFor();
  assert.equal(sql("SELECT count(*) FROM plan_fixture.customers"), "1000");
  provenance.success = true;
  console.log("Plans passed: selected-session PG, warning, tree/table, graph, raw, save/open, CSV, CLI path, readonly guard, MySQL and MariaDB estimated/ANALYZE plans and native screenshots.");
} catch (error) { await page?.screenshot({ path: join(shots, "failure.png") }).catch(() => undefined); throw error; }
finally {
  await stop();
  if (created) { sql(`DROP DATABASE "${database}" WITH (FORCE)`, "postgres"); provenance.databaseDropped = true; }
  for (const server of mysqlServers) if (server.created) mysqlSql(server, `DROP DATABASE IF EXISTS \`${mysqlDatabase}\``);
  try { rmSync(profile, { recursive:true, force:true }); provenance.profileRemoved = true; } catch { console.log(`Owned profile retained: ${profile}`); }
  writeFileSync(join(shots, "capture-provenance.json"), JSON.stringify(provenance, null, 2) + "\n");
}
