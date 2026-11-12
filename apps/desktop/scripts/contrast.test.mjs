// Contrast of the theme's text tokens on the surfaces they appear on (src/styles.css), in both
// themes, run by `npm run build` with `node --test` (SDB-69). docs/ui-guidelines.md asks 4.5:1
// for `--fg` and `--muted` on every surface they appear on, and for status text; the connected
// status and the connection test's "ok" marks use `--success`, never a literal green.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

const css = readFileSync(resolve(import.meta.dirname, "../src/styles.css"), "utf8");

/** The `--name: #hex;` tokens of the first block that starts with `selector {`. */
function tokens(selector) {
  const start = css.indexOf(`${selector} {`);
  assert.ok(start >= 0, selector);
  const block = css.slice(start, css.indexOf("\n}", start));
  return Object.fromEntries([...block.matchAll(/(--[\w-]+):\s*(#[0-9a-fA-F]{6})\b/g)].map((m) => [m[1], m[2]]));
}

const light = tokens(":root");
const dark = { ...light, ...tokens(':root[data-theme="dark"]') };

function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a, b) {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

const pairs = [
  // Secondary text: the explorer's group headers and section labels, the grid's header, panels.
  ["--muted", "--sidebar"],
  ["--muted", "--activity-bg"],
  ["--muted", "--grid-header"],
  ["--muted", "--panel"],
  ["--muted", "--bg"],
  ["--fg", "--sidebar"],
  ["--fg", "--panel"],
  // The connected status (the toolbar) and the connection test's "ok" marks.
  ["--success", "--panel"],
  ["--success", "--bg"],
];

for (const [name, theme] of [["light", light], ["dark", dark]]) {
  test(`text tokens reach 4.5:1 on their surfaces in the ${name} theme`, () => {
    for (const [text, surface] of pairs) {
      assert.ok(theme[text] && theme[surface], `${name}: ${text} and ${surface} are defined`);
      const ratio = contrast(theme[text], theme[surface]);
      assert.ok(ratio >= 4.5, `${name}: ${text} ${theme[text]} on ${surface} ${theme[surface]} is ${ratio.toFixed(2)}:1`);
    }
  });
}

test("the connected status and the test's ok marks use --success, not a literal green", () => {
  for (const selector of [".status-connected", ".test-result .ok", ".test-log header .ok", ".test-log .step.ok .icon"]) {
    const start = css.indexOf(`\n${selector} {`);
    assert.ok(start >= 0, selector);
    const rule = css.slice(start, css.indexOf("}", start));
    assert.match(rule, /color:\s*var\(--success\)/, `${selector}: ${rule.trim()}`);
  }
});
