// What the explorer has already read of each schema (SDB-18): the command palette lists these
// objects and never asks the database for more. A schema node publishes its objects when it
// reads them and forgets them when it goes away (collapsed connection, disconnect, refresh), so
// the palette shows what the tree shows, with the tree's filters (ADR-025).
// Metadata search (ADR-043, dbSearch.ts) also keeps each schema's columns here, read once when
// it first needs them: they go with the schema, so a refresh reads them again.

import { create } from "zustand";
import type { NameFilter, ObjectItem, SchemaColumnItem, SchemaObjectItem } from "./api";
import { connectionTag, type ConnectionKey } from "./connectionIdentity.ts";

export interface CatalogSchema {
  /** The connection's identity (ADR-075): two folders' connections of the same id are apart. */
  connection: ConnectionKey;
  schema: string;
  relations: ObjectItem[];
  others: SchemaObjectItem[];
  /** The connection's object filter the schema was read with (ADR-025): the palette's schema
   *  diagram draws what the tree's does. */
  filter?: NameFilter | null;
  /** Every column of the schema's relations; absent until metadata search reads them. */
  columns?: SchemaColumnItem[];
}

interface CatalogStore {
  schemas: Record<string, CatalogSchema>;
  publish(entry: CatalogSchema): void;
  forget(connection: ConnectionKey, schema: string): void;
  /** Keeps the columns read for a schema that is still published (a forgotten one stays so). */
  setColumns(connection: ConnectionKey, schema: string, columns: SchemaColumnItem[]): void;
}

/** The key of a schema in `schemas`, from its connection's `connectionTag` and its name. */
export const catalogKeyOfTag = (tag: string, schema: string) => JSON.stringify([tag, schema]);

/** The key of a schema in `schemas` (ADR-075: its connection's folder and id, and its name). */
export const catalogKey = (connection: ConnectionKey, schema: string) => catalogKeyOfTag(connectionTag(connection), schema);

/** What `catalogKeyOfTag` wrote, or null. */
export function catalogKeyParts(key: string): { tag: string; schema: string } | null {
  try {
    const value: unknown = JSON.parse(key);
    if (Array.isArray(value) && value.length === 2 && typeof value[0] === "string" && typeof value[1] === "string") return { tag: value[0], schema: value[1] };
  } catch {
    // Not a key.
  }
  return null;
}

/** The catalog as the metadata search reads it (`dbSearch.ts`): each connection as its tag. */
export function searchCatalog(schemas: CatalogSchema[]): (Omit<CatalogSchema, "connection"> & { connection: string })[] {
  return schemas.map((entry) => ({ ...entry, connection: connectionTag(entry.connection) }));
}

const keyOf = catalogKey;

export const useCatalog = create<CatalogStore>((set) => ({
  schemas: {},
  publish: (entry) => set((s) => ({ schemas: { ...s.schemas, [keyOf(entry.connection, entry.schema)]: entry } })),
  setColumns: (connection, schema, columns) =>
    set((s) => {
      const key = keyOf(connection, schema);
      const entry = s.schemas[key];
      if (!entry) return s;
      return { schemas: { ...s.schemas, [key]: { ...entry, columns } } };
    }),
  forget: (connection, schema) =>
    set((s) => {
      const key = keyOf(connection, schema);
      if (!(key in s.schemas)) return s;
      const schemas = { ...s.schemas };
      delete schemas[key];
      return { schemas };
    }),
}));
