// Contrast of text, icons and focus outlines on the surfaces they use, in both app themes.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import postcss from "postcss";
import { themeRules } from "./check-ui-tokens.mjs";

const css = readFileSync(resolve(import.meta.dirname, "../src/styles.css"), "utf8");

function tokens(block) {
  return Object.fromEntries(block.nodes
    .filter((node) => node.type === "decl" && node.prop.startsWith("--"))
    .map((decl) => [decl.prop, decl.value]));
}

const [lightBlock, darkBlock] = themeRules(postcss.parse(css));
const light = tokens(lightBlock);
const dark = { ...light, ...tokens(darkBlock) };

function tokenColor(theme, token) {
  const names = new Set();
  while (!names.has(token)) {
    names.add(token);
    const value = theme[token];
    const alias = /^var\(\s*(--[\w-]+)\s*\)$/.exec(value);
    if (alias) token = alias[1];
    else {
      assert.match(value ?? "", /^#(?:[\da-f]{3}|[\da-f]{6})$/i, `${token} defines an opaque color`);
      return value.length === 4 ? value.replace(/[\da-f]/gi, (c) => c + c) : value;
    }
  }
  assert.fail(`Circular color token: ${[...names].join(" → ")} → ${token}`);
}

function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a, b) {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
}

const textPairs = [
  // Secondary text: the explorer's group headers and section labels, the grid's header, panels.
  ["--muted", "--sidebar"],
  ["--muted", "--activity-bg"],
  ["--muted", "--grid-header"],
  ["--muted", "--panel"],
  ["--muted", "--bg"],
  ["--fg", "--sidebar"],
  ["--fg", "--panel"],
  // Values colored by their type in the grid (SDB-385), on its rows and its striped rows.
  ["--value-number", "--grid-bg"],
  ["--value-number", "--grid-stripe"],
  ["--value-date", "--grid-bg"],
  ["--value-date", "--grid-stripe"],
  ["--value-bool", "--grid-bg"],
  ["--value-bool", "--grid-stripe"],
  ["--value-text", "--grid-bg"],
  ["--value-text", "--grid-stripe"],
  ["--value-null", "--grid-bg"],
  ["--value-null", "--grid-stripe"],
  // The connected status (the toolbar) and the connection test's "ok" marks.
  ["--success", "--panel"],
  ["--success", "--bg"],
  // Status chips, alerts and the foreground of solid buttons.
  ...["success", "warning", "danger", "info"].map((name) => [`--${name}-fg`, `--${name}-soft`]),
  ["--accent-text", "--accent"],
  ["--danger-text", "--danger"],
  // Solid connection-type and transaction badges (ADR-029).
  ["--tone-neutral-text", "--tone-neutral"],
  ...["green", "blue", "amber", "red"].map((tone) => ["--tone-text", `--tone-${tone}`]),
];

const iconPairs = [
  // Selected activity button, active tab, and the explorer and test status icons.
  ["--fg", "--activity-bg"],
  ["--accent", "--panel"],
  ["--muted", "--sidebar"],
  ["--success", "--bg"],
];

const focusPairs = ["--bg", "--panel", "--sidebar", "--activity-bg", "--grid-bg"]
  .map((surface) => ["--accent", surface]);

for (const [name, theme] of [["light", light], ["dark", dark]]) {
  for (const [role, pairs, minimum] of [["text", textPairs, 4.5], ["icons", iconPairs, 3], ["focus outlines", focusPairs, 3]]) {
    test(`${role} contrast meets ${minimum}:1 in the ${name} theme`, () => {
      for (const [foreground, surface] of pairs) {
        const fg = tokenColor(theme, foreground);
        const bg = tokenColor(theme, surface);
        const ratio = contrast(fg, bg);
        assert.ok(ratio >= minimum, `${foreground} ${fg} on ${surface} ${bg} is ${ratio.toFixed(2)}:1`);
      }
    });
  }
}

/** A translucent token (`rgba(…)`) laid over an opaque surface, as the grid's canvas draws it. */
function blended(theme, overlay, surface) {
  const value = theme[overlay] ?? "";
  const parts = /^rgba\(\s*(\d+),\s*(\d+),\s*(\d+),\s*(0?\.\d+|1|0)\s*\)$/.exec(value);
  assert.ok(parts, `${overlay} defines a translucent color: ${value}`);
  const [r, g, b, alpha] = parts.slice(1).map(Number);
  const base = tokenColor(theme, surface);
  return "#" + [r, g, b].map((channel, i) => {
    const under = parseInt(base.slice(1 + i * 2, 3 + i * 2), 16);
    return Math.round(under * (1 - alpha) + channel * alpha).toString(16).padStart(2, "0");
  }).join("");
}

// The grid's highlights (SDB-75) over its rows and striped rows: the cell's text, plain or
// colored by its type, stays readable.
const gridHighlights = ["--grid-edited", "--grid-found", "--grid-found-current", "--grid-deleted"];
const gridTexts = ["--grid-text", "--value-number", "--value-date", "--value-bool", "--value-text", "--value-null"];
for (const [name, theme] of [["light", light], ["dark", dark]]) {
  test(`the grid's highlights keep its text at 4.5:1 in the ${name} theme`, () => {
    for (const highlight of gridHighlights) {
      for (const surface of ["--grid-bg", "--grid-stripe"]) {
        const bg = blended(theme, highlight, surface);
        for (const text of gridTexts) {
          const fg = tokenColor(theme, text);
          const ratio = contrast(fg, bg);
          assert.ok(ratio >= 4.5, `${text} ${fg} on ${highlight} over ${surface} (${bg}) is ${ratio.toFixed(2)}:1`);
        }
      }
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
