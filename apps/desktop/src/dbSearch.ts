// Metadata search (ADR-043): tables, views, routines and columns by name, across every schema
// the explorer has read on every connection. It only reads the catalog it is given (catalog.ts,
// with the columns each schema read once): typing never asks a database anything.

export interface SearchRelation {
  name: string;
  /** `table`, `view`, `materialized_view`… as the explorer lists it. */
  kind: string;
}

export interface SearchSchemaObject {
  name: string;
  /** `function`, `procedure`, `sequence`… (ADR-024). */
  kind: string;
  detail?: string | null;
}

export interface SearchColumn {
  table: string;
  name: string;
  nativeType: string;
}

export interface SearchCatalogSchema {
  connectionId: string;
  schema: string;
  relations: SearchRelation[];
  others: SearchSchemaObject[];
  /** Absent until the schema's columns are read. */
  columns?: SearchColumn[];
}

export type NameHitKind = "relation" | "schemaObject" | "column";

export interface NameHit {
  kind: NameHitKind;
  connectionId: string;
  schema: string;
  /** The relation, routine or other object; for a column, its table. */
  name: string;
  /** The relation's or object's kind (`table`, `function`…); a column's native type. */
  type: string;
  /** A column's name. */
  column?: string;
  /** A routine's arguments, a trigger's table… */
  detail?: string | null;
  /** Lower is better: 0 exact, 1 prefix, 2 a word's start, 3 anywhere. */
  score: number;
}

export interface NameSearchOptions {
  columns: boolean;
  /** Most hits returned; `total` still counts every match. */
  limit: number;
}

/** How well `name` matches `query` (both compared in lower case), or null when it does not. */
export function nameScore(query: string, name: string): number | null {
  const q = query.trim().toLowerCase();
  if (!q) return null;
  const n = name.toLowerCase();
  if (n === q) return 0;
  if (n.startsWith(q)) return 1;
  const at = n.indexOf(q);
  if (at < 0) return null;
  // A word starts after `_`, `-`, `.`, a space or a lower-case letter followed by upper case.
  for (let i = at; i >= 0; i = n.indexOf(q, i + 1)) {
    const before = name[i - 1] ?? "";
    const here = name[i] ?? "";
    if (/[_\-. $]/.test(before) || (before === before.toLowerCase() && here !== here.toLowerCase())) return 2;
  }
  return 3;
}

/**
 * Every name in `schemas` that `query` matches, best first. A query with a dot
 * (`orders.cust`) matches columns by `table.column` too.
 */
export function searchNames(schemas: SearchCatalogSchema[], query: string, options: NameSearchOptions): { hits: NameHit[]; total: number } {
  const q = query.trim();
  if (!q) return { hits: [], total: 0 };
  const hits: NameHit[] = [];
  const dot = q.lastIndexOf(".");
  const tablePart = dot > 0 ? q.slice(0, dot) : null;
  const columnPart = dot > 0 ? q.slice(dot + 1) : null;
  for (const entry of schemas) {
    for (const relation of entry.relations) {
      const score = nameScore(q, relation.name);
      if (score !== null) hits.push({ kind: "relation", connectionId: entry.connectionId, schema: entry.schema, name: relation.name, type: relation.kind, score });
    }
    for (const object of entry.others) {
      const score = nameScore(q, object.name);
      if (score !== null) hits.push({ kind: "schemaObject", connectionId: entry.connectionId, schema: entry.schema, name: object.name, type: object.kind, detail: object.detail ?? null, score });
    }
    if (!options.columns) continue;
    for (const column of entry.columns ?? []) {
      let score = nameScore(q, column.name);
      if (score === null && tablePart !== null && columnPart !== null) {
        const table = nameScore(tablePart, column.table);
        const name = columnPart ? nameScore(columnPart, column.name) : 3;
        if (table !== null && name !== null) score = Math.max(table, name);
      }
      if (score !== null) hits.push({ kind: "column", connectionId: entry.connectionId, schema: entry.schema, name: column.table, column: column.name, type: column.nativeType, score });
    }
  }
  const order: Record<NameHitKind, number> = { relation: 0, schemaObject: 1, column: 2 };
  hits.sort((a, b) =>
    a.score - b.score
    || order[a.kind] - order[b.kind]
    || (a.column ?? a.name).localeCompare(b.column ?? b.name)
    || a.name.localeCompare(b.name)
    || a.connectionId.localeCompare(b.connectionId)
    || a.schema.localeCompare(b.schema));
  return { hits: hits.slice(0, Math.max(0, options.limit)), total: hits.length };
}

/** The schemas whose columns have not been read yet. */
export function schemasWithoutColumns(schemas: SearchCatalogSchema[]): SearchCatalogSchema[] {
  return schemas.filter((entry) => entry.columns === undefined);
}

/** The tables and views a data search can choose from, sorted by schema and name. */
export function searchableTables(schemas: SearchCatalogSchema[], connectionId: string): { schema: string; table: string }[] {
  return schemas
    .filter((entry) => entry.connectionId === connectionId)
    .flatMap((entry) => entry.relations.map((relation) => ({ schema: entry.schema, table: relation.name })))
    .sort((a, b) => a.schema.localeCompare(b.schema) || a.table.localeCompare(b.table));
}
