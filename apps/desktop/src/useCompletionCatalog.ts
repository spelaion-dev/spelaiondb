import { useEffect, useMemo, useState } from "react";
import { call, commands } from "./api";
import { CompletionCache, completionCacheKey, type CompletionSnapshot } from "./completionCache";
import { EMPTY_CATALOG, referencedCompletionTables } from "./sqlCompletion";
import { SCHEMAS_REFRESHED_EVENT, EXPLORER_REFRESH_EVENT } from "./connect";
import { connectionTag, type ConnectionKey } from "./connectionKey";

const cache = new CompletionCache();
const generations = new Map<string, number>();
const refreshEvents = new WeakMap<Event, number>();
const initial: CompletionSnapshot = { catalog: EMPTY_CATALOG, loading: false, error: null };

/** The same unfiltered catalog operations the explorer uses (ADR-025). The clone keeps
 * metadata reads off the user's actor/transaction, and takes credentials only inside Rust.
 * `connection` is the connection's `connectionTag` (its folder and id, ADR-075), or null. */
export function useCompletionCatalog(session: string, connection: string | null, workspace: string | null, database: string | null, connected: boolean, sql: string, engine: string | null) {
  const [snapshot, setSnapshot] = useState<CompletionSnapshot>(initial);
  const [defaultSchema, setDefaultSchema] = useState<string | null>(null);
  const [prioritySql, setPrioritySql] = useState(sql);
  const [generation, setGeneration] = useState(() => generations.get(connection ?? "") ?? 0);
  useEffect(() => {
    const refresh = (event: Event) => {
      // Both events carry the connection's identity (`schemasRefreshed`, `refreshExplorer`).
      const detail = (event as CustomEvent<ConnectionKey | null>).detail;
      if (connection !== null && detail && connectionTag(detail) === connection) {
        const next = refreshEvents.get(event) ?? (generations.get(connection) ?? 0) + 1;
        refreshEvents.set(event, next);
        generations.set(connection, next); setGeneration(next);
      }
    };
    window.addEventListener(SCHEMAS_REFRESHED_EVENT, refresh);
    window.addEventListener(EXPLORER_REFRESH_EVENT, refresh);
    return () => { window.removeEventListener(SCHEMAS_REFRESHED_EVENT, refresh); window.removeEventListener(EXPLORER_REFRESH_EVENT, refresh); };
  }, [connection]);
  useEffect(() => {
    setDefaultSchema(null);
    if (!connected) return;
    let cancelled = false;
    // The shared catalog contains every schema, but unqualified names belong to this tab's
    // own search path. A metadata clone starts with the connection's default search path.
    void call(commands.activeSchema(session)).then((schema) => { if (!cancelled) setDefaultSchema(schema); }).catch(() => undefined);
    return () => { cancelled = true; };
  }, [session, connected, database, generation]);
  useEffect(() => {
    setSnapshot(initial);
    if (!connected || !connection) return;
    const metadata = `completion-${crypto.randomUUID()}`;
    let opened = false;
    let closing: Promise<void> | null = null;
    const close = async () => {
      // A release can race the connect. load's finally closes it after open finishes too.
      if (!opened) return;
      closing ??= call(commands.disconnect(metadata)).then(() => undefined);
      await closing;
    };
    return cache.acquire(completionCacheKey(workspace, connection, database, generation, session), {
      open: async () => {
        const outcome = await call(commands.connectFromSession(session, metadata, database, false));
        // Edited since that session opened, and it needs the user (SDB-426): no hints from it.
        if (outcome.kind !== "connected") throw new Error("the connection changed since its session opened");
        opened = true;
      },
      names: async (cancelled, schemaReady) => {
        const defaultSchema = await call(commands.activeSchema(metadata));
        const schemas = (await call(commands.listSchemas(metadata, null, false))).filter((schema) => !schema.system);
        schemas.sort((a, b) => Number(b.name === (defaultSchema ?? database)) - Number(a.name === (defaultSchema ?? database)));
        const tables = [];
        for (const schema of schemas) {
          if (cancelled()) break;
          const found = (await call(commands.listObjects(metadata, schema.name, null, false))).map((table) => ({ schema: table.schema, name: table.name, columns: [] as string[], foreignKeys: [], kind: table.kind }));
          tables.push(...found);
          await schemaReady(schema.name, found, defaultSchema ?? database ?? "main");
        }
        return { tables, defaultSchema: defaultSchema ?? database ?? "main" };
      },
      describe: async (table) => {
        const details = await call(commands.describeTable(metadata, table.schema, table.name, null));
        return {
          ...table,
          columns: details.columns.map((column) => column.name),
          foreignKeys: details.foreignKeys,
          // Types and comments for the editor's hover, read with the names: no hover asks the database.
          details: details.columns.map((column) => ({ name: column.name, type: column.nativeType, nullable: column.nullable, comment: column.comment ?? null })),
        };
      },
      columns: async (schema) => call(commands.listColumns(metadata, schema)),
      cancel: async () => { if (opened && !closing) await call(commands.cancel(metadata)); },
      close,
    }, setSnapshot);
  }, [session, connection, workspace, database, connected, generation]);
  const catalog = useMemo(() => ({ ...snapshot.catalog, defaultSchema: defaultSchema ?? snapshot.catalog.defaultSchema }), [snapshot.catalog, defaultSchema]);
  useEffect(() => {
    const timer = setTimeout(() => setPrioritySql(sql), 150);
    return () => clearTimeout(timer);
  }, [sql]);
  useEffect(() => {
    if (!connected || !connection) return;
    cache.prioritize(completionCacheKey(workspace, connection, database, generation, session), referencedCompletionTables(prioritySql, catalog, engine));
  }, [catalog, connected, connection, database, engine, generation, session, prioritySql, workspace]);
  const retry = () => {
    if (!connection) return;
    const next = (generations.get(connection) ?? 0) + 1;
    generations.set(connection, next);
    setGeneration(next);
  };
  return { ...snapshot, catalog, retry };
}
