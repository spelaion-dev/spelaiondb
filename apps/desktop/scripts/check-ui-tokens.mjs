// Enforce the desktop's token rules at build time (docs/ui-guidelines.md).
import { execFileSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import postcss from "postcss";
import ts from "typescript";

const desktop = resolve(import.meta.dirname, "..");
const baselineFile = "scripts/ui-tokens-baseline.json";
const themes = [":root", ':root[data-theme="dark"]'];

// CSS Color's named and system colors. Transparent and currentColor inherit or
// remove paint, so they can be used without a palette token.
const namedColors = new Set(`
  aliceblue antiquewhite aqua aquamarine azure beige bisque black blanchedalmond blue blueviolet brown burlywood
  cadetblue chartreuse chocolate coral cornflowerblue cornsilk crimson cyan darkblue darkcyan darkgoldenrod
  darkgray darkgreen darkgrey darkkhaki darkmagenta darkolivegreen darkorange darkorchid darkred darksalmon
  darkseagreen darkslateblue darkslategray darkslategrey darkturquoise darkviolet deeppink deepskyblue dimgray
  dimgrey dodgerblue firebrick floralwhite forestgreen fuchsia gainsboro ghostwhite gold goldenrod gray
  green greenyellow grey honeydew hotpink indianred indigo ivory khaki lavender lavenderblush lawngreen
  lemonchiffon lightblue lightcoral lightcyan lightgoldenrodyellow lightgray lightgreen lightgrey lightpink
  lightsalmon lightseagreen lightskyblue lightslategray lightslategrey lightsteelblue lightyellow lime
  limegreen linen magenta maroon mediumaquamarine mediumblue mediumorchid mediumpurple mediumseagreen
  mediumslateblue mediumspringgreen mediumturquoise mediumvioletred midnightblue mintcream mistyrose
  moccasin navajowhite navy oldlace olive olivedrab orange orangered orchid palegoldenrod palegreen
  paleturquoise palevioletred papayawhipp peachpuff peru pink plum powderblue purple rebeccapurple red
  rosybrown royalblue saddlebrown salmon sandybrown seagreen seashell sienna silver skyblue slateblue
  slategray slategrey snow springgreen steelblue tan teal thistle tomato turquoise violet wheat white
  whitesmoke yellow yellowgreen accentcolor accentcolortext activetext buttonborder buttonface buttontext
  canvas canvastext field fieldtext graytext highlight highlighttext linktext mark marktext selecteditem
  selecteditemtext visitedtext
`.trim().split(/\s+/));

const explicitColor = /#[\da-f]+\b|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color|device-cmyk)\(/i;
const dimension = /(?<![\w-])[+-]?(?:\d+(?:\.\d+)?|\.\d+)(?:%|px|cm|mm|q|in|pc|pt|em|ex|ch|cap|ic|rem|lh|rlh|[sld]?v[whib]|[sld]?vmin|[sld]?vmax|cq[whib]|cqmin|cqmax)(?![\w-])/i;
const fontSize = new RegExp(`${dimension.source}|(?<![\\w-])(?:x{1,3}-small|small|medium|large|x{1,3}-large|smaller|larger|math)(?![\\w-])`, "i");
const fontWeight = /(?<![\w-])(?:\d+(?:\.\d+)?|bold|bolder|lighter)(?=\s|[),]|$)/gi;
const allowedWeight = /^(?:400|600)(?:\.0+)?$/;
const cssWideKeywords = /^(?:inherit|initial|revert|revert-layer|unset)$/i;
const number = /(?<![\w-])[+-]?(?:\d+(?:\.\d+)?|\.\d+)(?![\w-])/;

// These exact selectors hold the existing engine badge and SQL syntax colors.
// Their other declarations are subject to the same rules as the rest of the app.
const engineBadges = new Set(
  "postgres sqlite mysql mariadb sqlserver oracle cockroachdb duckdb clickhouse snowflake bigquery mongodb redis cassandra"
    .split(" ").map((engine) => `.engine-${engine}`),
);
const syntaxColors = new Set(
  ["keyword", "string", "literal"].map((kind) => `${themes[1]} .sql-editor .sql-${kind}`),
);

function unquoted(value) {
  // A quoted string or a URL may contain a hash or a word without painting it.
  return value.replace(/"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'/g, " ")
    .replace(/\burl\((?:\\.|[^)\\])*\)/gi, " ");
}

function hasColor(prop, value) {
  const acceptsNamedColor = /color|^--|^(?:background|border|outline|fill|stroke|(?:box|text)-shadow|filter|text-decoration|column-rule|caret)(?:-|$)/.test(prop);
  return explicitColor.test(value) || (acceptsNamedColor &&
    (value.match(/[-a-z][\w-]*/gi) ?? []).some((word) => namedColors.has(word.toLowerCase())));
}

function rulesFor(prop, rawValue) {
  const value = unquoted(rawValue).trim();
  if (cssWideKeywords.test(value)) return [];
  const rules = [];
  if (hasColor(prop, value)) rules.push("color");
  if (prop === "font-size" && (fontSize.test(value) || /^0(?:\.0+)?$/.test(value))) rules.push("font size");
  if (/^border(?:-[a-z]+)*-radius$/.test(prop) && (dimension.test(value) || number.test(value))) rules.push("radius");
  if (prop === "z-index" && number.test(value)) rules.push("z-index");
  if ((/^(?:box|text)-shadow$/.test(prop) && (dimension.test(value) || number.test(value))) ||
      /\bdrop-shadow\(/i.test(value)) {
    rules.push("shadow");
  }
  if (prop === "font-weight" && !/^normal$/i.test(value) && !allowedWeight.test(value)) rules.push("font weight");
  if (prop === "font") {
    const size = fontSize.exec(value.split("/")[0]);
    if (size) rules.push("font size");
    const prefix = size ? value.slice(0, size.index) : value;
    if ([...prefix.matchAll(fontWeight)].some(([weight]) => !allowedWeight.test(weight))) {
      rules.push("font weight");
    }
  }
  return rules;
}

export function themeRules(css) {
  return themes.map((selector) => {
    const rule = css.nodes.find((node) => node.type === "rule" && node.selector === selector);
    if (!rule) throw new Error(`Missing token block: ${selector}`);
    return rule;
  });
}

export function cssViolations(css) {
  const tokenBlocks = new Set(themeRules(css));
  const found = [];
  css.walkDecls((decl) => {
    const prop = decl.prop.toLowerCase();
    const selectors = [];
    for (let parent = decl.parent; parent.type !== "root"; parent = parent.parent) {
      selectors.unshift(parent.selector ?? `@${parent.name} ${parent.params}`.trim());
    }
    const scope = selectors.join(" > ").replace(/\s+/g, " ");
    const exemptColor = (engineBadges.has(scope) && prop === "background") ||
      (syntaxColors.has(scope) && prop === "color");
    const rules = tokenBlocks.has(decl.parent) && prop.startsWith("--") ? [] : rulesFor(prop, decl.value);
    // The one allowed !important: the hidden attribute hides an element whatever display its
    // classes give it, as the browser's own rule would without author styles.
    const hiddenReset = scope === "[hidden]" && prop === "display" && decl.value.trim() === "none";
    if (decl.important && !hiddenReset) rules.push("!important");
    for (const rule of rules) {
      if (rule !== "color" || !exemptColor) {
        found.push({
          key: `styles.css | ${scope} | ${prop}: ${decl.value.replace(/\s+/g, " ")} | ${rule}`,
          line: decl.source.start.line,
        });
      }
    }
  });
  return found;
}

function canvasProperty(node) {
  const parent = node.parent;
  if (ts.isBinaryExpression(parent) && parent.right === node && ts.isPropertyAccessExpression(parent.left)) {
    return parent.left.name.text;
  }
  if ((ts.isPropertyAssignment(parent) || ts.isJsxAttribute(parent)) && parent.initializer === node) {
    return parent.name.getText().replace(/^['"]|['"]$/g, "");
  }
  if (ts.isCallExpression(parent) && ts.isPropertyAccessExpression(parent.expression) &&
      parent.expression.name.text === "addColorStop" && parent.arguments[1] === node) return "color";
  return "";
}

const canvasProperties = new Map(Object.entries({
  font: "font", fontSize: "font-size", fontWeight: "font-weight", borderRadius: "border-radius", zIndex: "z-index",
  boxShadow: "box-shadow", textShadow: "text-shadow", filter: "filter",
  fillStyle: "color", strokeStyle: "color", shadowColor: "color",
}));

function canvasRules(node, property, value) {
  const parent = node.parent;
  const cssProperty = canvasProperties.get(property) ?? property.toLowerCase();
  if (cssProperty === "font-size" && ts.isNumericLiteral(node)) return ["font size"];
  if (/^shadow(?:Blur|OffsetX|OffsetY)$/.test(property) && number.test(value)) return ["shadow"];
  if (ts.isCallExpression(parent) && ts.isPropertyAccessExpression(parent.expression) &&
      parent.expression.name.text === "roundRect" && parent.arguments[4] === node && number.test(value)) return ["radius"];
  if (ts.isArrayLiteralExpression(parent) && ts.isCallExpression(parent.parent) &&
      ts.isPropertyAccessExpression(parent.parent.expression) && parent.parent.expression.name.text === "roundRect" &&
      parent.parent.arguments[4] === parent && number.test(value)) return ["radius"];
  if (!property && /^(?:[\w.-]+\s+)*(?:\d*\.)?\d+px\s+\S/.test(value)) return rulesFor("font", value);
  if (!property && ts.isVariableDeclaration(parent) && /font$/i.test(parent.name.getText())) return rulesFor("font", value);
  if (ts.isBinaryExpression(parent) && parent.right === node &&
      /\.getPropertyValue\(["']--text-/.test(parent.left.getText())) return rulesFor("font-size", value);
  if (!property && explicitColor.test(value)) return ["color"];
  return property ? rulesFor(cssProperty, value) : [];
}

export function canvasViolations(file, code) {
  const source = ts.createSourceFile(file, code, ts.ScriptTarget.Latest, true);
  const found = [];
  function visit(node) {
    let value;
    if (ts.isStringLiteralLike(node)) value = node.text;
    if (ts.isNumericLiteral(node) && !ts.isPrefixUnaryExpression(node.parent)) value = node.text;
    if (ts.isPrefixUnaryExpression(node) && ts.isNumericLiteral(node.operand)) value = node.getText(source);
    if (ts.isTemplateExpression(node)) {
      value = node.head.text + node.templateSpans.map((span) => `__value__${span.literal.text}`).join("");
    }
    if (value !== undefined && !ts.isLiteralTypeNode(node.parent)) {
      const property = canvasProperty(node);
      const scope = property ||
        (ts.isVariableDeclaration(node.parent) ? node.parent.name.getText(source) : "canvas");
      for (const rule of canvasRules(node, property, value)) {
        found.push({
          key: `${file} | ${scope} | ${value.replace(/\s+/g, " ")} | ${rule}`,
          line: source.getLineAndCharacterOfPosition(node.getStart(source)).line + 1,
        });
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(source);
  return found;
}

function sourceFiles(dir) {
  return readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) return sourceFiles(path);
    return /\.(?:ts|tsx)$/.test(path) && entry.name !== "bindings.ts" ? [path] : [];
  });
}

export function countViolations(violations) {
  const counts = new Map();
  for (const { key } of violations) counts.set(key, (counts.get(key) ?? 0) + 1);
  return Object.fromEntries([...counts.keys()].sort().map((key) => [key, counts.get(key)]));
}

export function growingEntries(current, previous) {
  return Object.keys(current).filter((key) => current[key] > (previous[key] ?? 0));
}

function readBaseline(source) {
  const baseline = JSON.parse(source);
  if (baseline === null || typeof baseline !== "object" || Array.isArray(baseline) ||
      Object.values(baseline).some((count) => !Number.isSafeInteger(count) || count < 1)) {
    throw new Error(`Invalid counts in ${baselineFile}`);
  }
  return baseline;
}

function pullRequestBaseBaseline() {
  if (process.env.GITHUB_ACTIONS !== "true" || process.env.GITHUB_EVENT_NAME !== "pull_request") return null;
  const event = JSON.parse(readFileSync(process.env.GITHUB_EVENT_PATH, "utf8"));
  const sha = event.pull_request?.base?.sha;
  if (typeof sha !== "string" || !/^[\da-f]{40}$/.test(sha)) throw new Error("Invalid GitHub pull request base SHA");
  const git = (...args) => execFileSync("git", args, { cwd: desktop, encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
  try {
    git("cat-file", "-e", `${sha}^{commit}`);
  } catch {
    // The frontend job normally has a shallow checkout. Only CI needs the base;
    // a local build can work from source without a remote.
    git("fetch", "--no-tags", "--depth=1", "origin", sha);
  }
  const path = `apps/desktop/${baselineFile}`;
  if (!git("ls-tree", sha, "--", `:(top)${path}`).trim()) return null;
  return readBaseline(git("show", `${sha}:${path}`));
}

function main() {
  const src = join(desktop, "src");
  const css = postcss.parse(readFileSync(join(src, "styles.css"), "utf8"), { from: "src/styles.css" });
  const violations = cssViolations(css);
  for (const path of sourceFiles(src)) {
    const file = relative(src, path).replaceAll("\\", "/");
    const code = readFileSync(path, "utf8");
    if (file.startsWith("grid/") || /CanvasRenderingContext2D|\.getContext\(\s*["']2d["']/.test(code)) {
      violations.push(...canvasViolations(file, code));
    }
  }
  const actual = countViolations(violations);
  if (process.argv.includes("--print-baseline")) {
    console.log(JSON.stringify(actual, null, 2));
    return;
  }
  const baseline = readBaseline(readFileSync(join(desktop, baselineFile), "utf8"));
  const allowed = { ...baseline };
  for (const { key, line } of violations) {
    allowed[key] = (allowed[key] ?? 0) - 1;
    if (allowed[key] < 0) console.error(`src/${key.replace(" | ", `:${line} | `)}`);
  }
  const added = growingEntries(actual, baseline);
  const removed = growingEntries(baseline, actual);
  const previous = pullRequestBaseBaseline();
  const baselineGrowth = previous ? growingEntries(baseline, previous) : [];
  if (added.length) console.error("Use theme tokens for new style declarations.");
  if (removed.length) console.error(`Remove fixed violations from ${baselineFile}:\n${removed.join("\n")}`);
  if (baselineGrowth.length) console.error(`The baseline can only shrink:\n${baselineGrowth.join("\n")}`);
  if (added.length || removed.length || baselineGrowth.length) process.exitCode = 1;
  else console.log(`Desktop tokens: ${violations.length} existing violations in ${baselineFile}.`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
