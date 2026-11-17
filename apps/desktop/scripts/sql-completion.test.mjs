import assert from "node:assert/strict";
import test from "node:test";
import { EditorState } from "@codemirror/state";
import { CompletionContext } from "@codemirror/autocomplete";
import { completeSql, sqlCompletionSource, sqlDialect, mayChangeCompletionCatalog, referencedCompletionTables } from "../src/sqlCompletion.ts";
import { CompletionCache, completionCacheKey } from "../src/completionCache.ts";

const engines = ["postgres", "mysql", "mariadb", "sqlserver", "sqlite", "duckdb", "clickhouse"];
const table = (name, columns, foreignKeys = [], schema = "main") => ({ schema, name, columns, foreignKeys });
const catalog = { defaultSchema: "main", tables: [
  table("orders", ["id", "customer_id", "total"], [{ name: "orders_customer_fk", columns: ["customer_id"], referencedSchema: "main", referencedTable: "customers", referencedColumns: ["id"] }]),
  table("customers", ["id", "name", "country"]), table("other", ["other_only"]),
] };
function result(sql, engine = "postgres", data = catalog, keywordCase = "preserve") {
  const pos = sql.indexOf("|"); assert.ok(pos >= 0);
  return completeSql(sql.replace("|", ""), pos, data, engine, keywordCase);
}
// PostgreSQL's editor keyword list includes unreserved words (e.g. ID). Extra quoting is
// valid SQL; compare the names here, and separately check escaping of unusual identifiers.
const labels = (...args) => result(...args)?.options.map((option) => option.label.replace(/"([a-z_]+)"/g, "$1")) ?? [];
for (const engine of engines) {
  test(`${engine}: aliases, CTEs, chained CTEs and derived-table outputs`, () => {
    assert.deepEqual(labels("SELECT o.| FROM orders o", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("WITH x AS(SELECT customer_id AS a FROM orders)SELECT x.| FROM x", engine), ["a"]);
    assert.deepEqual(labels("WITH x AS(SELECT customer_id AS a FROM orders)SELECT x.|", engine), ["a"]);
    assert.deepEqual(labels("WITH x AS(SELECT customer_id AS a FROM orders), y AS(SELECT a AS b FROM x) SELECT y.| FROM y", engine), ["b"]);
    assert.deepEqual(labels("WITH x(a,b) AS(SELECT id,total FROM orders) SELECT x.| FROM x", engine), ["a", "b"]);
    assert.deepEqual(labels("SELECT q.| FROM (SELECT o.id, total AS amount FROM orders o) q", engine), ["id", "amount"]);
    assert.deepEqual(labels("SELECT q.| FROM (SELECT o.* FROM orders o) AS q", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT x.| FROM (SELECT count(*) AS n, total + 1 adjusted FROM orders) x", engine), ["n", "adjusted"]);
  });
  test(`${engine}: clauses offer relations or columns, never unrelated schemas`, () => {
    assert.deepEqual(labels("SELECT * FROM |", engine).sort(), ["customers", "orders", "other"]);
    assert.deepEqual(labels("SELECT * FROM orders JOIN |", engine).sort(), ["customers", "orders", "other"]);
    assert.deepEqual(labels("SELECT * FROM orders o, |", engine).sort(), ["customers", "orders", "other"]);
    assert.ok(labels("SELECT * FROM orders o WH|", engine).includes("WHERE"));
    assert.deepEqual(labels("SELECT * FROM orders WHERE |", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT * FROM orders GROUP BY |", engine), ["id", "customer_id", "total"]);
    assert.ok(!labels("SELECT * FROM orders WHERE |", engine).includes("other_only"));
    assert.ok(labels("WITH x AS(SELECT id FROM orders) SELECT * FROM |", engine).includes("x"));
  });
  test(`${engine}: FK predicates use aliases, both directions and composite keys`, () => {
    assert.ok(labels("SELECT * FROM orders o JOIN customers c ON |", engine).includes("o.customer_id = c.id"));
    assert.ok(labels("SELECT * FROM customers c JOIN orders o ON |", engine).includes("o.customer_id = c.id"));
    const data = { ...catalog, tables: [table("child", ["a", "b"], [{ name: "composite", columns: ["a", "b"], referencedSchema: "main", referencedTable: "parent", referencedColumns: ["a", "b"] }]), table("parent", ["a", "b"])] };
    assert.ok(labels("SELECT * FROM child c JOIN parent p ON |", engine, data).includes("c.a = p.a AND c.b = p.b"));
    assert.ok(labels("select * from child c join parent p on |", engine, data, "lower").includes("c.a = p.a and c.b = p.b"));
    assert.ok(!labels("SELECT * FROM orders o JOIN other c ON |", engine).some((label) => label.includes(" = ")));
  });
  test(`${engine}: keyword case follows the existing editor preference`, () => {
    assert.ok(labels("se|", engine, catalog, "upper").includes("SELECT"));
    assert.ok(labels("SE|", engine, catalog, "lower").includes("select"));
    assert.ok(labels("se|", engine).includes("select"));
    assert.ok(labels("SE|", engine).includes("SELECT"));
  });
  test(`${engine}: incomplete SQL, string/comment suppression and separate statements`, () => {
    assert.deepEqual(labels("SELECT * FROM orders o WHERE EXISTS(SELECT * FROM customers c WHERE o.|", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT o.| FROM orders o -- JOIN other o", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT 'o.| FROM orders o", engine), []);
    assert.deepEqual(labels("SELECT 1 -- o.|", engine), []);
    assert.deepEqual(labels("SELECT 1 /* o.|", engine), []);
    assert.deepEqual(labels("SELECT * FROM other o; SELECT o.| FROM orders o", engine), ["id", "customer_id", "total"]);
  });
  test(`${engine}: nested aliases shadow outer aliases; sibling and union scopes stay separate`, () => {
    assert.deepEqual(labels("SELECT * FROM orders o WHERE EXISTS(SELECT o.| FROM other o)", engine), ["other_only"]);
    assert.deepEqual(labels("SELECT o.| FROM orders o WHERE EXISTS(SELECT * FROM other o)", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT * FROM other o UNION ALL SELECT o.| FROM orders o", engine), ["id", "customer_id", "total"]);
    assert.deepEqual(labels("SELECT * FROM orders o JOIN (SELECT o.| FROM other x) q ON 1=1", engine), []);
  });
}

test("qualified schemas, escaped identifiers and exact quoted PostgreSQL names", () => {
  for (const engine of engines) {
    const quote = (s) => engine === "mysql" || engine === "mariadb" || engine === "clickhouse" ? `\`${s.replaceAll("`", "``")}\`` : engine === "sqlserver" ? `[${s.replaceAll("]", "]]")}]` : `"${s.replaceAll('"', '""')}"`;
    const data = { defaultSchema: "main", tables: [table("Order Items", ["part name", "odd]`\"column"], [], "Sales Team")] };
    assert.deepEqual(result(`SELECT i.| FROM ${quote("Sales Team")}.${quote("Order Items")} i`, engine, data).options.map((option) => option.label), [quote("part name"), quote("odd]`\"column")]);
    assert.ok(labels(`SELECT * FROM ${quote("Sales Team")}.|`, engine, data).includes("Order Items"));
  }
  const data = { defaultSchema: "main", tables: [table("Mixed", ["yes"]), table("mixed", ["no"])] };
  assert.deepEqual(labels('SELECT m.| FROM "Mixed" m', "postgres", data), ["yes"]);
  assert.deepEqual(labels('SELECT m.| FROM Mixed m', "postgres", data), ["no"]);
  assert.deepEqual(labels('SELECT o.| FROM orders "O"', "postgres"), []);
  assert.deepEqual(labels('SELECT "O".| FROM orders "O"', "postgres"), ["id", "customer_id", "total"]);
  for (const engine of engines) {
    const open = engine === "mysql" || engine === "mariadb" || engine === "clickhouse" ? "`" : engine === "sqlserver" ? "[" : '"';
    const end = open === "[" ? "]" : open;
    const completed = result(`SELECT * FROM orders o WHERE o.${open}cust|${end}`, engine);
    assert.equal(completed.filter, false);
    assert.deepEqual(completed.options.map((option) => option.displayLabel), ["customer_id"]);
  }
});

test("recursive CTEs are bounded and union outputs come from their first arm", () => {
  assert.deepEqual(labels("WITH RECURSIVE x(id) AS(SELECT id FROM orders UNION ALL SELECT id FROM x) SELECT x.| FROM x"), ["id"]);
  assert.deepEqual(labels("WITH x AS(SELECT id AS first FROM orders UNION ALL SELECT total FROM orders) SELECT x.| FROM x"), ["first"]);
  assert.equal(result("(".repeat(65) + "SELECT |"), null);
  assert.equal(result(" ".repeat(300001) + "SELECT |"), null);
});

test("dialect comment and dollar-quote rules match the engine", () => {
  for (const engine of ["mysql", "mariadb"]) {
    assert.deepEqual(labels("WITH x AS(SELECT 1--2 AS a) SELECT x.|", engine), ["a"]);
    assert.deepEqual(labels("SELECT * FROM orders WHERE # o.|", engine), []);
  }
  assert.deepEqual(labels("/* outer /* inner */ SELECT o.| FROM orders o", "sqlite"), ["id", "customer_id", "total"]);
  for (const engine of ["postgres", "duckdb", "sqlserver", "clickhouse"]) {
    assert.deepEqual(labels("/* outer /* inner */ SELECT o.| FROM orders o", engine), []);
    assert.deepEqual(labels("/* outer /* inner */ end */ SELECT o.| FROM orders o", engine), ["id", "customer_id", "total"]);
  }
  for (const engine of ["postgres", "duckdb", "clickhouse"]) assert.deepEqual(labels("SELECT $tag$ SELECT o.| FROM orders o", engine), []);
  // ClickHouse: `# ` and `#!` start a comment, `#x` does not; backslashes escape in every quote.
  assert.deepEqual(labels("SELECT * FROM orders WHERE # o.|", "clickhouse"), []);
  assert.deepEqual(labels("SELECT * FROM orders WHERE #! o.|", "clickhouse"), []);
  assert.deepEqual(labels("SELECT * FROM orders o WHERE 'it\\'s o.|", "clickhouse"), []);
  assert.deepEqual(labels("SELECT * FROM orders o WHERE #x o.|", "clickhouse"), ["id", "customer_id", "total"]);
  assert.ok(labels("SELECT $tag$ SELECT o.| FROM orders o", "sqlite").includes("customer_id"));
});

test("CodeMirror source is synchronous, cache-only and never runs user SQL", () => {
  const sql = "SELECT o. FROM orders o", pos = sql.indexOf(".") + 1;
  const state = EditorState.create({ doc: sql, extensions: [sqlDialect("sqlite").extension] });
  const output = sqlCompletionSource(catalog, "sqlite", "preserve")(new CompletionContext(state, pos, false));
  assert.equal(typeof output.then, "undefined");
  assert.deepEqual(output.options.map((option) => option.label), ["id", "customer_id", "total"]);
  assert.equal(mayChangeCompletionCatalog("SELECT 'CREATE TABLE'; -- DROP TABLE\nSELECT 1", "postgres"), false);
  assert.equal(mayChangeCompletionCatalog("/* note */ ALTER TABLE orders ADD a int", "postgres"), true);
});

test("large scripts and deeply nested statements retain cached basic completion", () => {
  for (const prefix of ["SELECT 1;\n".repeat(30001), " ".repeat(300001), "(".repeat(65)]) {
    const doc = prefix + "SELECT * FROM orders WHERE ";
    const state = EditorState.create({ doc, extensions: [sqlDialect("sqlite").extension] });
    const output = sqlCompletionSource(catalog, "sqlite", "upper")(new CompletionContext(state, doc.length, true));
    assert.ok(output?.options.length, "bounded rich parsing keeps basic cached suggestions available");
  }
});

const tick = () => new Promise((resolve) => setImmediate(resolve));
test("metadata priority resolves aliases and ignores tables shadowed by CTEs", () => {
  const sql = "WITH other AS (SELECT id FROM customers) SELECT o.id FROM orders o JOIN other x ON o.id = x.id";
  assert.deepEqual(referencedCompletionTables(sql, catalog, "postgres").map((item) => item.name).sort(), ["customers", "orders"]);
});
test("catalog keys isolate workspaces, connections, databases, refresh generations and immutable source sessions", () => {
  // A same-engine profile edit can leave the old tab connected to a different server/file.
  const keys = [["a","c","db",0,"old-session"],["b","c","db",0,"old-session"],["a","d","db",0,"old-session"],["a","c","db2",0,"old-session"],["a","c","db",1,"old-session"],["a\u0000c","db",null,0,"old-session"],["a","c","db",0,"new-session"]].map((parts) => completionCacheKey(...parts));
  assert.equal(new Set(keys).size, keys.length);
});
test("metadata reads are serial, shared and released; stale readers cannot overwrite a refresh", async () => {
  const cache = new CompletionCache(), events = [], snapshots = [];
  let resume; const paused = new Promise((done) => { resume = done; });
  const reader = {
    async open(){ events.push("open"); }, async names(){ return structuredClone(catalog); },
    async describe(t){ events.push(t.name); await paused; return {...t, columns:["loaded"]}; }, async cancel(){ events.push("cancel"); }, async close(){ if(events.at(-1) !== "close") events.push("close"); },
  };
  const release = cache.acquire("old", reader, (snapshot) => snapshots.push(snapshot));
  const release2 = cache.acquire("old", reader, () => {});
  await tick(); assert.deepEqual(events, ["open", "orders"]);
  release(); release2(); const count = snapshots.length; await tick();
  assert.deepEqual(events, ["open", "orders", "cancel", "close"], "last release starts cancellation/cleanup while describe is still pending");
  resume(); await tick();
  assert.deepEqual(events, ["open", "orders", "cancel", "close"]); assert.equal(snapshots.length, count);
  const next = [];
  const release3 = cache.acquire("old", {...reader, async describe(t){return {...t, columns:["fresh"]};}}, (snapshot) => next.push(snapshot));
  await tick(); assert.equal(next.at(-1).loading, false); assert.equal(next.at(-1).catalog.tables[0].columns[0], "fresh"); release3();
});
test("referenced tables move ahead of unrelated pending describes without interrupting an active read", async () => {
  const cache = new CompletionCache(), calls = [];
  let resume;
  const gate = new Promise((resolve) => { resume = resolve; });
  const tables = Array.from({ length: 30 }, (_, n) => ({ schema: "main", name: `t${n}`, columns: [], foreignKeys: [] }));
  const reader = { async open(){}, async names(){return { tables, defaultSchema: "main" };},
    async describe(table){ calls.push(table.name); if (table.name === "t0") await gate; return table; },
    async cancel(){}, async close(){} };
  const release = cache.acquire("test", reader, () => {});
  await tick();
  cache.prioritize("test", [tables[29]]);
  resume(); await tick();
  assert.deepEqual(calls.slice(0, 2), ["t0", "t29"]);
  release();
});
test("one bulk lookup exposes active-schema columns while table descriptions are pending", async () => {
  const cache = new CompletionCache(), snapshots = [], calls = [];
  let resume;
  const gate = new Promise((resolve) => { resume = resolve; });
  const reader = { async open(){}, async names(){return structuredClone(catalog);},
    async columns(schema){calls.push(schema); return [{ table: "customers", name: "bulk_column" }];},
    async describe(table){ await gate; return table; }, async cancel(){}, async close(){} };
  const release = cache.acquire("batch", reader, (s) => snapshots.push(s));
  await tick();
  assert.deepEqual(calls, ["main"]);
  assert.deepEqual(snapshots.at(-1).catalog.tables[1].columns, ["bulk_column"]);
  assert.equal(snapshots.at(-1).loading, true);
  release(); resume();
});
test("active schema columns arrive before unrelated schema discovery finishes", async () => {
  const cache = new CompletionCache(), snapshots = [], calls = [];
  let resume;
  const slowSchema = new Promise((resolve) => { resume = resolve; });
  const first = [{ schema: "main", name: "chosen", columns: [], foreignKeys: [] }];
  const reader = { async open(){}, async names(cancelled, ready){
    await ready("main", first, "main"); calls.push("other schema"); await slowSchema;
    if (!cancelled()) await ready("other", [], "main");
    return { tables: first, defaultSchema: "main" };
  }, async columns(){return [{ table: "chosen", name: "wanted" }];},
  async describe(t){return t;}, async cancel(){}, async close(){} };
  const release = cache.acquire("progressive", reader, (s) => snapshots.push(s));
  await tick();
  assert.deepEqual(calls, ["other schema"]);
  assert.deepEqual(snapshots.at(-1).catalog.tables[0].columns, ["wanted"]);
  assert.equal(snapshots.at(-1).loading, true);
  release(); const count = snapshots.length; resume(); await tick(); assert.equal(snapshots.length, count);
});
test("bulk metadata failure falls back to descriptions and retains foreign keys", async () => {
  const cache = new CompletionCache(), snapshots = [];
  const reader = { async open(){}, async names(){return structuredClone(catalog);},
    async columns(){throw Error("bulk metadata unsupported");},
    async describe(table){return table;}, async cancel(){}, async close(){} };
  const release = cache.acquire("fallback", reader, (s) => snapshots.push(s));
  await tick();
  assert.equal(snapshots.at(-1).error, null);
  assert.equal(snapshots.at(-1).catalog.tables[0].foreignKeys[0].name, "orders_customer_fk");
  assert.equal(snapshots.at(-1).loading, false);
  release();
});
test("a cancelled bulk lookup cannot publish into a replacement cache entry", async () => {
  const cache = new CompletionCache(), old = [], fresh = [];
  let resume;
  const gate = new Promise((resolve) => { resume = resolve; });
  const reader = { async open(){}, async names(){return structuredClone(catalog);}, async columns(){await gate; return [{table:"orders",name:"stale"}];}, async describe(t){return t;}, async cancel(){}, async close(){} };
  const release = cache.acquire("same", reader, (s) => old.push(s));
  await tick(); release(); const count = old.length;
  const releaseNew = cache.acquire("same", {...reader, async columns(){return [{table:"orders",name:"fresh"}];}}, (s) => fresh.push(s));
  await tick(); resume(); await tick();
  assert.equal(old.length, count);
  assert.ok(fresh.some((s) => s.catalog.tables[0]?.columns.includes("fresh")));
  assert.ok(fresh.every((s) => !s.catalog.tables[0]?.columns.includes("stale")));
  releaseNew();
});
test("a metadata error preserves names/loaded columns and is visible to the editor", async () => {
  const cache = new CompletionCache(), snapshots = [];
  const release = cache.acquire("error", {async open(){},async names(){return structuredClone(catalog);}, async describe(t){if(t.name === "orders") throw Error("catalog denied");return t;}, async cancel(){}, async close(){}}, (s) => snapshots.push(s));
  await tick(); assert.equal(snapshots.at(-1).error, "catalog denied"); assert.equal(snapshots.at(-1).catalog.tables.length, 3); assert.equal(snapshots.at(-1).loading, false); release();
});

test("releasing before connect finishes closes the eventual metadata session without reading its catalog", async () => {
  let resume; const opening = new Promise((done) => { resume = done; });
  const events = [], cache = new CompletionCache(); let opened = false, closed = false;
  const reader = { async open(){await opening; opened = true; events.push("opened");}, async names(){throw Error("must not read");}, async describe(){throw Error("must not read");}, async cancel(){events.push("cancel");}, async close(){if(opened && !closed){closed=true;events.push("closed");}} };
  const release = cache.acquire("connecting", reader, () => {}); release(); release(); await tick();
  assert.deepEqual(events, ["cancel"]); resume(); await tick(); assert.deepEqual(events, ["cancel", "opened", "closed"]);
});

test("malformed and partially typed SQL stays bounded and never throws", () => {
  const pieces = ["SELECT", "FROM", "WITH", "AS", "JOIN", "ON", "(", ")", ",", ".", "'", '"', "/*", "--", "orders", "x", " ", ";"];
  let seed = 27;
  for (const engine of engines) for (let n = 0; n < 200; n++) {
    let sql = "";
    for (let i = 0; i < 20; i++) { seed = (Math.imul(seed, 1664525) + 1013904223) >>> 0; sql += pieces[seed % pieces.length] + " "; }
    assert.doesNotThrow(() => completeSql(sql, Math.floor(sql.length / 2), catalog, engine, "preserve"));
  }
});
