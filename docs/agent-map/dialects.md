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
