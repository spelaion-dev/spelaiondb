// A small W3C WebDriver client for the Linux smoke test (smoke-linux.mjs): it starts
// `tauri-driver`, which puts WebKitWebDriver in front of the app (WebKitGTK has no CDP port),
// and speaks to it with Node's own fetch. Only what the smoke test uses is here: a session, scripts
// in the page, element lookups, real key and pointer actions, and screenshots.

import { spawn } from "node:child_process";
import { once } from "node:events";
import { createServer } from "node:net";
import { writeFileSync } from "node:fs";

/** The key a W3C key action sends for each name (the WebDriver spec's code points). */
const KEYS = {
  Control: "", Shift: "", Alt: "", Meta: "",
  Enter: "", Escape: "", Tab: "", Backspace: "", Delete: "",
  ArrowUp: "", ArrowDown: "", ArrowLeft: "", ArrowRight: "",
  Home: "", End: "", PageUp: "", PageDown: "", Space: " ",
};
const ELEMENT = "element-6066-11e4-a52e-4f735466cecf";

async function freePort() {
  const server = createServer();
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address();
  server.close();
  await once(server, "close");
  return port;
}

/** Starts `tauri-driver` (from TAURI_DRIVER or the PATH) on free ports, with `env` for the app it
 *  will start: WebKitWebDriver starts the app with the environment it was given. */
export async function startDriver(env) {
  const port = await freePort();
  const nativePort = await freePort();
  const args = ["--port", String(port), "--native-port", String(nativePort)];
  if (process.env.WEBKIT_WEBDRIVER) args.push("--native-driver", process.env.WEBKIT_WEBDRIVER);
  const child = spawn(process.env.TAURI_DRIVER ?? "tauri-driver", args, { env, stdio: ["ignore", "ignore", "inherit"] });
  const failed = new Promise((_, reject) => child.once("error", (e) => reject(new Error(`tauri-driver could not start (cargo install tauri-driver --locked): ${e.message}`))));
  const url = `http://127.0.0.1:${port}`;
  for (let i = 0; i < 100; i++) {
    const ready = await Promise.race([
      failed,
      fetch(`${url}/status`).then((r) => r.ok, () => false),
    ]);
    if (ready) return { url, child };
    if (child.exitCode !== null) break;
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill();
  throw new Error("tauri-driver did not answer on its port");
}

export class WebDriverError extends Error {}

/** A WebDriver session: the app started by tauri-driver with `application` and `args`. */
export class Session {
  static async start(url, application, args) {
    const session = new Session(url);
    const value = await session.#http("POST", "/session", {
      capabilities: { alwaysMatch: { "tauri:options": { application, args } } },
    });
    session.id = value.sessionId;
    return session;
  }

  constructor(url) {
    this.url = url;
    this.id = null;
  }

  async #http(method, path, body, timeout = 120_000) {
    const res = await fetch(this.url + path, {
      method,
      signal: AbortSignal.timeout(timeout),
      headers: { "content-type": "application/json" },
      body: body === undefined ? undefined : JSON.stringify(body),
    });
    const text = await res.text();
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      throw new WebDriverError(`${method} ${path}: ${res.status} ${text.slice(0, 300)}`);
    }
    if (json.value?.error) throw new WebDriverError(`${method} ${path}: ${json.value.error}: ${json.value.message}`);
    return json.value;
  }

  #cmd(method, path, body) {
    return this.#http(method, `/session/${this.id}${path}`, body);
  }

  /** Runs `fn` in the page with `args` (elements are passed and returned as elements); a promise
   *  it returns is awaited. */
  evaluate(fn, ...args) {
    return this.#cmd("POST", "/execute/async", {
      script: `const done = arguments[arguments.length - 1];
        Promise.resolve().then(() => (${fn}).apply(null, Array.prototype.slice.call(arguments, 0, -1)))
          .then((value) => done({ value }), (e) => done({ error: String((e && e.stack) || e) }));`,
      args,
    }).then((r) => {
      if (r?.error) throw new Error(`in the page: ${r.error}`);
      return r?.value;
    });
  }

  /** Waits until `fn` (run in the page) returns something truthy, and returns it. */
  async waitFor(fn, args = [], { timeout = 30_000, what = String(fn) } = {}) {
    const start = Date.now();
    let last;
    for (;;) {
      try {
        const value = await this.evaluate(fn, ...args);
        if (value) return value;
      } catch (e) {
        last = e;
      }
      if (Date.now() - start > timeout) throw new Error(`timed out waiting for ${what}${last ? ` (${last.message})` : ""}`);
      await new Promise((r) => setTimeout(r, 150));
    }
  }

  /** Waits until `element` is no longer in the page (WebKitWebDriver then calls it stale). */
  async waitDetached(element, { timeout = 30_000, what = "an element to go away" } = {}) {
    const start = Date.now();
    for (;;) {
      try {
        if (await this.evaluate((e) => !e.isConnected, element)) return;
      } catch (e) {
        if (/stale element|no such element/.test(e.message)) return;
        throw e;
      }
      if (Date.now() - start > timeout) throw new Error(`timed out waiting for ${what}`);
      await new Promise((r) => setTimeout(r, 150));
    }
  }

  /** The element `fn` returns from the page, once it returns one. */
  element(fn, args = [], options) {
    return this.waitFor(fn, args, options);
  }

  click(element) {
    return this.#cmd("POST", `/element/${element[ELEMENT]}/click`, {});
  }

  #pointer(actions) {
    return this.#cmd("POST", "/actions", { actions: [{ type: "pointer", id: "mouse", parameters: { pointerType: "mouse" }, actions }] });
  }

  /** Real pointer clicks on an element's centre (button 0 is the left one, 2 the right one). */
  async pointer(element, { button = 0, count = 1 } = {}) {
    const actions = [{ type: "pointerMove", origin: element, x: 0, y: 0, duration: 20 }];
    for (let i = 0; i < count; i++) actions.push({ type: "pointerDown", button }, { type: "pointerUp", button });
    await this.#pointer(actions);
  }

  /** Real pointer clicks at viewport coordinates (WebKitWebDriver refuses canvas origins). */
  async pointerAt(x, y, { button = 0, count = 1 } = {}) {
    const actions = [{ type: "pointerMove", origin: "viewport", x: Math.round(x), y: Math.round(y), duration: 20 }];
    for (let i = 0; i < count; i++) actions.push({ type: "pointerDown", button }, { type: "pointerUp", button });
    await this.#pointer(actions);
  }

  /** Presses a chord such as "Control+Shift+Enter" with real key actions. Letters are sent as
   *  key values, so they follow the keyboard layout: type text with `insertText`. */
  async press(chord) {
    const keys = chord.split("+").map((k) => KEYS[k] ?? k);
    const actions = [...keys.map((value) => ({ type: "keyDown", value })), ...[...keys].reverse().map((value) => ({ type: "keyUp", value }))];
    await this.#cmd("POST", "/actions", { actions: [{ type: "key", id: "keyboard", actions }] });
    await this.#cmd("DELETE", "/actions");
  }

  /** Inserts text where the focus is, as typed text (no key events, no autocompletion). */
  insertText(text) {
    return this.evaluate((t) => document.execCommand("insertText", false, t), text);
  }

  async screenshot(path) {
    writeFileSync(path, Buffer.from(await this.#cmd("GET", "/screenshot"), "base64"));
    return path;
  }

  /** Ends the session; tauri-driver then stops the app if it is still running. */
  async end() {
    if (!this.id) return;
    const id = this.id;
    this.id = null;
    await this.#http("DELETE", `/session/${id}`, undefined, 10_000).catch(() => {});
  }
}
