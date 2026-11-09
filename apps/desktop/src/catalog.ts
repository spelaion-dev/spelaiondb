// What the explorer has already read of each schema (SDB-18): the command palette lists these
// objects and never asks the database for more. A schema node publishes its objects when it
// reads them and forgets them when it goes away (collapsed connection, disconnect, refresh), so
// the palette shows what the tree shows, with the tree's filters (ADR-025).

import { create } from "zustand";
import type { ObjectItem, SchemaObjectItem } from "./api";

export interface CatalogSchema {
  connectionId: string;
  schema: string;
  relations: ObjectItem[];
  others: SchemaObjectItem[];
}

interface CatalogStore {
  schemas: Record<string, CatalogSchema>;
  publish(entry: CatalogSchema): void;
  forget(connectionId: string, schema: string): void;
}

const keyOf = (connectionId: string, schema: string) => `${connectionId}\u0000${schema}`;

export const useCatalog = create<CatalogStore>((set) => ({
  schemas: {},
  publish: (entry) => set((s) => ({ schemas: { ...s.schemas, [keyOf(entry.connectionId, entry.schema)]: entry } })),
  forget: (connectionId, schema) =>
    set((s) => {
      const key = keyOf(connectionId, schema);
      if (!(key in s.schemas)) return s;
      const schemas = { ...s.schemas };
      delete schemas[key];
      return { schemas };
    }),
}));
