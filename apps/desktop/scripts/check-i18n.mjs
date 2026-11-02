// Ensure shipped locales have the same messages and interpolation placeholders.
import { readdirSync, readFileSync } from "node:fs";
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
