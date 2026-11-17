// Incomplete SQL is normal here. This tokenizer and scope resolver only suggest text;
// execution, parsing and every safety decision remain in the core (ADR-010).
import { MariaSQL, MSSQL, MySQL, PostgreSQL, SQLite, keywordCompletionSource, schemaCompletionSource, type SQLDialect } from "@codemirror/lang-sql";
import { syntaxTree } from "@codemirror/language";
import type { Completion, CompletionContext, CompletionResult } from "@codemirror/autocomplete";
import type { ForeignKeyDetails } from "./bindings";

/** A column as the catalog describes it, for the editor's hover. */
export interface CompletionColumn {
  name: string;
  /** Its type as the engine writes it. */
  type: string;
  nullable: boolean;
  comment: string | null;
}
export interface CompletionTable {
  schema: string;
  name: string;
  columns: string[];
  foreignKeys: ForeignKeyDetails[];
  /** `table`, `view`…, as the explorer lists it. */
  kind?: string;
  /** Each column of `columns` with its type; absent until the table has been described. */
  details?: CompletionColumn[];
}
export interface CompletionCatalog {
  tables: CompletionTable[];
  defaultSchema: string | null;
}
export const EMPTY_CATALOG: CompletionCatalog = { tables: [], defaultSchema: null };
export type KeywordCase = "upper" | "lower" | "preserve";
export function sqlDialect(engine: string | null): SQLDialect {
  // ClickHouse: MySQL's keywords and backticks are the closest CodeMirror has.
  return ({ sqlite: SQLite, mysql: MySQL, mariadb: MariaSQL, sqlserver: MSSQL, clickhouse: MySQL } as Record<string, SQLDialect>)[engine ?? ""] ?? PostgreSQL;
}

interface Token { text: string; value: string; from: number; to: number; kind: "word" | "quoted" | "literal" | "comment" | "symbol"; closed: boolean; match: number }
const wordStart = /[\p{L}_]/u;
const wordPart = /[\p{L}\p{N}_$]/u;
const upper = (token: Token | undefined) => token?.kind === "word" ? token.value.toUpperCase() : "";
const identifier = (token: Token | undefined): token is Token & { kind: "word" | "quoted" } => token?.kind === "word" || token?.kind === "quoted";
const boundaries = new Set("SELECT FROM JOIN INNER LEFT RIGHT FULL CROSS OUTER NATURAL ON USING WHERE GROUP ORDER HAVING LIMIT OFFSET FETCH UNION EXCEPT INTERSECT RETURNING SET VALUES INTO UPDATE DELETE INSERT WINDOW QUALIFY FOR WHEN THEN ELSE END AS WITH".split(" "));

/** Comments/literals are tokens too: an unfinished quote must never expose fake SQL. */
function tokenize(sql: string, engine: string | null): Token[] {
  const tokens: Token[] = [];
  const mysql = engine === "mysql" || engine === "mariadb";
  // ClickHouse: backslash escapes in every quote, `# ` and `#!` comments, `$tag$` heredocs.
  const clickhouse = engine === "clickhouse";
  const nestedComments = !mysql && engine !== "sqlite";
  for (let i = 0; i < sql.length;) {
    if (/\s/.test(sql[i]!)) { i++; continue; }
    const from = i;
    let kind: Token["kind"] = "symbol", value = "", closed = true;
    if (sql.startsWith("--", i) && (!mysql || /[\s\x00-\x1f\x7f]/.test(sql[i + 2] ?? "")) || mysql && sql[i] === "#"
      || clickhouse && sql[i] === "#" && (sql[i + 1] === " " || sql[i + 1] === "!")) {
      kind = "comment";
      while (i < sql.length && sql[i] !== "\n") i++;
      closed = i < sql.length;
    } else if (sql.startsWith("/*", i)) {
      kind = "comment"; i += 2;
      let depth = 1;
      while (i < sql.length && depth) {
        if (sql.startsWith("/*", i) && nestedComments) { depth++; i += 2; }
        else if (sql.startsWith("*/", i)) { depth--; i += 2; }
        else i++;
      }
      closed = depth === 0;
    } else if (sql[i] === "$" && (engine === null || engine === "postgres" || engine === "duckdb" || clickhouse) && /^\$(?:[A-Za-z_][\w]*)?\$/.test(sql.slice(i))) {
      kind = "literal";
      const tag = sql.slice(i).match(/^\$(?:[A-Za-z_][\w]*)?\$/)![0];
      const end = sql.indexOf(tag, i + tag.length); closed = end >= 0;
      i = closed ? end + tag.length : sql.length;
    } else if (sql[i] === "'" || sql[i] === '"' || sql[i] === "`" || sql[i] === "[" && (engine === "sqlserver" || engine === "sqlite")) {
      const quote = sql[i++], end = quote === "[" ? "]" : quote;
      kind = quote === "'" || quote === '"' && mysql ? "literal" : "quoted";
      closed = false;
      while (i < sql.length) {
        if (sql[i] === end) {
          if (sql[i + 1] === end) { value += end; i += 2; }
          else { i++; closed = true; break; }
        } else if (sql[i] === "\\" && (mysql || clickhouse || kind === "literal" && /[eE]/.test(sql[from - 1] ?? ""))) {
          i++; value += sql[i++] ?? "";
        } else value += sql[i++];
      }
    } else if (wordStart.test(sql[i]!)) {
      kind = "word"; i++;
      while (i < sql.length && wordPart.test(sql[i]!)) i++;
      value = sql.slice(from, i);
    } else { value = sql[i++]!; }
    tokens.push({ text: sql.slice(from, i), value, from, to: i, kind, closed, match: -1 });
  }
  return tokens;
}

/** `parts`: the name as written in FROM (`schema.table`), when the source is a named relation;
 *  `cte`: the name is a CTE of the statement, which shadows any table of that name. */
interface Source { alias: Token; table?: CompletionTable; query?: Scope; names?: string[]; at: number; parts?: Token[]; cte?: boolean }
interface Scope { start: number; end: number; top: number[]; sources: Source[]; ctes: Map<string, Source>; parent?: Scope; derived: boolean }

/** The statement around `pos` as completion reads it: its tokens, its scopes (queries, CTEs,
 *  subqueries) with the relations each one names, and what is visible at `pos`. Completion,
 *  Ctrl+click and hover all resolve names from this one reading. */
interface Analysis {
  tokens: Token[];
  at(index: number | undefined): Token;
  key(token: Token): string;
  matches(token: Token, name: string): boolean;
  scopes: Scope[];
  active: Scope;
  /** The relations visible at `pos`, the nearest scope's first. */
  visible: Source[];
  columns(source: Source): string[];
  /** The table a written name (`table` or `schema.table`) is in the catalog, if exactly one. */
  tableNamed(parts: Token[]): CompletionTable | undefined;
}

function analyze(sql: string, pos: number, catalog: CompletionCatalog, engine: string | null, onLimit?: () => void): Analysis | null {
  // Completion must stay cheap even for an accidentally pasted dump. No server fallback.
  if (sql.length > 300_000) { onLimit?.(); return null; }
  const all = tokenize(sql, engine);
  const inside = all.find((t) => t.from < pos && (t.to > pos || t.to === pos && !t.closed));
  if (inside && (inside.kind === "literal" || inside.kind === "comment")) return null;
  let tokens = all.filter((t) => t.kind !== "comment");
  let begin = 0, end = tokens.length;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    const separator = t.text === ";" || engine === "sqlserver" && upper(t) === "GO" && /^\s*GO\s*$/i.test(sql.slice(sql.lastIndexOf("\n", t.from - 1) + 1, sql.indexOf("\n", t.to) < 0 ? sql.length : sql.indexOf("\n", t.to)));
    if (separator) { if (t.to <= pos) begin = i + 1; else { end = i; break; } }
  }
  tokens = tokens.slice(begin, end);
  const missing: Token = { text: "", value: "", from: sql.length, to: sql.length, kind: "symbol", closed: true, match: tokens.length };
  const at = (index: number | undefined): Token => index === undefined ? missing : tokens[index] ?? missing;
  const stack: number[] = [];
  for (let i = 0; i < tokens.length; i++) {
    if (at(i).text === "(") { stack.push(i); if (stack.length > 64) { onLimit?.(); return null; } }
    if (at(i).text === ")" && stack.length) { const open = stack.pop()!; at(open).match = i; at(i).match = open; }
  }
  for (const open of stack) at(open).match = tokens.length;
  const key = (t: Token) => t.kind === "quoted" && engine === "postgres" ? t.value : t.value.toLowerCase();
  const matches = (t: Token, name: string) => engine === "postgres" ? (t.kind === "quoted" ? t.value : t.value.toLowerCase()) === name : t.value.toLowerCase() === name.toLowerCase();
  const topOf = (start: number, end: number) => {
    const out: number[] = [];
    for (let i = start; i < end; i++) { out.push(i); if (at(i).text === "(" && at(i).match >= i) i = at(i).match; }
    return out;
  };
  const tableNamed = (parts: Token[]) => {
    const candidates = catalog.tables.filter((table) => matches(parts[parts.length - 1]!, table.name) && (parts.length < 2 || matches(parts[parts.length - 2]!, table.schema)));
    return candidates.find((table) => table.schema === catalog.defaultSchema) ?? (candidates.length === 1 ? candidates[0] : undefined);
  };
  const scopes: Scope[] = [];
  let scopeLimit = false;
  function parse(start: number, end: number, inherited: Map<string, Source>, parent?: Scope, derived = false): Scope {
    const scope: Scope = { start, end, top: topOf(start, end), sources: [], ctes: new Map(inherited), parent, derived };
    scopes.push(scope);
    if (scopes.length > 256) { scopeLimit = true; return scope; }
    let cursor = start;
    if (upper(at(cursor)) === "WITH") {
      cursor++;
      const recursive = upper(at(cursor)) === "RECURSIVE";
      if (recursive) cursor++;
      while (cursor < end && identifier(at(cursor))) {
        const alias = at(cursor++);
        let names: string[] | undefined;
        if (at(cursor)?.text === "(") {
          names = tokens.slice(cursor + 1, at(cursor).match).filter(identifier).map((t) => t.value);
          cursor = at(cursor).match + 1;
        }
        if (upper(at(cursor)) !== "AS") break;
        cursor++;
        if (upper(at(cursor)) === "NOT") cursor++;
        if (upper(at(cursor)) === "MATERIALIZED") cursor++;
        if (at(cursor)?.text !== "(") break;
        const source: Source = { alias, names, at: cursor };
        if (recursive) scope.ctes.set(key(alias), source);
        source.query = parse(cursor + 1, at(cursor).match, scope.ctes, scope, true);
        scope.ctes.set(key(alias), source);
        cursor = at(cursor).match + 1;
        if (at(cursor)?.text !== ",") break;
        cursor++;
      }
    }
    const mainTop = topOf(cursor, end);
    // Set operations have separate relation scopes. CTEs remain visible in every arm.
    const union = mainTop.find((i) => ["UNION", "EXCEPT", "INTERSECT"].includes(upper(at(i))));
    if (union !== undefined) {
      scope.top = [];
      parse(cursor, union, scope.ctes, scope, true);
      let next = union + 1;
      if (["ALL", "DISTINCT"].includes(upper(at(next)))) next++;
      parse(next, end, scope.ctes, scope, true);
      return scope;
    }
    scope.top = mainTop;
    let inFrom = false, expectSource = false;
    const parsedChildren = new Set<number>();
    for (let n = 0; n < mainTop.length; n++) {
      const i = mainTop[n]!, t = at(i), kw = upper(t);
      if (["FROM", "JOIN", "UPDATE", "INTO"].includes(kw)) { inFrom = true; expectSource = true; continue; }
      if (["WHERE", "GROUP", "ORDER", "HAVING", "SET", "VALUES", "RETURNING", "QUALIFY", "LIMIT", "ON", "USING"].includes(kw)) { inFrom = false; expectSource = false; }
      if (inFrom && t.text === ",") { expectSource = true; continue; }
      if (!expectSource || ["LATERAL", "ONLY"].includes(kw)) continue;
      expectSource = false;
      let table: CompletionTable | undefined, query: Scope | undefined, cte: Source | undefined;
      let last = n, parts: Token[] | undefined;
      if (t.text === "(" && ["SELECT", "WITH"].includes(upper(at(i + 1)))) {
        query = parse(i + 1, t.match, scope.ctes, scope, upper(at(i - 1)) !== "LATERAL"); parsedChildren.add(i);
      } else if (identifier(t)) {
        parts = [t];
        while (at(mainTop[last + 1])?.text === "." && identifier(at(mainTop[last + 2]))) { parts.push(at(mainTop[last + 2])); last += 2; }
        if (parts.length === 1) cte = scope.ctes.get(key(t));
        if (!cte) table = tableNamed(parts);
      } else continue;
      let alias = at(mainTop[last]), after = last + 1;
      if (upper(at(mainTop[after])) === "AS") after++;
      const maybeAlias = at(mainTop[after]);
      if (identifier(maybeAlias) && !boundaries.has(upper(maybeAlias))) { alias = maybeAlias; last = after; }
      if (alias && (table || query || cte)) scope.sources.push({ alias, table, query: query ?? cte?.query, names: cte?.names, at: i, parts, cte: cte !== undefined });
      n = last;
    }
    for (let i = cursor; i < end; i++) {
      if (at(i).text !== "(") continue;
      if (parsedChildren.has(i)) { i = at(i).match; continue; }
      if (["SELECT", "WITH"].includes(upper(at(i + 1)))) { parse(i + 1, at(i).match, scope.ctes, scope); i = at(i).match; }
    }
    return scope;
  }
  const root = parse(0, tokens.length, new Map());
  if (scopeLimit) { onLimit?.(); return null; }
  const atPos = (scope: Scope) => (at(scope.start)?.from ?? 0) <= pos && pos <= (at(scope.end)?.from ?? sql.length);
  const active = scopes.filter(atPos).sort((a, b) => (a.end - a.start) - (b.end - b.start))[0] ?? root;
  function columns(source: Source, seen = new Set<Scope>()): string[] {
    if (source.names) return source.names;
    if (source.table) return source.table.columns;
    const scope = source.query;
    if (!scope || seen.has(scope)) return [];
    seen = new Set(seen).add(scope);
    if (!scope.top.length) {
      const first = scopes.find((child) => child.parent === scope);
      return first ? columns({ alias: source.alias, query: first, at: 0 }, seen) : [];
    }
    const select = scope.top.findIndex((i) => upper(at(i)) === "SELECT");
    if (select < 0) return [];
    const projection: number[][] = [[]];
    for (const i of scope.top.slice(select + 1)) {
      if (upper(at(i)) === "FROM") break;
      if (["DISTINCT", "ALL"].includes(upper(at(i)))) continue;
      if (at(i).text === ",") projection.push([]); else projection[projection.length - 1]!.push(i);
    }
    return projection.flatMap((indices) => {
      const ts = indices.map((i) => at(i));
      const as = ts.findIndex((t) => upper(t) === "AS");
      if (as >= 0 && identifier(ts[as + 1])) return [ts[as + 1]!.value];
      if (ts.at(-1)?.text === "*") {
        const sources = ts.length === 1 ? scope.sources : scope.sources.filter((s) => matches(ts[0]!, s.alias.value));
        return sources.flatMap((s) => columns(s, seen));
      }
      const last = ts.at(-1), previous = ts.at(-2);
      // A bare alias follows an expression, not its final operator (a + b has no alias).
      if (identifier(last) && (ts.length === 1 || previous?.text === "." || previous && previous.to < last.from && (identifier(previous) || previous.text === "(" || previous.kind === "literal" || /\d/.test(previous.text)))) return [last.value];
      return [];
    });
  }
  const visible: Source[] = [], aliases = new Set<string>();
  for (let scope: Scope | undefined = active; scope; scope = scope.derived ? undefined : scope.parent) {
    for (const source of scope.sources) if (!aliases.has(key(source.alias))) { visible.push(source); aliases.add(key(source.alias)); }
  }
  return { tokens, at, key, matches, scopes, active, visible, columns, tableNamed };
}

export function completeSql(sql: string, pos: number, catalog: CompletionCatalog, engine: string | null, keywordCase: KeywordCase, explicit = true, onLimit?: () => void): CompletionResult | null {
  const analysis = analyze(sql, pos, catalog, engine, onLimit);
  if (!analysis) return null;
  const { tokens, at, key, matches, active, visible, columns } = analysis;
  const current = tokens.find((t) => identifier(t) && t.from < pos && pos <= t.to);
  const from = current?.from ?? pos;
  const before = tokens.filter((t) => t.to <= from);
  const prefix = current ? sql.slice(current.from, pos) : "";
  if (!explicit && !prefix && before.at(-1)?.text !== ".") return null;
  const keywords = (sqlDialect(engine).spec.keywords ?? "").split(/\s+/);
  const lastWord = before.filter((token) => token.kind === "word").at(-1)?.text;
  const lowerTyped = prefix ? prefix === prefix.toLowerCase() : lastWord !== undefined && lastWord === lastWord.toLowerCase();
  const lowerKeywords = keywordCase === "lower" || keywordCase === "preserve" && lowerTyped;
  const reserved = new Set(keywords.map((k) => k.toUpperCase()));
  const quote = (name: string) => {
    if (/^[a-z_][a-z0-9_$]*$/.test(name) && !reserved.has(name.toUpperCase())) return name;
    return engine === "mysql" || engine === "mariadb" || engine === "clickhouse" ? `\`${name.replaceAll("`", "``")}\`` : engine === "sqlserver" ? `[${name.replaceAll("]", "]]")}]` : `"${name.replaceAll('"', '""')}"`;
  };
  const applyName = (name: string) => quote(name);
  const options: Completion[] = [];
  const addColumn = (name: string, source: Source, qualified: boolean) => {
    const label = qualified ? `${source.alias.text}.${quote(name)}` : quote(name);
    options.push({ label, apply: qualified ? label : applyName(name), displayLabel: qualified ? label : name, type: "property", detail: source.alias.value });
  };
  const topBefore = active.top.filter((i) => at(i).to <= from);
  let clause = "", clauseAt = -1;
  for (const i of topBefore) {
    const kw = upper(at(i));
    if (["SELECT", "FROM", "JOIN", "WHERE", "ON", "USING", "HAVING", "GROUP", "ORDER", "SET", "RETURNING", "UPDATE", "INTO", "VALUES", "QUALIFY"].includes(kw)) { clause = kw; clauseAt = i; }
  }
  if (["FROM", "JOIN", "INTO", "UPDATE"].includes(clause) && ![",", "."].includes(before.at(-1)?.text ?? "") && active.sources.some((source) => source.at > clauseAt && source.alias.to <= from)) clause = "afterRelation";
  if (before.at(-1)?.text === ".") {
    const qualifier = before.at(-2);
    if (!qualifier) return null;
    const source = visible.find((source) => matches(qualifier, source.alias.value)) ?? active.ctes.get(key(qualifier));
    if (source && !["FROM", "JOIN", "INTO", "UPDATE"].includes(clause)) for (const name of columns(source)) addColumn(name, source, false);
    else for (const table of catalog.tables.filter((table) => matches(qualifier, table.schema))) options.push({ label: table.name, apply: quote(table.name), type: "class", detail: table.schema });
  } else if (["FROM", "JOIN", "INTO", "UPDATE"].includes(clause)) {
    for (const table of catalog.tables) {
      const label = table.schema === catalog.defaultSchema ? quote(table.name) : `${quote(table.schema)}.${quote(table.name)}`;
      options.push({ label, displayLabel: label, type: "class", detail: table.schema });
    }
    for (const source of active.ctes.values()) options.push({ label: source.alias.value, apply: quote(source.alias.value), type: "class" });
  } else {
    if (clause !== "afterRelation") for (const source of visible) for (const name of columns(source)) addColumn(name, source, visible.length > 1);
    if (clause === "ON") {
      const joined = active.sources.filter((s) => s.at < tokens.findIndex((t) => t.from >= from) || s.alias.to <= from).at(-1);
      if (joined) for (const source of active.sources) {
        if (source === joined || source.alias.to > joined.alias.to) continue;
        for (const [child, parent] of [[joined, source], [source, joined]] as const) {
          if (!child.table || !parent.table) continue;
          for (const fk of child.table.foreignKeys) {
            if (fk.referencedTable !== parent.table.name || (fk.referencedSchema || child.table.schema) !== parent.table.schema || !fk.columns.length || fk.columns.length !== fk.referencedColumns.length) continue;
            const label = fk.columns.map((column, i) => `${child.alias.text}.${quote(column)} = ${parent.alias.text}.${quote(fk.referencedColumns[i]!)}`).join(lowerKeywords ? " and " : " AND ");
            options.unshift({ label, type: "text", detail: fk.name, boost: 20 });
          }
        }
      }
    }
    if (!clause || clause === "SELECT" || clause === "afterRelation") {
      for (const keyword of keywords) options.push({ label: lowerKeywords ? keyword.toLowerCase() : keyword.toUpperCase(), type: "keyword" });
      if (clause !== "afterRelation") for (const builtin of (sqlDialect(engine).spec.builtin ?? "").split(/\s+/).filter(Boolean)) options.push({ label: builtin, type: "function" });
    }
  }
  const unique = [...new Map(options.map((option) => [option.label, option])).values()];
  const quotedPrefix = current?.kind === "quoted" ? current.value.slice(0, Math.max(0, pos - current.from - 1)).toLowerCase() : null;
  const filtered = quotedPrefix === null ? unique : unique.filter((option) => (option.displayLabel ?? option.label).toLowerCase().includes(quotedPrefix));
  return filtered.length ? { from, to: current?.closed ? current.to : pos, options: filtered, ...(quotedPrefix === null ? {} : { filter: false }) } : null;
}

/** What a name in the SQL text is, in the catalog already read: a table or view, or one of
 *  its columns; `from` and `to` are the name's place in the text. */
export type ResolvedName =
  | { kind: "table"; table: CompletionTable; from: number; to: number }
  | { kind: "column"; table: CompletionTable; column: string; from: number; to: number };

/** Words before a relation's name outside FROM and JOIN (`DROP TABLE t`, `REFERENCES t`). */
const beforeRelation = new Set(["TABLE", "VIEW", "REFERENCES", "TRUNCATE", "DESCRIBE", "DESC"]);

/**
 * The table or column the name at `pos` stands for (Ctrl+click and hover), read as completion
 * reads the statement: an alias leads to its table, a CTE of the statement shadows a table of
 * the same name, and a quoted name is compared as the engine compares it. Only the catalog
 * given is looked at, never the database. `null` whenever the name is not certainly one
 * thing: a table that is not in the catalog (or not read yet), a column that several visible
 * tables have, one that may come from a subquery, or a table whose columns are not read yet.
 */
export function resolveName(sql: string, pos: number, catalog: CompletionCatalog, engine: string | null): ResolvedName | null {
  const analysis = analyze(sql, pos, catalog, engine);
  if (!analysis) return null;
  const { tokens, at, matches, scopes, visible, columns, tableNamed } = analysis;
  let index = tokens.findIndex((t) => identifier(t) && t.from <= pos && pos < t.to);
  // Just after a name's last character still counts: where a click at its right edge lands.
  if (index < 0) index = tokens.findIndex((t) => identifier(t) && t.to === pos);
  if (index < 0) return null;
  const token = tokens[index]!;
  if (!token.closed) return null;
  const touching = (a: Token, b: Token) => a.to === b.from;
  // The dotted name the token is part of: `schema.table.column`, written without spaces.
  let first = index, last = index;
  while (first >= 2 && at(first - 1).text === "." && identifier(at(first - 2)) && touching(at(first - 2), at(first - 1)) && touching(at(first - 1), at(first))) first -= 2;
  while (at(last + 1).text === "." && identifier(at(last + 2)) && touching(at(last), at(last + 1)) && touching(at(last + 1), at(last + 2))) last += 2;
  const chain: Token[] = [];
  for (let i = first; i <= last; i += 2) chain.push(at(i));
  const place = (index - first) / 2;
  const asTable = (table: CompletionTable | undefined, from: Token, to: Token): ResolvedName | null => table ? { kind: "table", table, from: from.from, to: to.to } : null;
  const columnOf = (table: CompletionTable, name: Token): ResolvedName | null => {
    const column = table.columns.find((c) => matches(name, c));
    return column === undefined ? null : { kind: "column", table, column, from: name.from, to: name.to };
  };
  // A relation as written in FROM, JOIN, UPDATE or INTO, or its alias where it is declared.
  // A CTE's name is the CTE, whatever table has that name.
  for (const scope of scopes) for (const source of scope.sources) {
    if (source.parts?.includes(token)) return source.cte ? null : asTable(source.table, source.parts[0]!, source.parts[source.parts.length - 1]!);
    if (source.alias === token) return source.cte || source.query ? null : asTable(source.table, token, token);
  }
  // `DROP TABLE schema.name`, `REFERENCES name`: a relation named outside a query.
  let before = first - 1;
  if (upper(at(before)) === "EXISTS") { before--; if (upper(at(before)) === "NOT") before--; if (upper(at(before)) === "IF") before--; }
  if (first > 0 && beforeRelation.has(upper(at(before)))) {
    return asTable(tableNamed(chain), chain[0]!, chain[chain.length - 1]!);
  }
  if (chain.length > 1) {
    // `alias.column`, `table.column`, `schema.table.column`: the relation is what is before
    // the last dot, as an alias visible here or as a table's own name.
    const qualifier = chain.slice(0, -1);
    const source = qualifier.length === 1
      ? visible.find((s) => matches(qualifier[0]!, s.alias.value))
      : visible.find((s) => s.table !== undefined && s.table === tableNamed(qualifier));
    if (!source || source.cte || source.query || !source.table) return null;
    if (place === chain.length - 1) return columnOf(source.table, token);
    // The relation's own name before the dot (not its schema).
    return place === chain.length - 2 ? asTable(source.table, token, token) : null;
  }
  // A bare name: a column, if exactly one visible table has it and nothing else could.
  if (at(index + 1).text === "(" || !visible.length) return null;
  const derived = visible.filter((s) => s.cte || s.query || !s.table);
  if (derived.some((s) => columns(s).some((c) => matches(token, c)))) return null;
  const tables = visible.flatMap((s) => (s.table && !s.cte && !s.query ? [s.table] : []));
  // A table still being read has no columns yet: the name may be one of its own.
  if (tables.some((t) => t.details === undefined)) return null;
  const owners = [...new Set(tables.filter((t) => t.columns.some((c) => matches(token, c))))];
  return owners.length === 1 ? columnOf(owners[0]!, token) : null;
}

export function sqlCompletionSource(catalog: CompletionCatalog, engine: string | null, keywordCase: KeywordCase) {
  const dialect = sqlDialect(engine);
  const schema: Record<string, Record<string, string[]>> = Object.create(null);
  for (const table of catalog.tables) {
    schema[table.schema] ??= Object.create(null) as Record<string, string[]>;
    schema[table.schema]![table.name] = table.columns;
  }
  // These built-in CodeMirror sources are synchronous. Keep basic completion for large or
  // unusually deeply nested statements instead of losing the editor's previous behavior.
  const basicSchema = schemaCompletionSource({ dialect, schema, defaultSchema: catalog.defaultSchema ?? undefined });
  const basicUpper = keywordCompletionSource(dialect, true), basicLower = keywordCompletionSource(dialect, false);
  return (context: CompletionContext) => {
    let from = 0, to = context.state.doc.length;
    if (to > 300_000) {
      // CodeMirror's incremental syntax tree can isolate a small current statement after a
      // very large script without rescanning/copying the whole document on each keystroke.
      let node = syntaxTree(context.state).resolveInner(context.pos, -1);
      while (node.parent && node.name !== "Statement") node = node.parent;
      if (node.name === "Statement") { from = node.from; to = node.to; }
    }
    let limited = to - from > 300_000;
    const rich = limited ? null : completeSql(context.state.sliceDoc(from, to), context.pos - from, catalog, engine, keywordCase, context.explicit, () => { limited = true; });
    if (!limited) return rich && { ...rich, from: rich.from + from, ...(rich.to === undefined ? {} : { to: rich.to + from }) };
    const prefix = context.matchBefore(/[\w$]+/)?.text;
    const lower = keywordCase === "lower" || keywordCase === "preserve" && prefix !== undefined && prefix === prefix.toLowerCase();
    const names = basicSchema(context) as CompletionResult | null;
    const words = (lower ? basicLower : basicUpper)(context) as CompletionResult | null;
    return names && words && names.from === words.from ? { ...names, options: [...names.options, ...words.options] } : names ?? words;
  };
}

/** A conservative cache invalidation hint, never a statement classifier or permission check. */
export function mayChangeCompletionCatalog(sql: string, engine: string | null): boolean {
  return tokenize(sql, engine).some((token) => ["CREATE", "ALTER", "DROP", "RENAME", "ATTACH", "DETACH", "USE", "SET"].includes(upper(token)));
}
