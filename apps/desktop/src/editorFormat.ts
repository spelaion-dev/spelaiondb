import { MariaSQL, MSSQL, MySQL, PostgreSQL, SQLite, sql } from "@codemirror/lang-sql";
import type { SqlLanguage } from "sql-formatter";

export const FORMAT_LANGUAGES: Record<string, SqlLanguage> = {
  postgres: "postgresql", sqlite: "sqlite", mysql: "mysql", mariadb: "mariadb", duckdb: "duckdb", sqlserver: "transactsql",
};

/** Move only parsed comma punctuation, never a comma in a string, quoted identifier or
 * comment. Inline commas and commas before comments retain the formatter's layout. */
export function leadingCommas(text: string, engine: string | null): string {
  const dialects = { sqlite: SQLite, mysql: MySQL, mariadb: MariaSQL, sqlserver: MSSQL };
  const dialect = dialects[engine as keyof typeof dialects] ?? PostgreSQL;
  const edits: { from: number; to: number; insert: string }[] = [];
  sql({ dialect }).language.parser.parse(text).iterate({ enter(node) {
    if (node.name !== "Punctuation" || text.slice(node.from, node.to) !== ",") return;
    const after = /^[ \t]*\n([ \t]*)/.exec(text.slice(node.to));
    if (after) edits.push({ from: node.from, to: node.to + after[0].length, insert: "\n" + after[1] + ", " });
  } });
  for (const edit of edits.reverse()) text = text.slice(0, edit.from) + edit.insert + text.slice(edit.to);
  return text;
}
