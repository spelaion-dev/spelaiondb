import mysql from "../assets/engines/mysql-original.svg";
import postgres from "../assets/engines/postgresql-original.svg";
import mariadb from "../assets/engines/mariadb.svg";
import sqlite from "../assets/engines/sqlite.svg";
import duckdb from "../assets/engines/duckdb.svg";
import sqlserver from "../assets/engines/microsoftsqlserver.svg";
import oracle from "../assets/engines/oracle.svg";
import cockroachdb from "../assets/engines/cockroachlabs.svg";
import clickhouse from "../assets/engines/clickhouse.svg";
import snowflake from "../assets/engines/snowflake.svg";
import bigquery from "../assets/engines/googlebigquery.svg";
import mongodb from "../assets/engines/mongodb.svg";
import redis from "../assets/engines/redis.svg";
import cassandra from "../assets/engines/apachecassandra.svg";

const icons: Record<string, string> = { mysql, postgres, mariadb, sqlite, duckdb, sqlserver, oracle, cockroachdb, clickhouse, snowflake, bigquery, mongodb, redis, cassandra };

/** Engine marks are decorative: the engine name is already present in the surrounding UI.
 *  `fallback` is the engine whose mark stands in when `engine` has none of its own (a server
 *  reached through PostgreSQL's protocol shows PostgreSQL's). */
export function EngineIcon({ engine, fallback }: { engine: string | null; fallback?: string | null }) {
  const shown = engine && icons[engine] ? engine : fallback ?? null;
  const src = shown ? icons[shown] : undefined;
  return src
    ? <img className={`engine-icon ${shown === "sqlserver" || shown === "oracle" ? "colored" : ""}`} src={src} alt="" aria-hidden="true" />
    : <span className="engine-icon engine-icon-fallback" aria-hidden="true">●</span>;
}
