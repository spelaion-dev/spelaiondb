import { useEffect, useState } from "react";
import { commands, type EngineItem } from "./api";

// Two-letter engine badges, shared by the sidebar and the "add connection" catalog. Unique
// within the catalog; engines not listed fall back to their name's first letters.

const BADGES: Record<string, string> = {
  postgres: "PG",
  sqlite: "SQ",
  mysql: "MY",
  mariadb: "MA",
  sqlserver: "MS",
  oracle: "OR",
  cockroachdb: "CR",
  duckdb: "DK",
  clickhouse: "CH",
  snowflake: "SF",
  bigquery: "BQ",
  mongodb: "MG",
  redis: "RD",
  cassandra: "CA",
};

export function engineBadge(id: string, name?: string): string {
  return BADGES[id] ?? (name ?? id).slice(0, 2).toUpperCase();
}

let catalog: Promise<EngineItem[]> | null = null;

/** The engine catalog from the core, read once: what each engine is and can do. */
export function engineCatalog(): Promise<EngineItem[]> {
  catalog ??= commands.engineCatalog().catch((e: unknown) => {
    catalog = null;
    throw e;
  });
  return catalog;
}

/** What the core says about one engine, once the catalog is read. */
export function useEngineInfo(id: string | undefined): EngineItem | null {
  const [info, setInfo] = useState<EngineItem | null>(null);
  useEffect(() => {
    setInfo(null);
    if (!id) return;
    let current = true;
    engineCatalog()
      .then((all) => {
        if (current) setInfo(all.find((engine) => engine.id === id) ?? null);
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [id]);
  return info;
}
