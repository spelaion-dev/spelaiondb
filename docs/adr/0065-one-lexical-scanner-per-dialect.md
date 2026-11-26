# ADR-065: One lexical scanner per dialect, read by everything that needs to know what is code

- **Status:** Accepted (being implemented: the scanner type and PostgreSQL, SDB-453; MySQL and
  MariaDB, SDB-454; SQL Server and the guard's first word, SDB-455; SQLite, SDB-456)
- **Date:** 2026-11-23
- **Extends:** ADR-005 (each engine has a dialect) and ADR-041 (the dialects and the barriers
  live in the core; a plugin names one of the core's dialects, or none)
- **Builds on:** SDB-143 (the dialects moved into the core, in `spelaion-dialect`, without
  changing what they do) and SDB-156 (property and fuzz tests for the splitters, the placeholder
  scanner, the classifiers and the readers of untrusted files)
- **Decides:** SDB-157

## Context
Several places in the core need to know, for a piece of SQL text, which characters are code
and which are inside a string, a quoted name, a comment, a dollar-quoted body or another
dialect-specific region. Each of them scans the text its own way:

- **Each dialect's splitter** (`split_statements`, in `spelaion-dialect`): where statements end,
  with each engine's quotes, comments, `$tag$` bodies, `BEGIN … END`, `DELIMITER`, `GO`, psql's
  meta-command lines and `COPY … FROM STDIN` data. The editor's "statement under the cursor"
  (`cursor.rs`) reads it too.
- **Each dialect's `code_ranges`** (SDB-135): the ranges outside strings, names and comments,
  which the placeholder scanner (`params.rs`) and other readers search. It is built with
  `spelaion_driver_api::code_ranges` from a *skip* function that each dialect writes next to its
  splitter, as a second scanner of the same rules.
- **Other readers inside one dialect**, such as PostgreSQL's `copy_stdio`, which reads a
  statement's words to decide whether data follows it.
- **The guard's fallback** (`guard::destructive`): when the SQL parser cannot read a statement,
  its first word decides (SDB-117).
- **The remote SQLite check** (`remote.rs` of the SQLite driver, `check_statement`): what may
  reach the `sqlite3` shell, which has its own idea of a line (SDB-126).
- **The classifier's pre-checks** (`reject_mysql_executable_comments`,
  `reject_unicode_escapes`), which deliberately read the raw text, and the SQL parser's own
  tokenizer, which is not always the server's (SDB-115, SDB-116).

Every one of these was fixed as a symptom, and SDB-156's property tests have since found the
same family of bug again and again. Each of them was **two scanners of one dialect disagreeing**:

| Bug | Two readers that disagreed |
|---|---|
| SDB-135 | The placeholder scanner and the dialects: no bracket names, no `E'…'` strings, no nested comments in the first. Fixed by making it read `code_ranges`. |
| SDB-117 | The guard's first-word fallback and SQL Server's batches. |
| SDB-126 | The remote SQLite check and the shell: a dot-command after a comment line. |
| SDB-434 | Every splitter and `str::trim` on white space outside ASCII. An inverted span panicked. Fixed by one shared rule (`is_white_space_at`, `white_space_before`, `trimmed`) that every splitter now calls. |
| SDB-436 | PostgreSQL's splitter and its `code_ranges` on psql meta-command lines. A quote there hid a placeholder. |
| SDB-437 | PostgreSQL's splitter and `copy_stdio` on a psql `\copy` line. It was given COPY data and sent to the server. |
| SDB-439 | PostgreSQL's splitter and `copy_stdio` on a word after non-ASCII white space. Text after a `;` could be sent both as COPY data and as SQL. |

Each of the last four was found by fuzzing the splitter, not by reading the code, and each was
fixed by copying the splitter's rule into the second scanner. The second scanner then waits for
the next rule the splitter learns. Three of them could have sent the server something other
than what the core checked.

## Decision
1. **Each SQL dialect has one lexical scanner**, in `spelaion-dialect`, and every reader of that
   dialect's text reads it. Those readers are the splitter, `code_ranges`, `copy_data` and its
   word reader, the statement's first word, and the guard's fallback. The scanner is the only
   code that decides whether a character is code. A rule such as SDB-434's white space, or a
   new kind of quote, is written once and every reader has it.
2. **Its shape is a lazy sequence of regions**, each a byte range with a kind: code, string,
   quoted name, comment, executable comment (MySQL's `/*! … */`), quoted body (`$tag$`, PL/SQL
   bodies), meta-command line (psql's `\`, MySQL's `DELIMITER`, T-SQL's `GO`), copy data and
   statement end.
   - It is an iterator over the text, never a token list. A dump of millions of lines is
     scanned in one pass, in constant memory beyond the current region.
   - The splitter's state that depends on words (PostgreSQL's `BEGIN ATOMIC` blocks, Oracle's
     PL/SQL units, MySQL's `DELIMITER`) lives in the scanner, because it decides where the next
     region starts.
3. **It is not the SQL parser's tokenizer.** That tokenizer is one of the readers that
   disagrees with the servers, and it knows neither `DELIMITER` nor `GO`. The classifier keeps
   sqlparser for what a statement *does*. Its pre-checks keep reading the raw text where they
   refuse a marker even inside a literal on purpose, because a session's `sql_mode` can change
   what a literal is (SDB-116).
4. **A session's mode is the scanner's input.** A dialect made with `with_sql_mode` (MySQL's
   `ANSI_QUOTES`, `NO_BACKSLASH_ESCAPES`) gives a scanner that reads quotes as that session
   does, so every reader follows the session together.
5. **Nothing changes for other people's plugins (ADR-041).** A plugin that names one of the
   core's dialects gets its scanner with it. One that names none keeps the standard reading it
   has today (`Dialect`'s defaults). The scanner never assumes an engine of ours.
6. **The move changes no behaviour.** Every existing example test of every dialect passes
   untouched, the statement under the cursor picks what it picks today, and SDB-156's property
   tests guard each step. A dialect moves onto its scanner in a change of its own, and the move
   is the only thing in it.
7. **Differential tests come first.** Before any dialect moves, the property tests learn to
   compare the readers of each dialect. Every byte that `code_ranges` calls code must be code
   for the splitter. Every statement `copy_data` gives data to must be one whose first word,
   read the splitter's way, is `COPY`. The guard's first word must be the splitter's. These
   tests find the remaining disagreements before the rewrite and keep the rewrite honest.
8. **Order** (each step is a Backlog item):
   1. the differential tests (SDB-452);
   2. the scanner type and PostgreSQL, the dialect with the most readers (SDB-453);
   3. MySQL and MariaDB (`DELIMITER`, executable comments, `sql_mode`) (SDB-454);
   4. SQL Server (`GO`, bracket names), with the guard's first-word fallback reading the
      scanner (SDB-455);
   5. the other SQL dialects (SDB-456);
   6. the remote SQLite check, which reads SQLite's scanner plus the shell's own line rules
      (SDB-456).

## Consequences
- The class of bug in the table becomes structural: a reader cannot see code where the splitter
  sees a string, because both read the same regions. The property tests stop being the only line
  of defence and become a check on one scanner instead of on every pair.
- Each dialect file gets shorter. Today a dialect such as `postgres.rs` holds its splitter's
  loop, its skip function for `code_ranges` and `copy_stdio`'s own word loop. Each of the three
  re-implements quotes, comments and `$tag$` bodies.
- The scanner is security code: the splitter and `code_ranges` decide what the guard and the
  barriers look at. Each move is reviewed as such: CI's tests plus the differential properties,
  long fuzz runs (`SPELAION_FUZZ_CASES`) before merging, and no other change in the same pull
  request.
- Languages that are not SQL (ADR-056: MongoDB, Redis, Neo4j's Cypher) keep their own readers.
  They are not covered by this decision.

## Alternatives
- **Keep a scanner per use, and rely on the property tests to find the differences.**
  - *Cost:* none now. Each fix stays small.
  - *Against it:* SDB-434, 436, 437 and 439 show what that means. The tests find a disagreement
    only after one exists, a long fuzz run finds it only if its generator happens to build the
    input, and each fix copies a rule into one more place. This is the fallback if the scanner
    turns out to cost more than expected. Step 1 above (the differential tests) is worth doing
    either way.
- **One generic scanner, configured per dialect by data** (quote characters, comment markers,
  escape rules).
  - *Cost:* smaller code.
  - *Against it:* the regions that cause the bugs are the stateful ones: psql meta lines,
    `DELIMITER`, `GO`, `BEGIN ATOMIC`, PL/SQL units, COPY data. A table cannot describe them
    without becoming code again. Each dialect can still build its scanner from shared pieces
    (doubled-quote strings, nested block comments), as the splitters already share the white
    space rule.
- **Use the SQL parser's tokenizer.** Refused above: it disagrees with the servers, does not
  know the client-side separators, and reads whole statements, not scripts.

## What would change this
- If the differential tests (step 1), run long against every dialect, find no disagreement
  beyond PostgreSQL's, the other dialects may stay as they are and only PostgreSQL moves. That
  would be recorded as a note here.
- If a dialect's scanner, once written, makes its splitter measurably slower on a large dump
  (the tests of SDB-162 and of the large results), that dialect keeps its loop until the scanner
  is as fast.
