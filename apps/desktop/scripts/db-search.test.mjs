// Metadata search (src/dbSearch.ts, ADR-043), run by `npm run build` with `node --test`: it
// answers from the explorer's catalog of several connections and never asks a database.
import assert from "node:assert/strict";
import test from "node:test";
import { nameScore, schemasWithoutColumns, searchNames, searchableTables } from "../src/dbSearch.ts";

/** Five connections, as the explorer's catalog holds them after reading their schemas. */
function catalog() {
  const shop = (connectionId, schema) => ({
    connectionId,
    schema,
    relations: [
      { name: "orders", kind: "table" },
      { name: "customers", kind: "table" },
      { name: "order_totals", kind: "view" },
    ],
    others: [{ name: "refresh_customer_stats", kind: "function", detail: "(integer)" }],
    columns: [
      { table: "orders", name: "id", nativeType: "integer" },
      { table: "orders", name: "customer_id", nativeType: "integer" },
      { table: "customers", name: "id", nativeType: "integer" },
      { table: "customers", name: "email", nativeType: "text" },
    ],
  });
  return [
    shop("pg-prod", "public"),
    shop("pg-staging", "public"),
    { ...shop("mysql", "shop"), columns: undefined },
    shop("sqlite", "main"),
    {
      connectionId: "mssql",
      schema: "dbo",
      relations: [{ name: "CustomerAudit", kind: "table" }],
      others: [{ name: "usp_GetCustomer", kind: "procedure" }],
      columns: [{ table: "CustomerAudit", name: "CustomerId", nativeType: "int" }],
    },
  ];
}

test("names match ignoring case: exact, then prefix, then a word's start, then anywhere", () => {
  assert.equal(nameScore("orders", "Orders"), 0);
  assert.equal(nameScore("ord", "orders"), 1);
  assert.equal(nameScore("customer", "refresh_customer_stats"), 2);
  assert.equal(nameScore("Id", "CustomerId"), 2);
  assert.equal(nameScore("tome", "customers"), 3);
  assert.equal(nameScore("xyz", "customers"), null);
  assert.equal(nameScore("  ", "customers"), null);
});

test("one search answers from the catalog of five connections", () => {
  const { hits, total } = searchNames(catalog(), "customer_id", { columns: true, limit: 100 });
  // The column on every connection whose columns were read; MySQL's are not read yet.
  assert.deepEqual(
    hits.map((h) => [h.connectionId, h.kind, h.name, h.column]),
    [
      ["pg-prod", "column", "orders", "customer_id"],
      ["pg-staging", "column", "orders", "customer_id"],
      ["sqlite", "column", "orders", "customer_id"],
    ],
  );
  assert.equal(total, 3);
  const customer = searchNames(catalog(), "customer", { columns: true, limit: 100 });
  const connections = new Set(customer.hits.map((h) => h.connectionId));
  assert.deepEqual([...connections].sort(), ["mssql", "mysql", "pg-prod", "pg-staging", "sqlite"]);
  // Tables before routines before columns, at the same score.
  const first = customer.hits.filter((h) => h.connectionId === "pg-prod").map((h) => h.kind);
  assert.deepEqual(first, ["relation", "column", "schemaObject"]);
});

test("columns can be left out, a dot matches table and column, and the limit keeps the total", () => {
  assert.equal(searchNames(catalog(), "email", { columns: false, limit: 100 }).total, 0);
  const dotted = searchNames(catalog(), "orders.cust", { columns: true, limit: 100 });
  assert.ok(dotted.hits.length > 0);
  assert.ok(dotted.hits.every((h) => h.kind === "column" && h.name === "orders" && h.column === "customer_id"));
  const limited = searchNames(catalog(), "o", { columns: true, limit: 2 });
  assert.equal(limited.hits.length, 2);
  assert.ok(limited.total > 2);
  assert.deepEqual(searchNames(catalog(), "   ", { columns: true, limit: 10 }), { hits: [], total: 0 });
});

test("the schemas still to read and the tables a data search can choose", () => {
  assert.deepEqual(schemasWithoutColumns(catalog()).map((s) => s.connectionId), ["mysql"]);
  assert.deepEqual(searchableTables(catalog(), "pg-prod"), [
    { schema: "public", table: "customers" },
    { schema: "public", table: "order_totals" },
    { schema: "public", table: "orders" },
  ]);
  assert.deepEqual(searchableTables(catalog(), "nowhere"), []);
});
