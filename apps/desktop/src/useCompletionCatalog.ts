import { useEffect, useMemo, useState } from "react";
import { call, commands } from "./api";
import { CompletionCache, completionCacheKey, type CompletionSnapshot } from "./completionCache";
import { EMPTY_CATALOG } from "./sqlCompletion";
import { SCHEMAS_REFRESHED_EVENT, EXPLORER_REFRESH_EVENT } from "./connect";

const cache = new CompletionCache();
const generations = new Map<string, number>();
const refreshEvents = new WeakMap<Event, number>();
const initial: CompletionSnapshot = { catalog: EMPTY_CATALOG, loading: false, error: null };

/** The same unfiltered catalog operations the explorer uses (ADR-025). The clone keeps
 * metadata reads off the user's actor/transaction, and takes credentials only inside Rust. */
export function useCompletionCatalog(session: string, connection: string | null, workspace: string | null, database: string | null, connected: boolean) {
  const [snapshot, setSnapshot] = useState<CompletionSnapshot>(initial);
  const [defaultSchema, setDefaultSchema] = useState<string | null>(null);
  const [generation, setGeneration] = useState(() => generations.get(connection ?? "") ?? 0);
  useEffect(() => {
    const refresh = (event: Event) => {
      const detail: unknown = (event as CustomEvent).detail;
      const id = typeof detail === "string" ? detail : (detail as { connectionId?: string } | null)?.connectionId;
      if (id === connection) {
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
      open: async () => { await call(commands.connectFromSession(session, metadata, database, false)); opened = true; },
      names: async (cancelled) => {
        const defaultSchema = await call(commands.activeSchema(metadata));
        const schemas = (await call(commands.listSchemas(metadata, null, false))).filter((schema) => !schema.system);
        const tables = [];
        for (const schema of schemas) {
          if (cancelled()) break;
          for (const table of await call(commands.listObjects(metadata, schema.name, null, false))) {
            tables.push({ schema: table.schema, name: table.name, columns: [] as string[], foreignKeys: [] });
          }
        }
        return { tables, defaultSchema: defaultSchema ?? database ?? "main" };
      },
      describe: async (table) => {
        const details = await call(commands.describeTable(metadata, table.schema, table.name, null));
        return { ...table, columns: details.columns.map((column) => column.name), foreignKeys: details.foreignKeys };
      },
      cancel: async () => { if (opened && !closing) await call(commands.cancel(metadata)); },
      close,
    }, setSnapshot);
  }, [session, connection, workspace, database, connected, generation]);
  const catalog = useMemo(() => ({ ...snapshot.catalog, defaultSchema: defaultSchema ?? snapshot.catalog.defaultSchema }), [snapshot.catalog, defaultSchema]);
  return { ...snapshot, catalog };
}
