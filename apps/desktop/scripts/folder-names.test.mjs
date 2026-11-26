// How open folders and connections are named with several folders open (src/folderNames.ts,
// ADR-075 point 8, SDB-502). Run by `npm run build` with `node --test`.
import assert from "node:assert/strict";
import test from "node:test";
import { connectionName, folderNames } from "../src/folderNames.ts";

test("a folder is named by its name, with its parent's when another has the same name", () => {
  const names = folderNames([
    { path: "C:\\work\\shop\\api", root: "C:\\work\\shop\\api" },
    { path: "/home/me/crm/api/", root: "/home/me/crm/api" },
    { path: "/home/me/billing", root: "/home/me/billing" },
  ]);
  assert.equal(names.get("C:\\work\\shop\\api"), "api (shop)");
  assert.equal(names.get("/home/me/crm/api"), "api (crm)");
  assert.equal(names.get("/home/me/billing"), "billing");
});

test("names that differ only in case are the same name", () => {
  const names = folderNames([
    { path: "/a/Api", root: "/a/Api" },
    { path: "/b/api", root: "/b/api" },
  ]);
  assert.equal(names.get("/a/Api"), "Api (a)");
  assert.equal(names.get("/b/api"), "api (b)");
});

test("a connection shows its folder only where another has the same name", () => {
  const names = new Map([["/work/shop", "shop"], ["/work/crm", "crm"]]);
  const shop = { id: "prod/orders", name: "Orders", folder: "/work/shop" };
  const crm = { id: "prod/orders", name: "orders", folder: "/work/crm" };
  const mine = { id: "~/orders", name: "Orders", folder: null };
  const alone = { id: "prod/stock", name: "Stock", folder: "/work/shop" };
  const all = [shop, crm, mine, alone];
  assert.equal(connectionName(shop, all, names, "Personal"), "Orders · shop");
  assert.equal(connectionName(crm, all, names, "Personal"), "orders · crm");
  assert.equal(connectionName(mine, all, names, "Personal"), "Orders · Personal");
  assert.equal(connectionName(alone, all, names, "Personal"), "Stock");
  // With one folder open, nothing is added.
  assert.equal(connectionName(shop, [shop, alone], names, "Personal"), "Orders");
});
