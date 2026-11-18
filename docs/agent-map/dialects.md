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
