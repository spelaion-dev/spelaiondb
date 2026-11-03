// Ensure shipped locales have the same messages and interpolation placeholders, and that every
// literal key the code asks for (`t("a.b")`, `t("ns:a.b")`) exists.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

const locales = resolve(import.meta.dirname, "../src/locales");
const source = join(locales, "en");
const target = join(locales, "es");
const files = readdirSync(source).filter((name) => name.endsWith(".json")).sort();
const translated = readdirSync(target).filter((name) => name.endsWith(".json")).sort();
if (JSON.stringify(files) !== JSON.stringify(translated)) throw new Error("Translation namespaces do not match");

function flatten(value, prefix = "", output = {}) {
  for (const [key, entry] of Object.entries(value)) {
    const path = prefix ? `${prefix}.${key}` : key;
    if (typeof entry === "string") output[path] = entry;
    else if (entry && typeof entry === "object" && !Array.isArray(entry)) flatten(entry, path, output);
    else throw new Error(`Invalid translation: ${path}`);
  }
  return output;
}

function placeholders(value) {
  return [...new Set([...value.matchAll(/{{\s*([^{},\s]+)(?:\s*,[^}]*)?\s*}}/g)].map((match) => match[1]))].sort();
}

for (const file of files) {
  const en = flatten(JSON.parse(readFileSync(join(source, file), "utf8")));
  const es = flatten(JSON.parse(readFileSync(join(target, file), "utf8")));
  const keys = Object.keys(en).sort();
  if (JSON.stringify(keys) !== JSON.stringify(Object.keys(es).sort())) throw new Error(`Translation keys do not match: ${file}`);
  for (const key of keys) {
    if (JSON.stringify(placeholders(en[key])) !== JSON.stringify(placeholders(es[key]))) throw new Error(`Translation placeholders do not match: ${file}:${key}`);
  }
}

// Keys used in the code. A file may use several namespaces (one per component): a key is
// found if any of them has it. Keys built at run time (template strings) are not checked.
const messages = Object.fromEntries(
  files.map((file) => [file.replace(/\.json$/, ""), flatten(JSON.parse(readFileSync(join(source, file), "utf8")))]),
);
const exists = (namespace, key) => {
  const table = messages[namespace] ?? {};
  return key in table || `${key}_one` in table || Object.keys(table).some((k) => k.startsWith(`${key}.`));
};
function sources(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return name === "locales" ? [] : sources(path);
    return /\.(tsx|ts)$/.test(name) && name !== "bindings.ts" ? [path] : [];
  });
}
const missing = [];
for (const file of sources(resolve(import.meta.dirname, "../src"))) {
  const code = readFileSync(file, "utf8");
  const namespaces = [...code.matchAll(/useTranslation\(\s*(\[[^\]]*\]|"[^"]+")/g)].flatMap((m) =>
    [...m[1].matchAll(/"([^"]+)"/g)].map((n) => n[1]),
  );
  if (namespaces.length === 0) continue;
  for (const [, key] of code.matchAll(/\bt\(\s*"([A-Za-z0-9_.:]+)"/g)) {
    const [namespace, path] = key.includes(":") ? key.split(":") : [null, key];
    const found = namespace ? exists(namespace, path) : namespaces.some((n) => exists(n, path));
    if (!found) missing.push(`${file.slice(file.indexOf("src"))}: ${key}`);
  }
}
if (missing.length > 0) throw new Error(`Translation keys used in the code but missing:\n${missing.join("\n")}`);
