// The resolver behind Ctrl+click and hover in the SQL editor (SDB-382): what a name in the
// text is, read as completion reads the statement and looked up only in the catalog given.
import assert from "node:assert/strict";
import test from "node:test";
import { resolveName } from "../src/sqlCompletion.ts";

const engines = ["postgres", "mysql", "mariadb", "sqlserver", "sqlite", "duckdb", "clickhouse"];
/** A table whose columns have been read (`details`), as the catalog holds it once described. */
const table = (name, columns, schema = "main", kind = "table") => ({
  schema, name, columns, foreignKeys: [], kind,
  details: columns.map((column) => ({ name: column, type: "integer", nullable: true, comment: null })),
});
const catalog = { defaultSchema: "main", tables: [
  table("orders", ["id", "customer_id", "total"]),
  table("customers", ["id", "name", "country"]),
  table("orders", ["id", "old_total"], "archive"),
  table("Order Items", ["Id", "qty"]),
  table("big_orders", ["id", "total"], "main", "view"),
] };

/** What the name at `|` resolves to, as text: `table main.orders [orders]`, or `null`. */
function at(sql, engine = "postgres", data = catalog) {
  const pos = sql.indexOf("|");
  assert.ok(pos >= 0, sql);
  const text = sql.replace("|", "");
  const found = resolveName(text, pos, data, engine);
  if (!found) return null;
  const name = `${found.table.schema}.${found.table.name}`;
  return `${found.kind} ${found.kind === "column" ? `${name}.${found.column}` : name} [${text.slice(found.from, found.to)}]`;
}

for (const engine of engines) {
  test(`${engine}: a relation's name where it is written, qualified or not, and its alias`, () => {
    assert.equal(at("SELECT * FROM ord|ers o", engine), "table main.orders [orders]");
    assert.equal(at("SELECT * FROM |orders", engine), "table main.orders [orders]");
    assert.equal(at("SELECT * FROM orders|", engine), "table main.orders [orders]");
    assert.equal(at("SELECT * FROM archive.ord|ers", engine), "table archive.orders [archive.orders]");
    assert.equal(at("SELECT * FROM arch|ive.orders a", engine), "table archive.orders [archive.orders]");
    assert.equal(at("SELECT * FROM orders AS |o", engine), "table main.orders [o]");
    assert.equal(at("SELECT * FROM customers c JOIN big_or|ders b ON b.id = c.id", engine), "table main.big_orders [big_orders]");
    assert.equal(at("UPDATE cust|omers SET name = 'x'", engine), "table main.customers [customers]");
    assert.equal(at("INSERT INTO cust|omers (name) VALUES ('x')", engine), "table main.customers [customers]");
    assert.equal(at("DELETE FROM ord|ers WHERE id = 1", engine), "table main.orders [orders]");
    // The second statement is read on its own.
    assert.equal(at("SELECT 1; SELECT * FROM cust|omers", engine), "table main.customers [customers]");
    // A table that is not in the catalog shows nothing.
    assert.equal(at("SELECT * FROM unkn|own", engine), null);
    assert.equal(at("SELECT * FROM nowhere.ord|ers", engine), null);
  });

  test(`${engine}: through an alias, a table's own name or its schema, to the table or its column`, () => {
    assert.equal(at("SELECT o|.total FROM orders o", engine), "table main.orders [o]");
    assert.equal(at("SELECT o.to|tal FROM orders o", engine), "column main.orders.total [total]");
    assert.equal(at("SELECT orders.to|tal FROM orders", engine), "column main.orders.total [total]");
    assert.equal(at("SELECT ord|ers.total FROM orders", engine), "table main.orders [orders]");
    assert.equal(at("SELECT archive.orders.old_to|tal FROM archive.orders", engine), "column archive.orders.old_total [old_total]");
    assert.equal(at("SELECT * FROM orders o JOIN customers c ON c.i|d = o.customer_id", engine), "column main.customers.id [id]");
    assert.equal(at("SELECT * FROM orders o JOIN customers c ON c.id = o.customer_|id", engine), "column main.orders.customer_id [customer_id]");
    // The alias of another relation, a column the table does not have, an alias not declared.
    assert.equal(at("SELECT c.to|tal FROM orders o JOIN customers c ON c.id = o.id", engine), null);
    assert.equal(at("SELECT x.to|tal FROM orders o", engine), null);
    // Once a table has an alias, its own name no longer names it.
    assert.equal(at("SELECT orders.to|tal FROM orders o", engine), null);
  });

  test(`${engine}: a bare name is a column only when exactly one visible table has it`, () => {
    assert.equal(at("SELECT na|me FROM orders o JOIN customers c ON c.id = o.customer_id", engine), "column main.customers.name [name]");
    assert.equal(at("SELECT to|tal FROM orders", engine), "column main.orders.total [total]");
    assert.equal(at("SELECT * FROM orders WHERE customer_|id = 1 ORDER BY total", engine), "column main.orders.customer_id [customer_id]");
    assert.equal(at("INSERT INTO orders (id, to|tal) VALUES (1, 2)", engine), "column main.orders.total [total]");
    assert.equal(at("UPDATE orders SET to|tal = 0 WHERE id = 1", engine), "column main.orders.total [total]");
    // Both tables have an id: it is not guessed.
    assert.equal(at("SELECT i|d FROM orders o JOIN customers c ON c.id = o.customer_id", engine), null);
    // Not a column of any table here, a keyword, a function call.
    assert.equal(at("SELECT coun|try FROM orders", engine), null);
    assert.equal(at("SEL|ECT total FROM orders", engine), null);
    assert.equal(at("SELECT to|tal(1) FROM orders", engine), null);
    assert.equal(at("SELECT to|tal", engine), null);
    // A subquery sees the outer query's tables; the outer query does not see the inner ones.
    assert.equal(at("SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND coun|try = 'ES')", engine), "column main.customers.country [country]");
    assert.equal(at("SELECT to|tal FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id)", engine), null);
  });

  test(`${engine}: a CTE or a subquery is not a table, even with a table's name`, () => {
    // The CTE named like the table shadows it: neither its name nor its columns are the table's.
    assert.equal(at("WITH orders AS (SELECT 1 AS total) SELECT * FROM ord|ers", engine), null);
    assert.equal(at("WITH orders AS (SELECT 1 AS total) SELECT to|tal FROM orders", engine), null);
    assert.equal(at("WITH orders AS (SELECT 1 AS total) SELECT orders.to|tal FROM orders", engine), null);
    assert.equal(at("WITH orders AS (SELECT 1 AS total) SELECT o.to|tal FROM orders o", engine), null);
    assert.equal(at("WITH orders AS (SELECT 1 AS total) SELECT * FROM orders |o", engine), null);
    // Inside the CTE's own body the name still is the table (it is not recursive).
    assert.equal(at("WITH customers AS (SELECT * FROM cust|omers WHERE country = 'ES') SELECT * FROM customers", engine), "table main.customers [customers]");
    // Another CTE leaves the tables alone.
    assert.equal(at("WITH recent AS (SELECT * FROM orders) SELECT * FROM rec|ent r JOIN cust|omers c ON c.id = r.customer_id".replace("rec|ent", "recent"), engine), "table main.customers [customers]");
    assert.equal(at("WITH recent AS (SELECT * FROM orders) SELECT * FROM rec|ent", engine), null);
    // A derived table's columns are its own, and a bare name that one of them has is not guessed.
    assert.equal(at("SELECT x.i|d FROM (SELECT id FROM orders) x", engine), null);
    assert.equal(at("SELECT * FROM (SELECT id FROM orders) |x", engine), null);
    assert.equal(at("SELECT to|tal FROM (SELECT total FROM orders) q JOIN customers c ON c.id = 1", engine), null);
    // Inside the derived table, the table is the table.
    assert.equal(at("SELECT x.id FROM (SELECT i|d FROM orders) x", engine), "column main.orders.id [id]");
  });

  test(`${engine}: a relation named outside a query`, () => {
    assert.equal(at("DROP TABLE ord|ers", engine), "table main.orders [orders]");
    assert.equal(at("DROP TABLE IF EXISTS archive.ord|ers", engine), "table archive.orders [archive.orders]");
    assert.equal(at("DROP VIEW big_or|ders", engine), "table main.big_orders [big_orders]");
    assert.equal(at("TRUNCATE TABLE cust|omers", engine), "table main.customers [customers]");
    assert.equal(at("ALTER TABLE orders ADD FOREIGN KEY (customer_id) REFERENCES cust|omers (id)", engine), "table main.customers [customers]");
    assert.equal(at("CREATE TABLE new_|one (id int)", engine), null);
  });

  test(`${engine}: comments, strings and unfinished text show nothing`, () => {
    assert.equal(at("SELECT 1 -- from ord|ers\n", engine), null);
    assert.equal(at("SELECT 1 /* ord|ers */ FROM customers", engine), null);
    assert.equal(at("SELECT 'ord|ers' FROM customers", engine), null);
    assert.equal(at("SELECT * FROM orders WHERE |", engine), null);
    assert.equal(at("SELECT * FROM | orders", engine), null);
    assert.equal(at("|", engine), null);
  });

  test(`${engine}: a table whose columns are not read yet is known by name only`, () => {
    const loading = { defaultSchema: "main", tables: [
      { schema: "main", name: "orders", columns: [], foreignKeys: [], kind: "table" },
      table("customers", ["id", "name", "country"]),
    ] };
    assert.equal(at("SELECT * FROM ord|ers", engine, loading), "table main.orders [orders]");
    assert.equal(at("SELECT o.to|tal FROM orders o", engine, loading), null);
    // `name` is a column of customers, but orders may have one too: nothing rather than a guess.
    assert.equal(at("SELECT na|me FROM orders o JOIN customers c ON c.id = o.id", engine, loading), null);
    assert.equal(at("SELECT c.na|me FROM orders o JOIN customers c ON c.id = o.id", engine, loading), "column main.customers.name [name]");
    assert.equal(at("SELECT * FROM ord|ers", engine, { defaultSchema: null, tables: [] }), null);
  });
}

test("quoted names, as each engine quotes and compares them", () => {
  // PostgreSQL and DuckDB: double quotes; a quoted name is exact, an unquoted one folds to lower case.
  assert.equal(at('SELECT * FROM "Order It|ems"', "postgres"), 'table main.Order Items ["Order Items"]');
  assert.equal(at('SELECT "I|d" FROM "Order Items"', "postgres"), 'column main.Order Items.Id ["Id"]');
  assert.equal(at('SELECT i.q|ty FROM "Order Items" i', "postgres"), "column main.Order Items.qty [qty]");
  assert.equal(at('SELECT "main"."Order Items"."qt|y" FROM "main"."Order Items"', "postgres"), 'column main.Order Items.qty ["qty"]');
  assert.equal(at("SELECT * FROM ORD|ERS", "postgres"), "table main.orders [ORDERS]");
  assert.equal(at('SELECT * FROM "ORD|ERS"', "postgres"), null);
  assert.equal(at('SELECT i|d FROM "Order Items"', "postgres"), null);
  assert.equal(at('SELECT * FROM "ord|ers"', "postgres"), 'table main.orders ["orders"]');
  assert.equal(at('SELECT * FROM "Order It|ems"', "duckdb"), 'table main.Order Items ["Order Items"]');
  // MySQL, MariaDB and ClickHouse: backticks; names compare without case, and "…" is a string
  // in MySQL and MariaDB.
  for (const engine of ["mysql", "mariadb", "clickhouse"]) {
    assert.equal(at("SELECT * FROM `Order It|ems`", engine), "table main.Order Items [`Order Items`]", engine);
    assert.equal(at("SELECT `I|D` FROM `order items`", engine), "column main.Order Items.Id [`ID`]", engine);
    assert.equal(at("SELECT * FROM `main`.`ORD|ERS`", engine), "table main.orders [`main`.`ORDERS`]", engine);
  }
  for (const engine of ["mysql", "mariadb"]) assert.equal(at('SELECT * FROM "ord|ers"', engine), null, engine);
  // A backtick inside a backticked name is doubled.
  const odd = { defaultSchema: "main", tables: [table("odd`name", ["a"]), table('say "hi"', ["a"]), table("a]b", ["a"])] };
  assert.equal(at("SELECT * FROM `odd``na|me`", "mysql", odd), "table main.odd`name [`odd``name`]");
  assert.equal(at('SELECT * FROM "say ""h|i"""', "postgres", odd), 'table main.say "hi" ["say ""hi"""]');
  // SQL Server: brackets (a `]` doubled) and double quotes; SQLite: brackets, backticks and double quotes.
  assert.equal(at("SELECT * FROM [Order It|ems]", "sqlserver"), "table main.Order Items [[Order Items]]");
  assert.equal(at("SELECT [main].[orders].[to|tal] FROM [main].[orders]", "sqlserver"), "column main.orders.total [[total]]");
  assert.equal(at("SELECT * FROM [a]]|b]", "sqlserver", odd), "table main.a]b [[a]]b]]");
  assert.equal(at('SELECT * FROM "Order It|ems"', "sqlserver"), 'table main.Order Items ["Order Items"]');
  for (const quoted of ["[Order It|ems]", "`Order It|ems`", '"Order It|ems"']) {
    assert.equal(at(`SELECT qty FROM ${quoted}`, "sqlite")?.startsWith("table main.Order Items"), true, quoted);
  }
  // An unfinished quoted name is not a name yet.
  assert.equal(at('SELECT * FROM "Order It|ems', "postgres"), null);
  assert.equal(at("SELECT * FROM `Order It|ems", "mysql"), null);
});

test("the default schema decides between tables of one name; without one, only a single match counts", () => {
  assert.equal(at("SELECT * FROM ord|ers"), "table main.orders [orders]");
  assert.equal(at("SELECT * FROM ord|ers", "postgres", { ...catalog, defaultSchema: "archive" }), "table archive.orders [orders]");
  assert.equal(at("SELECT * FROM ord|ers", "postgres", { ...catalog, defaultSchema: "elsewhere" }), null);
  assert.equal(at("SELECT * FROM cust|omers", "postgres", { ...catalog, defaultSchema: "elsewhere" }), "table main.customers [customers]");
  assert.equal(at("SELECT old_to|tal FROM orders", "postgres", { ...catalog, defaultSchema: "archive" }), "column archive.orders.old_total [old_total]");
});

test("a pasted dump is not read: nothing is shown rather than the editor stalling", () => {
  const dump = `${"SELECT 1;\n".repeat(40_000)}SELECT * FROM orders`;
  assert.ok(dump.length > 300_000);
  assert.equal(resolveName(dump, dump.length - 3, catalog, "postgres"), null);
});
