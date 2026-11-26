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
  where its variables live (the guard still checks each statement in it). SQLite's
  `skip_quote_or_comment` and `code_line_starts` are what `remote.rs` relies on to keep a
  dot-command away from the server's shell: any change to them needs its tests there too.
  `skip_quote_or_comment` is also `SqliteScanner`'s one reading of quotes and comments; until
  `remote.rs` moves onto the scanner (SDB-456), `code_line_starts` is the one loop of its own
  left, and it must read what the scanner reads.
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
  Every dialect but SQL Server answers `None` to `first_word` for now, and the guard reads the
  word through its
  `code_ranges`. Until another dialect moves, a rule its splitter learns (a quote, a meta line,
  white space) must be copied into its other readers (`code_ranges`'s skip function) in the
  same change: that gap is what SDB-436, 437 and 439 were.
