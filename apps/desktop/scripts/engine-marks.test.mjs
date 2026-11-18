// Every engine of the "add connection" picker has a mark or a decision that it has none
// (SDB-363), run by `npm run build` with `node --test`. The engines are read from the core's
// catalogue (`spelaion-core`'s `catalog.rs`) and their names from `spelaion-config`'s
// `Engine::as_str`, so an engine added there fails here until `assets/engines/marks.json`
// gives it a mark (a licensed file, listed in that folder's README) or says why it has none.
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";

const root = resolve(import.meta.dirname, "../../..");
const assets = resolve(import.meta.dirname, "../src/assets/engines");
const table = JSON.parse(readFileSync(resolve(assets, "marks.json"), "utf8"));
const readme = readFileSync(resolve(assets, "README.md"), "utf8");

/** The ids of the picker's entries, as `engine_catalog` lists them. */
function catalogIds() {
  const source = readFileSync(resolve(root, "crates/spelaion-core/src/catalog.rs"), "utf8");
  const body = source.slice(source.indexOf("pub fn engine_catalog"), source.indexOf("#[cfg(test)]"));
  assert.ok(body.length > 0, "engine_catalog found in catalog.rs");
  const model = readFileSync(resolve(root, "crates/spelaion-config/src/model.rs"), "utf8");
  const names = new Map([...model.matchAll(/Engine::(\w+) => "([a-z0-9-]+)",/g)].map((m) => [m[1], m[2]]));
  const ids = [];
  for (const m of body.matchAll(/supported\(\s*Engine::(\w+)/g)) {
    assert.ok(names.has(m[1]), `Engine::${m[1]} has a name in Engine::as_str`);
    ids.push(names.get(m[1]));
  }
  for (const m of body.matchAll(/kind_of\(\s*Engine::\w+,\s*"([^"]+)"/g)) ids.push(m[1]);
  for (const m of body.matchAll(/planned\(\s*"([^"]+)"/g)) ids.push(m[1]);
  for (const m of body.matchAll(/\bid:\s*"([^"]+)"/g)) ids.push(m[1]);
  return ids;
}

test("every engine and kind of the catalogue has a mark or a decision that it has none", () => {
  const ids = catalogIds();
  // The reader itself: the catalogue has the engines it is known to have.
  for (const id of ["postgres", "tidb", "file-folder", "snowflake", "meilisearch"]) assert.ok(ids.includes(id), id);
  const missing = ids.filter((id) => !(id in table.marks) && !(id in table.without));
  assert.deepEqual(missing, [], "add a licensed mark to marks.json, or the engine to its `without` list with the reason");
  // Nothing in the table that the catalogue does not have (a renamed engine).
  const stale = [...Object.keys(table.marks), ...Object.keys(table.without)].filter((id) => !ids.includes(id));
  assert.deepEqual(stale, []);
});

test("an engine has a mark or a reason, never both, and the list without one stays short", () => {
  for (const id of Object.keys(table.without)) assert.ok(!(id in table.marks), id);
  for (const [id, entry] of Object.entries(table.without)) {
    assert.ok(["base", "folder", "database"].includes(entry.shows), `${id}: ${entry.shows}`);
    assert.ok(entry.why.length > 10, `${id} says why it has no mark`);
  }
  assert.ok(Object.keys(table.without).length <= 12, "most engines have a mark: look for one before adding to the list");
  for (const id of table.colored) assert.ok(id in table.marks, id);
});

test("every mark is a file of the folder, credited in its README, with nothing that runs", () => {
  for (const [id, file] of Object.entries(table.marks)) {
    const path = resolve(assets, file);
    assert.ok(existsSync(path), `${id}: ${file}`);
    assert.ok(readme.includes(`\`${file}\``), `${file} is credited in assets/engines/README.md`);
    const svg = readFileSync(path, "utf8");
    assert.match(svg, /^\s*(<\?xml[^>]*>\s*)?<svg[\s>]/, `${file} is an SVG`);
    // Shown as <img>, where scripts never run; still, a mark holds only shapes.
    assert.doesNotMatch(svg, /<script|\bon[a-z]+\s*=|javascript:|<foreignObject|xlink:href\s*=\s*"(?!#)|href\s*=\s*"(?!#)/i, file);
  }
});
