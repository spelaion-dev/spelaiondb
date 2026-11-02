// Smoke test of the real app: starts it with the WebView2 debugging port open and drives it
// with Playwright (CDP). It uses examples/demo and a temporary personal directory, so it does
// not touch the user's configuration or keychain.
//
// Requirements: the app built (npx tauri build --no-bundle), examples/demo/make-shop-db.py
// run and, for the PostgreSQL steps, testing/docker up.
//   node e2e/smoke.mjs [screenshots-dir]

import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import { once } from "node:events";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { chromium } from "playwright-core";

const root = resolve(import.meta.dirname, "../../..");
const exe = join(root, "target/release/spelunk-desktop" + (process.platform === "win32" ? ".exe" : ""));
const demo = join(root, "examples/demo");
const shots = resolve(process.argv[2] ?? join(import.meta.dirname, "screenshots"));
const userDir = mkdtempSync(join(tmpdir(), "spelunk-e2e-"));
const port = 9333;
mkdirSync(shots, { recursive: true });

function startApp() {
  return spawn(exe, [demo], {
    env: {
      ...process.env,
      SPELUNK_USER_DIR: userDir,
      WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${port}`,
    },
    stdio: "inherit",
  });
}

let app = startApp();

async function stopApp() {
  if (app.exitCode !== null || app.signalCode !== null) return;
  const exited = once(app, "exit");
  app.kill();
  await exited;
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
let browser = await connect();
try {
  let page = browser.contexts()[0].pages()[0];
  page.setDefaultTimeout(30_000);
  const shot = (name) => page.screenshot({ path: join(shots, `${name}.png`) });
  const step = (text) => console.log(`· ${text}`);

  async function newQuery(connection, sql) {
    await page.locator(`.connection-row:has-text("${connection}") button[title="New query"]`).click();
    const panel = page.locator(".tab-panel:not([hidden])");
    await panel.locator(".cm-content").click();
    // insertText does not trigger autocompletion (Enter would accept a suggestion).
    await page.keyboard.insertText(sql);
    return panel;
  }
  async function run(panel) {
    await panel.locator(".cm-content").click();
    await page.keyboard.press("Control+Enter");
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
  // Opens a settings category in its own tab, runs `action` and goes back to the tab that
  // was active, closing the settings tab, so the rest of the test sees the same layout.
  const CATEGORY = { appearance: 1, language: 2, history: 3 };
  async function withSettings(category, action) {
    const tabs = page.locator(".tabbar [role=tab]");
    let selected = -1;
    for (let i = 0; i < (await tabs.count()); i++) {
      if ((await tabs.nth(i).getAttribute("aria-selected")) === "true") selected = i;
    }
    await page.locator(".sidebar-footer .settings-button").click();
    await page.locator(`.settings-nav button:nth-child(${CATEGORY[category]})`).click();
    await action(page.locator(".settings"));
    if (selected >= 0) await tabs.nth(selected).click();
    await page.locator(".settings-tab .close").click();
    await page.locator(".settings-tab").waitFor({ state: "detached" });
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
  await page.locator(".sidebar-footer .settings-button").waitFor();
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
  await setLanguage("en");
  await page.getByText("Shop (SQLite)").waitFor();
  await page.getByText("is not trusted").waitFor();
  await shot("01-start");

  step("settings: theme in its own tab, Ctrl+, shortcut");
  await page.keyboard.press("Control+,");
  await page.locator(".settings-tab[aria-selected=true]").waitFor();
  await page.getByRole("heading", { name: "Appearance" }).waitFor();
  await page.locator(".settings-tab .close").click();
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
  await run(panel);
  await panel.locator(".statusline", { hasText: "6 rows" }).waitFor();
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
  });
  const copied = async (count) => {
    await page.waitForFunction((n) => window.__copied.length === n, count);
    return page.evaluate(() => window.__copied[window.__copied.length - 1]);
  };
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
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).waitFor();
  await shot("03b-copy-menu");
  await panel.getByRole("menuitem", { name: "Copy as JSON" }).click();
  const json = JSON.parse(await copied(3));
  assert.equal(json.length, 3);
  assert.equal(typeof json[0].orders, "number");
  assert.ok(json[0].country);
  await panel.getByRole("status").filter({ hasText: "Copied 3 rows × 2 columns" }).waitFor();

  step("SQLite: 50,000 rows and scrolling");
  panel = await newQuery("Shop (SQLite)", "select * from orders;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "50,000 rows" }).waitFor();
  await panel.locator(".grid-scroller").evaluate((el) => (el.scrollTop = 26 * 25_000));
  await page.waitForTimeout(500);
  await shot("04-grid-scroll");

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
  await waitForHistoryCount(3);
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
  await waitForHistoryCount(3);
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
  await waitForHistoryCount(3);
  await history.getByRole("textbox", { name: "Search query history" }).fill("private");
  await history.getByText("No matching queries.", { exact: true }).waitFor();
  await shot("05b-history-disabled");
  await page.getByRole("button", { name: "Close query history" }).click();

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
  await page.getByLabel("Password", { exact: true }).fill("spelunk");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await panel.locator(".statusline", { hasText: "1,000 rows" }).waitFor();
  await shot("07-postgres");

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
    await run(panel);
    await page.getByRole("dialog").getByText(`Password for ${connection}`).waitFor();
    await page.getByLabel("Password", { exact: true }).fill("spelunk");
    await page.getByRole("button", { name: "Connect", exact: true }).click();
    await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
    assert.match(await panel.locator(".status-connected").innerText(), version);
    if (connection.startsWith("MySQL")) await shot("07b-mysql");
  }
  // The explorer lists MySQL databases and their tables.
  // Its own session ("ask every time"): the password is asked again.
  await page.locator('.connection-row:has-text("MySQL (Docker)") .twisty').click();
  await page.getByLabel("Password", { exact: true }).fill("spelunk");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  await page.getByRole("button", { name: "Expand or collapse schema spelunk" }).waitFor();
  assert.equal(await page.locator('.connection-row:has-text("MySQL (Docker)") .engine').innerText(), "MY");

  step("PostgreSQL behind two bastions with TLS verify-full");
  panel = await newQuery(
    "Postgres behind bastions",
    "select inet_server_addr()::text as server, ssl, version, cipher\nfrom pg_stat_ssl\nwhere pid = pg_backend_pid();",
  );
  await run(panel);
  // First the password (the keychain does not have it yet, so it offers to remember it); then,
  // when the tunnel opens, the fingerprint of each bastion.
  await page.getByRole("dialog").getByText("Password for Postgres behind bastions").waitFor();
  await page.getByText("Remember in the system keychain").waitFor();
  await page.getByLabel("Password", { exact: true }).fill("spelunk");
  await page.getByRole("button", { name: "Connect", exact: true }).click();
  for (let hop = 1; hop <= 2; hop++) {
    await page.getByRole("dialog").getByText(`(hop ${hop})`).waitFor();
    if (hop === 1) await shot("08-ssh-fingerprint");
    await page.getByRole("button", { name: "Trust and remember" }).click();
  }
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();
  await panel.getByText("PRODUCTION").waitFor();
  await shot("09-bastions");

  step("add connection: catalog by category, test and save (personal, nothing in the repo)");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  let wizard = page.getByRole("dialog", { name: "Add connection" });
  await wizard.getByRole("heading", { name: "Relational" }).waitFor();
  await wizard.getByRole("heading", { name: "NoSQL" }).waitFor();
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "MySQL" }).isDisabled()));
  assert.ok(!(await wizard.locator(".engine-card", { hasText: "MariaDB" }).isDisabled()));
  assert.ok(await wizard.locator(".engine-card", { hasText: "SQL Server" }).isDisabled());
  await wizard.locator(".engine-card", { hasText: "SQL Server" }).getByText("Coming soon").waitFor();
  await shot("12a-add-connection-catalog");
  await wizard.getByRole("searchbox").fill("lite");
  assert.equal(await wizard.locator(".engine-card").count(), 1);
  await wizard.locator(".engine-card", { hasText: "SQLite" }).click();
  wizard = page.locator(".modal.wizard");
  await wizard.getByRole("heading", { name: "New SQLite connection" }).waitFor();
  // SQLite has no SSL or SSH tabs.
  assert.equal(await wizard.getByRole("tab").count(), 2);
  await wizard.getByLabel("Name", { exact: true }).fill("Shop copy");
  await wizard.getByLabel("File", { exact: true }).fill(join(demo, "data", "shop.db"));
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 3\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Shop copy" }).waitFor();
  panel = await newQuery("Shop copy", "select count(*) as n from product;");
  await run(panel);
  await panel.locator(".statusline", { hasText: "1 row" }).waitFor();

  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard");
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  assert.deepEqual(await wizard.getByRole("tab").allTextContents(), ["Main", "SSL", "SSH", "Options"]);
  await wizard.getByLabel("Name", { exact: true }).fill("Docker PG (wizard)");
  await wizard.getByLabel("Host", { exact: true }).fill("127.0.0.1");
  await wizard.getByLabel("Port", { exact: true }).fill("15432");
  await wizard.getByLabel("Database", { exact: true }).fill("spelunk");
  await wizard.getByLabel("User", { exact: true }).fill("spelunk");
  // Not saving the password means "ask every time": the typed one is only used for the test
  // and never stored (not even in the keychain).
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: "Enter the password" }).waitFor();
  await wizard.getByLabel("Password", { exact: true }).fill("wrong");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: "password authentication failed" }).waitFor();
  await wizard.getByLabel("Password", { exact: true }).fill("spelunk");
  await shot("12b-add-connection-main");
  await wizard.getByRole("tab", { name: "SSL" }).click();
  await wizard.getByLabel("TLS", { exact: true }).selectOption("require");
  await wizard.getByRole("tab", { name: "SSH" }).click();
  // The demo defines one tunnel; leaving the box unchecked connects directly.
  assert.equal(await wizard.getByRole("checkbox", { name: "Connect through an SSH tunnel" }).isChecked(), false);
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByLabel("Folder", { exact: true }).fill("Wizard");
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await shot("12c-add-connection-options");
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Docker PG (wizard)" }).waitFor();
  const saved = readFileSync(join(userDir, "connections", "wizard", "docker-pg-wizard.toml"), "utf8");
  assert.match(saved, /secret = "prompt"/);
  assert.match(saved, /mode = "require"/);
  assert.doesNotMatch(saved, /wrong|password = "/);
  assert.ok(!existsSync(join(demo, ".spelunk", "connections", "shop-copy.toml")), "personal connections stay out of the repository");

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
  await wizard.getByLabel("Password", { exact: true }).fill("spelunk");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await shot("13a-edit-connection");
  await wizard.getByRole("button", { name: "Save changes" }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Docker PG (edited)" }).waitFor();
  const edited = readFileSync(join(userDir, "connections", "wizard", "docker-pg-wizard.toml"), "utf8");
  assert.match(edited, /name = "Docker PG \(edited\)"/);
  assert.doesNotMatch(edited, /\[tls\]|spelunk"\s*\n\s*password = "/);

  step("SSH tunnel editor: two hops with keys, tested, used by a new connection");
  await page.getByRole("button", { name: "Add connection", exact: true }).first().click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.locator(".engine-card", { hasText: "PostgreSQL" }).click();
  await wizard.getByLabel("Name", { exact: true }).fill("PG through wizard tunnel");
  await wizard.getByLabel("Host", { exact: true }).fill("postgres");
  await wizard.getByLabel("Database", { exact: true }).fill("spelunk");
  await wizard.getByLabel("User", { exact: true }).fill("spelunk");
  await wizard.getByRole("checkbox", { name: "Save password" }).uncheck();
  await wizard.getByLabel("Password", { exact: true }).fill("spelunk");
  await wizard.getByRole("tab", { name: "Options" }).click();
  await wizard.getByRole("radio", { name: "Personal", exact: true }).check();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "New tunnel…" }).click();
  const editor = page.getByRole("dialog", { name: "New SSH tunnel" });
  const key = join(root, "testing", "docker", "keys", "id_ed25519");
  await editor.getByLabel("Tunnel name").fill("Wizard bastions");
  await editor.getByRole("radio", { name: "Personal", exact: true }).check();
  await editor.getByLabel("Hop 1 Host").fill("127.0.0.1");
  await editor.getByLabel("Hop 1 Port").fill("2221");
  await editor.getByLabel("Hop 1 User").fill("spelunk");
  await editor.getByLabel("Hop 1 Authentication").selectOption("key");
  await editor.getByLabel("Hop 1 Private key file").fill(key);
  await editor.getByRole("button", { name: "+ Add hop" }).click();
  await editor.getByLabel("Hop 2 Host").fill("bastion2");
  await editor.getByLabel("Hop 2 User").fill("spelunk");
  await editor.getByLabel("Hop 2 Authentication").selectOption("key");
  await editor.getByLabel("Hop 2 Private key file").fill(key);
  await editor.getByRole("button", { name: "Test tunnel" }).click();
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
  assert.equal(await wizard.getByRole("checkbox", { name: "Connect through an SSH tunnel" }).isChecked(), true);
  assert.equal(await wizard.getByLabel("SSH tunnel", { exact: true }).inputValue(), "wizard-bastions");
  await wizard.getByRole("button", { name: "Test connection" }).click();
  await wizard.locator(".test-result", { hasText: /Connected · 17\./ }).waitFor();
  await wizard.getByRole("button", { name: "Save", exact: true }).click();
  await wizard.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "PG through wizard tunnel" }).waitFor();
  const tunnelFile = readFileSync(join(userDir, "tunnels", "wizard-bastions.toml"), "utf8");
  assert.equal(tunnelFile.match(/\[\[hops\]\]/g)?.length, 2, tunnelFile);
  assert.match(tunnelFile, /port = 2221/);
  assert.doesNotMatch(tunnelFile, /password/);

  // While a connection uses it, the tunnel cannot be deleted.
  await (await connectionMenu("PG through wizard tunnel")).getByRole("button", { name: "Edit…" }).click();
  wizard = page.locator(".modal.wizard").first();
  await wizard.getByRole("tab", { name: "SSH" }).click();
  await wizard.getByRole("button", { name: "Edit tunnel…" }).click();
  const editTunnel = page.getByRole("dialog", { name: "Edit tunnel wizard-bastions" });
  assert.ok(await editTunnel.getByLabel("Tunnel name").isDisabled());
  assert.equal(await editTunnel.getByLabel("Hop 2 Host").inputValue(), "bastion2");
  await editTunnel.getByRole("button", { name: "Delete tunnel" }).click();
  await editTunnel.getByRole("button", { name: "Click again to delete" }).click();
  await editTunnel.locator(".test-result", { hasText: "PG through wizard tunnel" }).waitFor();
  await editTunnel.getByRole("button", { name: "Cancel" }).click();
  await editTunnel.waitFor({ state: "detached" });
  await wizard.getByRole("button", { name: "Cancel" }).click();
  await wizard.waitFor({ state: "detached" });
  assert.ok(existsSync(join(userDir, "tunnels", "wizard-bastions.toml")));

  step("delete a connection: confirmation, its tab is left without a connection");
  await (await connectionMenu("Shop copy")).getByRole("button", { name: "Delete…" }).click();
  const confirm = page.getByRole("dialog", { name: "Delete connection?" });
  await confirm.getByText("only this computer is affected").waitFor();
  await shot("13b-delete-connection");
  await confirm.getByRole("button", { name: "Delete", exact: true }).click();
  await confirm.waitFor({ state: "detached" });
  await page.locator(".connection-row", { hasText: "Shop copy" }).waitFor({ state: "detached" });
  assert.ok(!existsSync(join(userDir, "connections", "shop-copy.toml")));

  step("history, theme and per-connection preference survive an app restart");
  await setLanguage("es");
  await setTheme("dark", "dark");
  await browser.close();
  await stopApp();
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
  panel = await newQuery("Shop (SQLite)", "select 'history_resumed_marker' as resumed;");
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

  console.log(`\nOK. Screenshots in ${shots}`);
} catch (e) {
  failed = true;
  console.error("\nFAILED:", e);
  try {
    await browser.contexts()[0].pages()[0].screenshot({ path: join(shots, "failure.png") });
  } catch {}
} finally {
  await browser.close().catch(() => {});
  await stopApp();
  rmSync(userDir, { recursive: true, force: true });
  process.exit(failed ? 1 : 0);
}
