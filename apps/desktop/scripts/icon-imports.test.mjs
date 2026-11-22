// Every icon goes through `components/Icons.tsx` (SDB-73, docs/ui-guidelines.md "Icons"), run by
// `npm run build` with `node --test`: a component that imports `lucide-react` itself would bring
// its own size and stroke, and icons the shared list does not name.
import assert from "node:assert/strict";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, resolve } from "node:path";
import test from "node:test";

const src = resolve(import.meta.dirname, "../src");

function sources(dir) {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return sources(path);
    return /\.(ts|tsx)$/.test(name) ? [path] : [];
  });
}

test("only Icons.tsx imports lucide-react", () => {
  const importers = sources(src)
    .filter((path) => /from\s+["']lucide-react(\/[^"']*)?["']/.test(readFileSync(path, "utf8")))
    .map((path) => relative(src, path).replaceAll("\\", "/"));
  assert.deepEqual(importers, ["components/Icons.tsx"]);
});
