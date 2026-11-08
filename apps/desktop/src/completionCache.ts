import type { CompletionCatalog, CompletionTable } from "./sqlCompletion";

export interface CompletionSnapshot { catalog: CompletionCatalog; loading: boolean; error: string | null }
export interface CompletionReader {
  open(): Promise<void>;
  names(cancelled: () => boolean): Promise<{ tables: CompletionTable[]; defaultSchema: string | null }>;
  describe(table: CompletionTable): Promise<CompletionTable>;
  /** Cancels only this reader's dedicated metadata session, never the editor's session. */
  cancel(): Promise<void>;
  close(): Promise<void>;
}
interface Entry { snapshot: CompletionSnapshot; listeners: Set<(value: CompletionSnapshot) => void>; cancelled: boolean; reader: CompletionReader }

/** One background reader per immutable source session and database. Never called by a completion source.
 * Releasing the last consumer drops the entry: a reconnect must not reuse the old server's schema.
 * A generation in the key makes refresh/DDL readers independent of stale in-flight replies. */
export class CompletionCache {
  private entries = new Map<string, Entry>();

  acquire(key: string, reader: CompletionReader, listener: (value: CompletionSnapshot) => void): () => void {
    let entry = this.entries.get(key);
    if (!entry) {
      entry = { snapshot: { catalog: { tables: [], defaultSchema: null }, loading: true, error: null }, listeners: new Set(), cancelled: false, reader };
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

  private publish(entry: Entry, snapshot: CompletionSnapshot) {
    if (entry.cancelled) return;
    entry.snapshot = snapshot;
    for (const listener of entry.listeners) listener(snapshot);
  }

  private async load(entry: Entry, reader: CompletionReader) {
    try {
      await reader.open();
      if (entry.cancelled) return;
      const catalog = await reader.names(() => entry.cancelled);
      this.publish(entry, { catalog, loading: true, error: null });
      // One describe at a time, on an independent metadata session, never the editor actor.
      // Names work immediately; columns/FKs appear incrementally, without blocking typing.
      for (let i = 0; i < catalog.tables.length && !entry.cancelled; i++) {
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
