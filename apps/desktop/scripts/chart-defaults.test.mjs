// A result's first chart (src/chartDefaults.ts), run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { defaultChart, isTemporal } from "../src/chartDefaults.ts";

const column = (name, logical, nativeType = logical) => ({ name, logical, nativeType, masked: false });

test("a MySQL date column (text with a date native type) is X, as a line", () => {
  // Since SDB-122 MySQL and MariaDB send DATE, DATETIME and TIMESTAMP as their server text.
  for (const nativeType of ["date", "DATETIME", "datetime(3)", "timestamp", "timestamp(6)"]) {
    const columns = [column("id", "int"), column("region", "text", "varchar"), column("day", "text", nativeType), column("total", "decimal")];
    assert.deepEqual(defaultChart(columns), { kind: "line", x: 2, y: [3], aggregate: "sum" }, nativeType);
  }
});

test("SQL Server's text dates and times count too, as the core's chart reads them", () => {
  for (const nativeType of ["datetime2", "datetime2(7)", "datetimeoffset(7)", "time(7)"]) {
    assert.ok(isTemporal(column("at", "text", nativeType)), nativeType);
  }
});

test("typed date columns stay temporal, and other text stays labels", () => {
  for (const logical of ["date", "time", "timestamp", "timestamptz"]) {
    assert.ok(isTemporal(column("at", logical)), logical);
  }
  for (const nativeType of ["varchar", "text", "nvarchar(50)", "dateish", "enum('a','b')"]) {
    assert.equal(isTemporal(column("label", "text", nativeType)), false, nativeType);
  }
  // A typed column is read by its logical type alone: MySQL's TIME is an interval.
  assert.equal(isTemporal(column("span", "interval", "time")), false);
  assert.equal(isTemporal(undefined), false);
});

test("without a date column the first label is X, as bars", () => {
  const columns = [column("id", "int"), column("region", "text", "varchar"), column("total", "decimal")];
  assert.deepEqual(defaultChart(columns), { kind: "bar", x: 1, y: [2], aggregate: "sum" });
});

test("a picked text date column is X, as a line", () => {
  const columns = [column("total", "decimal"), column("day", "text", "date"), column("count", "int")];
  assert.deepEqual(defaultChart(columns, [1, 0, 2]), { kind: "line", x: 1, y: [0, 2], aggregate: "sum" });
});

test("no numbers, no chart", () => {
  assert.equal(defaultChart([column("day", "text", "date"), column("region", "text", "varchar")]), null);
});
