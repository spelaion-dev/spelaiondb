import type { CompletionCatalog, CompletionTable } from "./sqlCompletion";

export interface CompletionSnapshot { catalog: CompletionCatalog; loading: boolean; error: string | null }
export interface CompletionReader {
  open(): Promise<void>;
  names(cancelled: () => boolean, schemaReady: (schema: string, tables: CompletionTable[], defaultSchema: string | null) => Promise<void>): Promise<{ tables: CompletionTable[]; defaultSchema: string | null }>;
  describe(table: CompletionTable): Promise<CompletionTable>;
  /** Optional bulk column lookup for one schema; full descriptions still supply foreign keys. */
  columns?(schema: string): Promise<{ table: string; name: string }[]>;
  /** Cancels only this reader's dedicated metadata session, never the editor's session. */
  cancel(): Promise<void>;
  close(): Promise<void>;
}
interface Entry { snapshot: CompletionSnapshot; listeners: Set<(value: CompletionSnapshot) => void>; cancelled: boolean; reader: CompletionReader; priority: Set<string> }
const tableKey = (table: CompletionTable) => JSON.stringify([table.schema, table.name]);

/** One background reader per immutable source session and database. Never called by a completion source.
 * Releasing the last consumer drops the entry: a reconnect must not reuse the old server's schema.
 * A generation in the key makes refresh/DDL readers independent of stale in-flight replies. */
export class CompletionCache {
  private entries = new Map<string, Entry>();

  acquire(key: string, reader: CompletionReader, listener: (value: CompletionSnapshot) => void): () => void {
    let entry = this.entries.get(key);
    if (!entry) {
      entry = { snapshot: { catalog: { tables: [], defaultSchema: null }, loading: true, error: null }, listeners: new Set(), cancelled: false, reader, priority: new Set() };
      this.entries.set(key, entry);
      void this.load(entry, reader);
    }
    entry.listeners.add(listener);
    listener(entry.snapshot);
    const acquired = entry;
    return () => {
      if (!acquired.listeners.delete(listener)) return;
      if (!acquired.listeners.size) {
        acquired.cancelled = true; this.entries.delete(key);
        void (async () => {
          await acquired.reader.cancel().catch(() => undefined);
          await acquired.reader.close().catch(() => undefined);
        })();
      }
    };
  }

  /** Reorder only work that has not started; never interrupt a catalog query. */
  prioritize(key: string, tables: CompletionTable[]) {
    const entry = this.entries.get(key);
    if (entry) entry.priority = new Set(tables.map(tableKey));
  }

  private publish(entry: Entry, snapshot: CompletionSnapshot) {
    if (entry.cancelled) return;
    entry.snapshot = snapshot;
    for (const listener of entry.listeners) listener(snapshot);
  }

  private async load(entry: Entry, reader: CompletionReader) {
    try {
      await reader.open();
      if (entry.cancelled) return;
      const published = new Set<string>();
      const addSchema = async (schema: string, incoming: CompletionTable[], defaultSchema: string | null) => {
        if (entry.cancelled) return;
        let tables = [...entry.snapshot.catalog.tables, ...incoming];
        published.add(schema);
        this.publish(entry, { ...entry.snapshot, catalog: { tables, defaultSchema } });
        if (schema !== defaultSchema || !reader.columns) return;
        try {
          const columns = await reader.columns(schema);
          const byTable = new Map<string, string[]>();
          for (const column of columns) {
            const names = byTable.get(column.table) ?? [];
            names.push(column.name); byTable.set(column.table, names);
          }
          tables = entry.snapshot.catalog.tables.map((table) => table.schema === schema && byTable.has(table.name)
            ? { ...table, columns: byTable.get(table.name)! } : table);
          this.publish(entry, { ...entry.snapshot, catalog: { tables, defaultSchema } });
        } catch {
          // Some drivers do not support bulk metadata. Individual describes remain available.
        }
      };
      const catalog = await reader.names(() => entry.cancelled, addSchema);
      if (entry.cancelled) return;
      // Readers that do not publish incrementally (including existing consumers) still work.
      if (!published.size) {
        await addSchema(catalog.defaultSchema ?? "", catalog.tables, catalog.defaultSchema);
      } else {
        const tables = [...entry.snapshot.catalog.tables, ...catalog.tables.filter((table) => !published.has(table.schema))];
        this.publish(entry, { ...entry.snapshot, catalog: { ...catalog, tables } });
      }
      // One describe at a time, on an independent metadata session, never the editor actor.
      // Names work immediately; columns/FKs appear incrementally, without blocking typing.
      const pending = catalog.tables.map((_, index) => index);
      while (pending.length && !entry.cancelled) {
        const preferred = pending.findIndex((index) => entry.priority.has(tableKey(catalog.tables[index]!)));
        const [i] = pending.splice(preferred < 0 ? 0 : preferred, 1);
        if (i === undefined) break;
        try {
          const table = await reader.describe(catalog.tables[i]!);
          const tables = [...entry.snapshot.catalog.tables]; tables[i] = table;
          this.publish(entry, { ...entry.snapshot, catalog: { ...catalog, tables } });
        } catch (error) {
          this.publish(entry, { ...entry.snapshot, error: error instanceof Error ? error.message : String(error) });
        }
      }
    } catch (error) {
      this.publish(entry, { ...entry.snapshot, error: error instanceof Error ? error.message : String(error) });
    } finally {
      await reader.close().catch(() => undefined);
      this.publish(entry, { ...entry.snapshot, loading: false });
    }
  }
}

/** Encode parts rather than joining with a separator that a database name might contain. */
export const completionCacheKey = (workspace: string | null, connection: string, database: string | null, generation: number, session: string) => JSON.stringify([workspace, connection, database, generation, session]);
