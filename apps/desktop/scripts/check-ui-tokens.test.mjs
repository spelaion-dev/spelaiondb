import assert from "node:assert/strict";
import test from "node:test";
import postcss from "postcss";
import { canvasViolations, countViolations, cssViolations, growingEntries } from "./check-ui-tokens.mjs";

const stylesheet = (css) => postcss.parse(`
  :root { --fg: #f22; --text-small: 12px; --shadow: 0 2px 3px black; --weight: 700; }
  :root[data-theme="dark"] { --fg: #f9f9f9; }
  ${css}
`);

const rules = (violations) => violations.map(({ key }) => key.split(" | ").at(-1));

test("CSS declarations use the two theme blocks, with narrow exceptions for brand and syntax colors", () => {
  const css = stylesheet(`
    /* .ghost { color: red !important; } */
    .button {
      color: var(--fg);
      background: color-mix(in srgb, var(--fg), transparent);
      font: 600 var(--text-small) var(--font-large);
      font-size: var(--text-small);
      border-radius: var(--radius-small);
      box-shadow: var(--shadow);
      z-index: var(--raised);
      font-weight: 400;
      content: "#fff";
      mask: url("#icon");
    }
    .engine-postgres { background: #336791; }
    :root[data-theme="dark"] .sql-editor .sql-keyword { color: #c792ea; }
    [hidden] { display: none !important; }
  `);
  assert.deepEqual(cssViolations(css), []);
});

test("CSS catches values in shorthands, fallbacks, nested rules and additional root blocks", () => {
  const css = stylesheet(`
    @media (width > 10px) {
      .button {
        color: var(--fg, rebeccapurple);
        background: linear-gradient(90deg, #abcd, oklch(70% .2 40));
        font: 650 1rem/1.5 sans-serif;
        border-end-start-radius: calc(var(--radius-small) + 2px);
        box-shadow: var(--shadow), 0 1px currentColor;
        z-index: -1;
        font-weight: var(--weight, 700);
        font-weight: var(--weight);
        display: none !important;
      }
    }
    .engine-postgres { font-weight: bold; }
    :root[data-theme="dark"] .sql-editor .sql-keyword { border-radius: 1px; }
    :root { --other: #eee; }
    [hidden] { display: block !important; }
    .hidden, [hidden] { display: none !important; }
  `);
  assert.deepEqual(rules(cssViolations(css)), [
    "color", "color", "font size", "font weight", "radius", "shadow", "z-index", "font weight",
    "font weight", "!important", "font weight", "radius", "color", "!important", "!important",
  ]);
});

test("canvas catches literals in paint, font, shadow and rounded corners", () => {
  const canvas = `
    const canvas = document.createElement("canvas");
    const context = canvas.getContext("2d");
    // context.font = "bold 22px sans-serif";
    const accent = style.getPropertyValue("--accent");
    context.fillStyle = accent;
    context.strokeStyle = "rebeccapurple";
    context.font = \`700 \${size}px \${family}\`;
    const size = style.getPropertyValue("--text-sm") || "12px";
    context.roundRect(0, 0, width, height, [2, 3]);
    context.shadowOffsetY = -3;
    gradient.addColorStop(.5, "white");
    const props = { fontWeight: 650, zIndex: -2 };
  `;
  assert.deepEqual(rules(canvasViolations("Example.tsx", canvas)), [
    "color", "font weight", "font size", "radius", "radius", "shadow", "color", "font weight", "z-index",
  ]);
});

test("the baseline detects new declarations and additional copies of an existing one", () => {
  const key = "styles.css | .button | color: red | color";
  const other = key.replace(".button", ".other");
  const baseline = countViolations([{ key }, { key }]);
  assert.deepEqual(baseline, { [key]: 2 });
  assert.deepEqual(growingEntries({ [key]: 3, [other]: 1 }, baseline), [key, other]);
  assert.deepEqual(growingEntries({ [key]: 1 }, baseline), []);
});
