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
