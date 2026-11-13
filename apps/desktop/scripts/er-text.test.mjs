// The ER diagram's text exports (ADR-046) parsed by their own tools: Mermaid's `erDiagram`
// parser and DBML's (`@dbml/core`). The texts are written by the core: `cargo test -p
// spelaion-core --test er_diagram` checks the fixtures in `fixtures/er` are what the core writes
// now (and rewrites them with `SPELAION_UPDATE_ER_FIXTURES=1`); this test, run by `npm run
// build` and so by CI, checks every one of them parses and keeps what the model had.
import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { resolve } from "node:path";
import test from "node:test";
import { Parser } from "@dbml/core";
import mermaid from "mermaid";

const folder = resolve(import.meta.dirname, "fixtures/er");
const read = (name) => readFileSync(resolve(folder, name), "utf8");
const files = (extension) => readdirSync(folder).filter((name) => name.endsWith(extension));

/** The entities and relationships Mermaid reads from a text (it throws if it cannot parse). */
async function mermaidModel(text) {
  await mermaid.parse(text);
  const diagram = await mermaid.mermaidAPI.getDiagramFromText(text);
  return { entities: diagram.db.getEntities(), relationships: diagram.db.getRelationships() };
}

test("there are fixtures for both formats", () => {
  assert.ok(files(".mmd").length >= 2);
  assert.ok(files(".dbml").length >= 2);
});

test("every Mermaid export parses with Mermaid", async () => {
  for (const name of files(".mmd")) {
    const text = read(name);
    const { entities, relationships } = await mermaidModel(text);
    const declared = text.split("\n").filter((line) => /^ {4}"[^"]*" \{$/.test(line)).length;
    assert.equal(entities.size, declared, `${name}: entities`);
    const relations = text.split("\n").filter((line) => / : "[^"]*"$/.test(line)).length;
    assert.equal(relationships.length, relations, `${name}: relationships`);
  }
});

test("Mermaid keeps the awkward names, keys and comments", async () => {
  const { entities } = await mermaidModel(read("awkward.mmd"));
  const lines = entities.get("sales.order lines");
  assert.ok(lines, "sales.order lines");
  const byName = new Map(lines.attributes.map((a) => [a.name, a]));
  assert.deepEqual(byName.get("order_no").keys, ["PK", "FK"]);
  assert.equal(byName.get("order_no").comment, "order no");
  assert.equal(byName.get("日本語").type, "text");
  assert.equal(byName.get("pk_").comment, "pk: a column named like a key");
  assert.ok(entities.has("Ventas ñ.producto"));
});

test("every DBML export parses with DBML", () => {
  for (const name of files(".dbml")) {
    const text = read(name);
    const database = new Parser().parse(text, "dbml");
    const tables = database.schemas.flatMap((schema) => schema.tables);
    const declared = text.split("\n").filter((line) => line.startsWith("Table ")).length;
    assert.equal(tables.length, declared, `${name}: tables`);
    const refs = database.schemas.flatMap((schema) => schema.refs);
    assert.equal(refs.length, text.split("\n").filter((line) => line.startsWith("Ref ")).length, `${name}: refs`);
  }
});

test("DBML keeps the awkward names, keys and notes", () => {
  const database = new Parser().parse(read("awkward.dbml"), "dbml");
  const sales = database.schemas.find((schema) => schema.name === "sales");
  const lines = sales.tables.find((table) => table.name === "order lines");
  assert.equal(lines.note, "Lines of an order: it's \"quoted\"\nand on two lines \\ with a backslash");
  const field = (name) => lines.fields.find((f) => f.name === name);
  assert.equal(field("order no").type.type_name, "character varying(80)");
  assert.equal(field("日本語").note, "non-Latin name");
  assert.equal(field("Table").type.type_name, "timestamp with time zone");
  // A note cannot end in a backslash: a space follows it.
  assert.equal(field("back\\slash").note, "a \\' pair, and a backslash at the end \\ ");
  const pk = lines.indexes.find((index) => index.pk);
  assert.deepEqual(pk.columns.map((c) => c.value), ["id", "order no"]);
  const composite = sales.refs.find((ref) => ref.name === "lines→product");
  assert.ok(composite, "the composite reference");
  assert.deepEqual(composite.endpoints.map((e) => e.fieldNames), [["Ref", "Note"], ["id", "variante"]]);
});

test("the parsers refuse broken text, so a pass means something", async () => {
  await assert.rejects(mermaid.parse('erDiagram\n    "t" {\n        int a b c d\n    }\n'));
  assert.throws(() => new Parser().parse('Table "t" {\n  "a b" int c d\n}\n', "dbml"));
});
