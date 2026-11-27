# Dialects

- `crates/spelaion-dialect`: each engine's `Dialect` (statement splitting, quoting, code
  ranges), one file per engine, with their tests. Only text: no network, no native library.
  They are the core's rules, not the driver's (ADR-041): the core takes an engine's dialect
  from `for_engine` (`spelaion-core`'s `dialect_of`); a driver has none to give, and
  uses its engine's file only for what it writes itself (the names in its catalog queries,
  the words it must recognise in a statement it sends). A new engine's dialect is added here,
  with its name in `for_engine`. MySQL's splitter (`mysql.rs`) counts a statement written only
  as executable comments (how `mysqldump` writes views and triggers) as a statement, and reads
  quotes as the session's `sql_mode` does (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`):
  `Dialect::with_sql_mode` gives the session's dialect, which `spelaion-core`'s `sql_mode.rs`
  reads when the session opens and after a statement that names `sql_mode`. T-SQL batches
  (`GO`, `BEGIN … END`) are split in `sqlserver.rs`; a `DECLARE` keeps the rest of its batch,
  where its variables live (the guard still checks each statement in it). The remote
  SQLite check (`remote.rs`, ADR-065 step 6) reads SQLite's scanner through `sqlite.rs`'s
  `statement_start` and `code_line_starts`, both built from `SqliteScanner`'s regions, and keeps
  only the `sqlite3` shell's own line rules (a line starting with `.` or `#`) to itself: it is
  what keeps a dot-command away from the server's shell, so any change to SQLite's scanner
  needs `remote.rs`'s tests too.
- White space is one rule for every splitter (SDB-434): Unicode's, as `str::trim` reads it
  (a no-break space, a vertical tab, U+2028, U+3000 too). `lib.rs`'s `is_white_space_at` says
  whether a byte is white space, `white_space_before` whether a word may start after it, and
  `trimmed` makes every span, `None` for a statement of white space alone. A new dialect
  uses them, never `is_ascii_whitespace` or a trim of its own.
- `tests/split_properties.rs` (SDB-156) generates scripts from each dialect's tricky pieces
  and checks that they split back into their statements, and that any text splits without a
  panic into spans that lose nothing but comments, white space and separators (read with the
  dialect's own `code_ranges`). A new dialect gets its pieces there (`extra_atoms`, and a
  property of its own for blocks or separators); a change to a splitter keeps them passing.
  `SPELAION_FUZZ_CASES=<n>` runs a long local fuzz (see `CONTRIBUTING.md`).
  `readers_agree_with_the_splitter` (ADR-065 step 1, SDB-452) compares each dialect's readers
  with its splitter: every statement reads alike with `code_ranges` alone and in its script,
  and COPY data follows only a statement whose first word, read the splitter's way, is `COPY`.
  A disagreement it finds gets a bug card and an entry in `KNOWN`, which lets through only
  statements of that shape; the card's fix takes the entry out, and no other change to a
  dialect goes in with the test.
- ADR-065 (SDB-157): each SQL dialect gets one lexical scanner, a lazy sequence of regions
  (`scanner.rs`: `Region`, `RegionKind`) that the splitter, `code_ranges`, `copy_data` and the
  guard's fallback all read, instead of each scanning the text its own way. PostgreSQL has moved
  (SDB-453): `PostgresScanner` in `postgres.rs` is its only reader of quotes, comments, `$tag$`
  bodies, psql lines and COPY data; `split_statements`, `code_ranges` and `copy_stdio`'s word
  reader read its regions, and a new PostgreSQL rule goes into the scanner alone. `copy_data`
  reads one span's text (the scanner on that text), never the whole script, so a dump is not
  scanned once per statement. MySQL and MariaDB have moved too (SDB-454): `MysqlScanner` in
  `mysql.rs` reads quotes as the session's `sql_mode` does (`Quoting`, from `with_sql_mode`),
  `DELIMITER` lines (a `MetaLine`, code as a whole, never a statement) and executable comments
  (`ExecutableComment`, which make their statement one); `split`, `code_ranges` and
  `first_word` read its regions. The classifier's raw-text check for executable comments
  (`reject_mysql_executable_comments`, SDB-116) deliberately stays off the scanner.
  SQL Server has moved as well (SDB-455): `SqlServerScanner` in `sqlserver.rs` reads quotes,
  `[names]`, nested comments, `GO` lines (a statement end, code; a `--` comment after the `GO`
  is a comment), `BEGIN … END` blocks and the rest of a batch a `DECLARE` or a module takes;
  `split`, `code_ranges`, `first_word` and `counts_rows` read its regions, and it gives the
  guard its first word (`Dialect::first_word`). SQLite has moved (SDB-456): `SqliteScanner` in
  `sqlite.rs` reads quotes and comments through `skip_quote_or_comment` and keeps a
  `CREATE [TEMP] TRIGGER`'s `BEGIN … END` depth from the statement's first words; `split` and
  `code_ranges` read its regions.
  DuckDB has moved (SDB-456): its statements end at a `;` outside what `opening` in `duckdb.rs`
  reads (strings, `E'…'` too, quoted names, nested comments, `$tag$` bodies), so its scanner is
  `scanner.rs`'s `SemicolonScanner` given that `opening`; `split` and `code_ranges` read its
  regions through `scanner.rs`'s `statements` and `code_ranges`. A dialect whose words decide
  nothing about where a statement ends moves the same way: an `Opening` of its own, the shared
  scanner.
  Oracle has moved (SDB-456): `OracleScanner` in `oracle.rs` reads strings (`q'[…]'`, `N'…'`),
  names and comments through `opening`, ends any statement at a line holding only `/` and
  decides at a statement's first code whether it is a PL/SQL unit (`is_plsql`, whose words read
  `opening` too; the guard calls it). `skip_non_code` is `opening` without the kind.
  ClickHouse has moved (SDB-456): its `opening` in `clickhouse.rs` reads strings, names (both
  with backslash escapes), `--` and `# ` comments, nested block comments and `$tag$` heredocs,
  for the `SemicolonScanner`.
  Trino has moved (SDB-456): `opening` in `trino.rs` is standard SQL's reading (`'…'`, `"…"`,
  `--`, block comments that do not nest), which the standard dialect (`standard.rs`, the Flight
  SQL engines) reads too, through `trino::split` and `trino::code_ranges`.
  Hive has moved (SDB-456): `opening` in `hive.rs` reads HiveQL's strings (`'…'` and `"…"`,
  backslash escapes), backquoted names and comments for Hive, Spark, Kyuubi and Impala alike.
  Spanner has moved (SDB-456): `opening` in `spanner.rs` reads GoogleSQL's `'…'` and `"…"`
  strings (triple-quoted too), backquoted names and `--`, `#` and block comments.
  Databend has moved (SDB-456): `opening` in `databend.rs` reads its strings and names
  (backslash escapes, doubled quotes), backticked names, `--` and block comments and `$$`
  bodies.
  TDengine has moved (SDB-456): `TdengineScanner` in `tdengine.rs` reads strings, backquoted
  names and comments through `opening`, and keeps what its splitter decides with: whether the
  statement's code holds a word, and where its first code is (leading comments are left out of
  what is sent). `words`, which the guard's `statement_class` and `refuse_statement` read,
  reads the scanner's code ranges.
  CQL has moved (SDB-456): `CqlScanner` in `cql.rs` reads strings, names, `$$` bodies and `--`,
  `//` and block comments through `opening`, and keeps the statement's words as it goes, so it
  tells an unapplied `BEGIN … BATCH` (whose `;` do not end it) without reading the statement
  again at every `;`. `words` and `batch_class`, which the guard reads, read the scanner's code
  ranges.
  Every SQL dialect has moved: a rule a splitter learns (a quote, a meta line, white space)
  goes into its scanner alone, and every reader has it.
  Client command lines (ADR-078, SDB-466): every SQL dialect's scanner reads a line that starts
  with `@` and an ASCII letter, with only white space before it on its line and no code yet in
  the statement (`scanner::command_line`), as a `MetaLine` statement of its own, with no strings
  or comments in it; anywhere else an `@` is what it was (a string, `SELECT` then `@x := 1`,
  Spanner's `@{…}` hint). Every SQL dialect's `refuse_statement` refuses it
  (`refuse_client_command`: a known command that nothing resolves, `@export` for good, an
  unknown `@word`). CQL and TDengine, which are not SQL (ADR-056), have none. A new SQL dialect
  checks for the line where a statement could begin and refuses it the same way
  (`tests/command_lines.rs`). Every dialect but SQL Server answers
  `None` to `first_word` for now, and the guard reads the word through its `code_ranges`.
