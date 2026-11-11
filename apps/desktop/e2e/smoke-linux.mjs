// Smoke test of the real app on Linux (WebKitGTK). smoke.mjs drives the Windows build over
// WebView2's CDP port, which WebKitGTK does not have; this pass drives the release build through
// tauri-driver and WebKitWebDriver (webdriver.mjs) instead. It covers what differs between the
// two webviews and systems: start, workspace trust, a query on every engine, the real system
// clipboard both ways (no stub), an SSH tunnel and quitting with a transaction open.
// It uses examples/demo and a temporary personal directory: it never reads or writes the user's
// configuration, AI clients, kubeconfig or keychain (the app gets no session bus).
//
// Requirements (AGENTS.md): Linux under Wayland (wl-clipboard) or X11 (xclip); tauri-driver
// (cargo install tauri-driver --locked) and WebKitWebDriver (webkit2gtk-driver / webkitgtk-6.0);
// the release app and CLI built in one command (CONTRIBUTING.md); examples/demo/make-shop-db.py
// run; testing/docker up.
//   node e2e/smoke-linux.mjs [screenshots-dir]

import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { descendants, requestClose } from "./processes.mjs";
import { Session, startDriver } from "./webdriver.mjs";

if (process.platform !== "linux") {
  console.error("smoke-linux.mjs drives the Linux build; on Windows run node e2e/smoke.mjs");
  process.exit(2);
}

const root = resolve(import.meta.dirname, "../../..");
const exe = process.env.SPELAION_E2E_EXE ?? join(root, "target/release/spelaion-desktop");
const demo = join(root, "examples/demo");
const keys = join(root, "testing/docker/keys");
const shots = resolve(process.argv[2] ?? join(import.meta.dirname, "screenshots", "linux"));
mkdirSync(shots, { recursive: true });
for (const [path, hint] of [
  [exe, "in apps/desktop, run npx tauri build --no-bundle -- -p spelaion-desktop -p spelaion-cli"],
  [join(demo, "data/shop.db"), "run python examples/demo/make-shop-db.py"],
  [join(keys, "ca.crt"), "start testing/docker (docker compose -f testing/docker/compose.yaml up -d --wait)"],
]) {
  if (!existsSync(path)) {
    console.error(`${path} is missing: ${hint}`);
    process.exit(2);
  }
}

// The real clipboard, through another program, as a user's other apps would use it.
const wayland = !!process.env.WAYLAND_DISPLAY;
const clipboard = {
  // wl-copy and xclip stay in the background to serve the clipboard: they must not keep this
  // process's output open, or whatever reads it never sees the end.
  set(text) {
    if (wayland) execFileSync("wl-copy", [], { input: text, stdio: ["pipe", "ignore", "ignore"] });
    else execFileSync("xclip", ["-selection", "clipboard", "-in", ...(process.env.SMOKE_XCLIP_TARGET ? ["-t", process.env.SMOKE_XCLIP_TARGET] : [])], { input: text, stdio: ["pipe", "ignore", "ignore"] });
  },
  get() {
    return wayland
      ? execFileSync("wl-paste", ["--no-newline"], { encoding: "utf8" })
      : execFileSync("xclip", ["-selection", "clipboard", "-out"], { encoding: "utf8" });
  },
};

const userDir = mkdtempSync(join(tmpdir(), "spelaion-e2e-"));
// Personal connections of this run: SQL Server from testing/docker, and the demo's PostgreSQL
// behind testing/docker's two bastions with its password asked (the demo's own bastion
// connection reads the keychain). Passwords are asked, never stored. (The DuckDB one is made in
// the form.) The tunnel is the demo repository's `docker-bastions`: its relative key paths must
// be read from the repository, not from this personal folder.
mkdirSync(join(userDir, "connections"), { recursive: true });
const personal = {
  "sql-server-docker.toml": `schema_version = 1\nname = "SQL Server (Docker)"\nengine = "sqlserver"\nhost = "127.0.0.1"\nport = 11433\nuser = "sa"\npassword = { secret = "prompt" }\n`,
  "bastions.toml": `schema_version = 1\nname = "Postgres through the tunnel"\nengine = "postgres"\nhost = "postgres"\ndatabase = "spelaion"\nuser = "spelaion"\npassword = { secret = "prompt" }\ntunnel = "docker-bastions"\n\n[tls]\nmode = "verify-full"\nca = ${JSON.stringify(join(keys, "ca.crt"))}\n`,
};
for (const [file, text] of Object.entries(personal)) writeFileSync(join(userDir, "connections", file), text);

const appEnv = { ...process.env };
// The SSH tunnel uses the test key only, never the user's agent.
delete appEnv.SSH_AUTH_SOCK;
Object.assign(appEnv, {
  SPELAION_USER_DIR: userDir,
  // No request to GitHub for updates: the run must not depend on the network (ADR-021).
  SPELAION_NO_UPDATES: "1",
  // kubectl only ever sees nothing, never the user's clusters.
  KUBECONFIG: join(userDir, "no-kubeconfig"),
  // The MCP page never reads or writes the user's AI clients' files (ADR-033).
  SPELAION_MCP_CLIENTS_HOME: join(userDir, "ai-clients"),
  // No session bus: the Secret Service (the user's keychain) cannot be reached at all.
  DBUS_SESSION_BUS_ADDRESS: `unix:path=${join(userDir, "no-session-bus")}`,
});

const P = ".tab-panel:not([hidden])";
let driver;
let wd;
let current = "starting tauri-driver";
let failed = false;
const started = Date.now();
const seconds = (ms) => `${(ms / 1000).toFixed(1)} s`;

async function step(name, body) {
  current = name;
  const t0 = Date.now();
  process.stdout.write(`· ${name}`);
  await body();
  console.log(` (${seconds(Date.now() - t0)})`);
}
/** A screenshot once the page's transitions (a dialog fading in) have ended. */
async function shot(name) {
  await wd
    .waitFor(() => document.getAnimations().every((a) => a.playState !== "running" || a.effect?.getComputedTiming().iterations === Infinity), [], { timeout: 2_000 })
    .catch(() => {});
  return wd.screenshot(join(shots, `${name}.png`));
}

// ---- Page helpers: each function runs in the page and must not use anything outside it. ----
const visibleText = (selector, text) =>
  wd.waitFor(
    (selector, text) => [...document.querySelectorAll(selector)].find((e) => e.getClientRects().length > 0 && e.textContent.includes(text)),
    [selector, text],
    { what: `"${text}" in ${selector}` },
  );
const gone = (selector, text = "") =>
  wd.waitFor(
    (selector, text) => ![...document.querySelectorAll(selector)].some((e) => e.getClientRects().length > 0 && e.textContent.includes(text)),
    [selector, text],
    { what: `${selector} ${text} to go away` },
  );
/** A visible button (or menu item) whose own text is `text`, inside `scope`. */
const button = (text, scope = "body") =>
  wd.element(
    (text, scope) => {
      const roots = [...document.querySelectorAll(scope)].filter((e) => e.getClientRects().length > 0);
      for (const r of roots.reverse()) {
        const found = [...r.querySelectorAll('button, [role="button"], [role="menuitem"]')].find(
          (b) => b.getClientRects().length > 0 && !b.disabled && ((b.getAttribute("aria-label") ?? "").trim() === text || b.textContent.trim() === text),
        );
        if (found) return found;
      }
      return null;
    },
    [text, scope],
    { what: `the button "${text}"` },
  );
/** The topmost dialog whose text contains `text`. */
const dialog = (text) =>
  wd.element(
    (text) => [...document.querySelectorAll('[role="dialog"], [role="alertdialog"], .modal')].reverse().find((d) => d.getClientRects().length > 0 && d.textContent.includes(text)),
    [text],
    { what: `a dialog with "${text}"` },
  );
const statusline = (text) => visibleText(`${P} .statusline`, text);

async function newQuery(connection, sql) {
  // Double-clicking a connection opens a new query on it.
  const label = await wd.element(
    (name) => [...document.querySelectorAll(".connection-row")].find((r) => r.querySelector(".label")?.textContent.trim() === name)?.querySelector(".label"),
    [connection],
    { what: `the connection ${connection}` },
  );
  await wd.pointer(label, { count: 2 });
  await wd.waitFor((p) => document.querySelector(`${p} .cm-content`) && !document.querySelector(`${p} .status-connecting`), [P], { what: "a new query tab" });
  await wd.click(await wd.element((p) => document.querySelector(`${p} .cm-content`), [P]));
  await wd.insertText(sql);
}
async function replaceQuery(sql) {
  await wd.click(await wd.element((p) => document.querySelector(`${p} .cm-content`), [P]));
  await wd.press("Control+a");
  await wd.insertText(sql);
}
async function run() {
  await wd.click(await wd.element((p) => document.querySelector(`${p} .cm-content`), [P]));
  await wd.press("Control+Shift+Enter");
}
/** Fills a text field as typed text and leaves the focus there. */
async function fill(field, text) {
  await wd.click(field);
  await wd.evaluate((el) => el.select?.(), field);
  await wd.insertText(text);
}
/** Answers the password dialog of `connection`; the keychain is never offered a password. */
async function password(connection, secret) {
  const d = await dialog(`Password for ${connection}`);
  const remember = await wd.evaluate((d) => [...d.querySelectorAll('input[type="checkbox"]')].find((c) => c.closest("label")?.textContent.includes("keychain")) ?? null, d);
  if (remember && (await wd.evaluate((c) => c.checked, remember))) await wd.click(remember);
  await fill(await wd.element((d) => d.querySelector('input[type="password"]'), [d]), secret);
  await wd.click(await button("Connect", '[role="dialog"], .modal'));
}

/** The pid of the app tauri-driver started (WebKitWebDriver starts it under tauri-driver). */
function appPid() {
  const app = descendants(driver.child.pid).find((p) => p.exe === exe || p.argv[0] === exe);
  if (!app) throw new Error("the app's process was not found under tauri-driver");
  return app.pid;
}
/** Whether process `pid` still runs (an exited one waiting for its parent does not). */
const alive = (pid) => {
  try {
    const stat = readFileSync(`/proc/${pid}/stat`, "utf8");
    return stat.slice(stat.lastIndexOf(")") + 2, stat.lastIndexOf(")") + 3) !== "Z";
  } catch {
    return false;
  }
};

try {
  driver = await startDriver(appEnv);
  current = "starting the app";
  wd = await Session.start(driver.url, exe, [demo]);

  await step("startup: the demo repository is listed and not trusted yet", async () => {
    await visibleText(".connection-row", "Shop (SQLite)");
    await visibleText("body", "is not trusted");
    await shot("01-start");
  });

  await step("trust the repository", async () => {
    await wd.click(await button("Review and trust"));
    const d = await dialog("Trust this repository?");
    await shot("02-trust");
    await wd.click(await button("Trust", '[role="dialog"], .modal'));
    await wd.waitDetached(d, { what: "the trust dialog to close" });
    await gone("body", "is not trusted");
  });

  await step("SQLite: a query on the demo's Shop", async () => {
    await newQuery(
      "Shop (SQLite)",
      "select c.country, count(*) as orders, round(sum(o.total), 2) as total\nfrom orders o\njoin customer c on c.id = o.customer_id\ngroup by c.country\norder by total desc;",
    );
    await run();
    await statusline("6 rows");
    await shot("03-sqlite");
  });

  await step("copy: the grid's selection reaches the system clipboard (another program reads it)", async () => {
    const before = "nothing copied yet";
    clipboard.set(before);
    // The grid is a canvas: a real click on its first data row, then Ctrl+A and Ctrl+Shift+C
    // (with the column names), as a user does.
    const box = await wd.evaluate((p) => {
      const r = document.querySelector(`${p} .grid-canvas`).getBoundingClientRect();
      return { x: r.x, y: r.y };
    }, P);
    await wd.pointerAt(box.x + 30, box.y + 40);
    await wd.press("Control+a");
    await wd.press("Control+Shift+c");
    let text = before;
    for (let i = 0; i < 50 && text === before; i++) {
      await new Promise((r) => setTimeout(r, 100));
      text = clipboard.get();
    }
    const lines = text.trimEnd().split(/\r?\n/);
    assert.equal(lines[0], "country\torders\ttotal", `the clipboard holds: ${JSON.stringify(text.slice(0, 200))}`);
    assert.equal(lines.length, 7, `a header and six rows: ${JSON.stringify(text.slice(0, 300))}`);
    assert.ok(lines.slice(1).every((line) => line.split("\t").length === 3), text);
  });

  await step("DuckDB: a new database file made in the add-connection form, a query on it", async () => {
    await wd.click(await button("Add connection"));
    const wizard = await wd.element(() => document.querySelector(".modal.wizard"));
    await wd.click(await wd.element((w) => [...w.querySelectorAll(".engine-card")].find((c) => c.textContent.includes("DuckDB")), [wizard], { what: "the DuckDB card" }));
    // The save dialog is answered through the app's test hook (dialogs.ts), as on Windows.
    const duckFile = join(userDir, "analytics.duckdb");
    await wd.evaluate((path) => (window.__spelaionSavePath = path), duckFile);
    await wd.click(await button("New database…", ".modal.wizard"));
    await wd.waitFor((w, path) => [...w.querySelectorAll("input")].some((i) => i.value === path), [wizard, duckFile], { what: "the new file in the form" });
    await wd.evaluate(() => delete window.__spelaionSavePath);
    await wd.click(await wd.element((w) => [...w.querySelectorAll('[role="tab"]')].find((t) => t.textContent.trim() === "Options"), [wizard], { what: "the Options tab" }));
    await wd.click(await wd.element((w) => [...w.querySelectorAll('input[type="radio"]')].find((r) => r.closest("label")?.textContent.trim() === "Personal"), [wizard], { what: "Personal" }));
    await wd.click(await button("Test connection", ".modal.wizard"));
    await visibleText(".modal.wizard .test-result", "Connected");
    await wd.click(await button("Save", ".modal.wizard"));
    await wd.waitDetached(wizard, { what: "the form to close" });
    assert.match(readFileSync(join(userDir, "connections", "analytics.toml"), "utf8"), /engine = "duckdb"/);
    await newQuery("analytics", "create table sales as select range as id, [range, range * 2] as pair, {'city': 'Madrid'} as place from range(3);\nselect * from sales order by id;");
    await run();
    await statusline("3 rows");
    await shot("04-duckdb");
  });

  await step("import: the dialog takes the system clipboard's text (Ctrl+Alt+V, then Paste from clipboard) into a new table", async () => {
    clipboard.set("first_id,first_name\n1,Ann\n");
    if (process.env.SMOKE_DEBUG_CLIPBOARD) {
      const sh = (cmd) => {
        try {
          return execFileSync("bash", ["-c", cmd], { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
        } catch (e) {
          return `ERR ${e.message} ${e.stderr ?? ""}`;
        }
      };
      console.log("\n[debug] xclip -o:", JSON.stringify(sh("xclip -selection clipboard -out")));
      console.log("[debug] xclip TARGETS:", sh("xclip -selection clipboard -out -t TARGETS"));
      console.log("[debug] wmctrl -l -p:", sh("wmctrl -l -p"), "app pid", appPid());
      console.log("[debug] active:", sh("xprop -root _NET_ACTIVE_WINDOW"));
      console.log("[debug] hasFocus:", await wd.evaluate(() => document.hasFocus()));
      await wd.evaluate(() => {
        window.__dbg = "none";
        document.addEventListener("keydown", (e) => {
          if (e.key === "F9") navigator.clipboard.readText().then((t) => (window.__dbg = "ok:" + t), (err) => (window.__dbg = "err:" + err));
        }, { capture: true });
      });
      for (const target of ["", "UTF8_STRING", "STRING", "text/plain", "text/plain;charset=utf-8"]) {
        execFileSync("xclip", ["-selection", "clipboard", "-in", ...(target ? ["-t", target] : [])], { input: `via ${target || "default"} ñ`, stdio: ["pipe", "ignore", "ignore"] });
        await wd.evaluate(() => (window.__dbg = "none"));
        await wd.click(await wd.element((p) => document.querySelector(`${p} .cm-content`), [P]));
        await wd.press("\ue039").catch((e) => console.log("[debug] press:", e.message));
        await new Promise((r) => setTimeout(r, 800));
        console.log(`[debug] xclip -t ${target || "(default)"} -> TARGETS ${sh("xclip -selection clipboard -out -t TARGETS").replace(/\n/g, " ")} -> readText:`, JSON.stringify(await wd.evaluate(() => window.__dbg)));
      }
      clipboard.set("first_id,first_name\n1,Ann\n");
      console.log("[debug] hasFocus:", await wd.evaluate(() => document.hasFocus()));
    }
    await wd.click(await wd.element((p) => document.querySelector(`${p} .cm-content`), [P]));
    await wd.press("Control+Alt+v");
    const d = await dialog("Import data");
    await visibleText('[role="dialog"] [role="region"]', "first_name");
    clipboard.set("city,visits\nMadrid,3\nA Coruña,5\nLisboa,8\n");
    await wd.click(await button("Paste from clipboard", '[role="dialog"]'));
    await visibleText('[role="dialog"] [role="region"]', "A Coruña");
    await shot("05-import-preview");
    await wd.click(await button("Next", '[role="dialog"]'));
    const table = await wd.element((d) => d.querySelector('input[name="import-table"]'), [d]);
    await fill(table, "visits_from_clipboard");
    await wd.click(await button("Review SQL…", '[role="dialog"]'));
    await visibleText('[role="dialog"] .import-sql', "visits_from_clipboard");
    await wd.click(await button("Import", '[role="dialog"]'));
    await visibleText('[role="dialog"]', "Imported 3 rows into visits_from_clipboard");
    await wd.press("Escape");
    await wd.waitDetached(d, { what: "the import dialog to close" });
    await replaceQuery("select city from visits_from_clipboard where visits = 5;");
    await run();
    await statusline("1 row");
    await shot("06-import-done");
  });

  await step("PostgreSQL: password asked in a dialog, a query over TLS", async () => {
    await newQuery("Postgres (Docker)", "select ssl from pg_stat_ssl where pid = pg_backend_pid() and ssl;");
    await run();
    await password("Postgres (Docker)", "spelaion");
    await statusline("1 row");
    await shot("07-postgres");
  });

  for (const [connection, cipher] of [
    ["MySQL (Docker)", "performance_schema.session_status"],
    ["MariaDB (Docker)", "information_schema.session_status"],
  ]) {
    await step(`${connection.replace(" (Docker)", "")}: password asked, a query over TLS`, async () => {
      // Only an encrypted session has a cipher: the row exists only if TLS is on.
      await newQuery(connection, `select 42 as answer, 'text ñ €' as txt, variable_value as cipher\nfrom ${cipher}\nwhere variable_name = 'Ssl_cipher' and variable_value <> '';`);
      await run();
      await password(connection, "spelaion");
      await statusline("1 row");
    });
  }

  await step("SQL Server: password asked, a batch with two result sets", async () => {
    await newQuery("SQL Server (Docker)", "select name from sys.databases where name = 'master'\nselect 1 as a, N'ñandú' as b");
    await run();
    await password("SQL Server (Docker)", "Spelaion-test-1");
    await visibleText(`${P} .result-tabs button`, "Result 2");
    await shot("08-sqlserver");
  });

  await step("SSH tunnel: PostgreSQL behind two bastions, each host key shown, TLS verify-full", async () => {
    await newQuery("Postgres through the tunnel", "select ssl, version from pg_stat_ssl where pid = pg_backend_pid() and ssl;");
    await run();
    await password("Postgres through the tunnel", "spelaion");
    for (let hop = 1; hop <= 2; hop++) {
      await dialog(`(hop ${hop})`);
      if (hop === 1) await shot("09-ssh-fingerprint");
      await wd.click(await button("Trust and remember", '[role="dialog"], .modal'));
    }
    await statusline("1 row");
    await shot("10-tunnel");
  });

  await step("quitting with a transaction open asks first: Cancel keeps the app, Quit and roll back closes it", async () => {
    await newQuery("Shop (SQLite)", "create temp table quit_tx (x);");
    await run();
    await visibleText(P, "Statement 1: OK");
    await wd.click(await wd.element((p) => document.querySelector(`${p} .transaction-controls input[type=checkbox]`), [P]));
    await wd.waitFor((p) => document.querySelector(`${p} .transaction-controls input[type=checkbox]:not(:checked)`), [P], { what: "auto-commit off" });
    await replaceQuery("insert into quit_tx values (1);");
    await run();
    await visibleText(`${P} .tx-badge`, "TX open · 1");
    const pid = appPid();
    // The window's close button, through the compositor: the core holds the window and the UI asks.
    requestClose(pid);
    let d = await dialog("Quit with an open transaction?");
    await shot("11-quit-open-transaction");
    await wd.click(await button("Cancel", '[role="dialog"], .modal'));
    await wd.waitDetached(d, { what: "the quit dialog to close" });
    await new Promise((r) => setTimeout(r, 500));
    assert.ok(alive(pid), "Cancel keeps the app open");
    requestClose(pid);
    d = await dialog("Quit with an open transaction?");
    const quit = await button("Quit and roll back", '[role="dialog"], .modal');
    // The app is gone before WebDriver answers the click, which then reports the session ended.
    await wd.click(quit).catch((e) => {
      if (!/Session terminated|invalid session/.test(e.message)) throw e;
    });
    for (let i = 0; i < 150 && alive(pid); i++) await new Promise((r) => setTimeout(r, 100));
    assert.ok(!alive(pid), "the app did not quit");
  });

  console.log(`\nOK in ${seconds(Date.now() - started)}. Screenshots in ${shots}`);
} catch (e) {
  failed = true;
  console.log("");
  console.error(`FAILED at step "${current}":`, e);
  try {
    console.error(`Screenshot: ${await wd.screenshot(join(shots, "failure.png"))}`);
  } catch (screenshotError) {
    console.error(`No screenshot: ${screenshotError.message}`);
  }
} finally {
  await wd?.end();
  // Nothing this run started outlives it: WebKitWebDriver and the app, then tauri-driver.
  if (driver) {
    const left = descendants(driver.child.pid);
    for (const signal of ["SIGTERM", "SIGKILL"]) {
      for (const p of left) {
        try {
          process.kill(p.pid, signal);
        } catch {}
      }
      for (let i = 0; i < 30 && left.some((p) => alive(p.pid)); i++) await new Promise((r) => setTimeout(r, 100));
    }
    driver.child.kill("SIGKILL");
  }
  rmSync(userDir, { recursive: true, force: true, maxRetries: 20, retryDelay: 250 });
  process.exit(failed ? 1 : 0);
}
