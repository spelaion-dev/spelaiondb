// What the explorer has already read of each schema (SDB-18): the command palette lists these
// objects and never asks the database for more. A schema node publishes its objects when it
// reads them and forgets them when it goes away (collapsed connection, disconnect, refresh), so
// the palette shows what the tree shows, with the tree's filters (ADR-025).
// Metadata search (ADR-043, dbSearch.ts) also keeps each schema's columns here, read once when
// it first needs them: they go with the schema, so a refresh reads them again.

import { create } from "zustand";
import type { ObjectItem, SchemaColumnItem, SchemaObjectItem } from "./api";

export interface CatalogSchema {
  connectionId: string;
  schema: string;
  relations: ObjectItem[];
  others: SchemaObjectItem[];
  /** Every column of the schema's relations; absent until metadata search reads them. */
  columns?: SchemaColumnItem[];
}

interface CatalogStore {
  schemas: Record<string, CatalogSchema>;
  publish(entry: CatalogSchema): void;
  forget(connectionId: string, schema: string): void;
  /** Keeps the columns read for a schema that is still published (a forgotten one stays so). */
  setColumns(connectionId: string, schema: string, columns: SchemaColumnItem[]): void;
}

const keyOf = (connectionId: string, schema: string) => `${connectionId}\u0000${schema}`;

export const useCatalog = create<CatalogStore>((set) => ({
  schemas: {},
  publish: (entry) => set((s) => ({ schemas: { ...s.schemas, [keyOf(entry.connectionId, entry.schema)]: entry } })),
  setColumns: (connectionId, schema, columns) =>
    set((s) => {
      const key = keyOf(connectionId, schema);
      const entry = s.schemas[key];
      if (!entry) return s;
      return { schemas: { ...s.schemas, [key]: { ...entry, columns } } };
    }),
  forget: (connectionId, schema) =>
    set((s) => {
      const key = keyOf(connectionId, schema);
      if (!(key in s.schemas)) return s;
      const schemas = { ...s.schemas };
      delete schemas[key];
      return { schemas };
    }),
}));
