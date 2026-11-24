# ADR-078: Client commands in scripts: `@set`, `@unset` and `@echo`, then a confined `@include`; no `@export`

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-022 (repository trust), ADR-029 (the guard's questions), ADR-035 (script
  runs), ADR-051 (masking), ADR-053 (read-only connections), ADR-065 (one lexical scanner per
  dialect) and SDB-383 (`${name}` variables, `variables.rs`)
- **Amends:** ADR-036 (the core may read a `.sql` file the user did not pick, when a script the
  user did pick names it inside its own folder tree)
- **Leaves as it is:** ADR-034 (nothing here writes a result to a file)
- **Decides:** SDB-400

## Context
DBeaver's scripts may hold lines that are for the client, not the server: `@set name = value`,
`@unset name`, `@echo text`, `@include file.sql` and `@export {options}`, which writes the next
query's result to a file. SDB-383 gave the editor `${name}` variables without them: values are
set in a panel, per tab or per connection, and a script cannot carry its own.

What exists today, checked on `dev`:

- **Variables are written in by the core before anything else reads the text**
  (`substitute_variables`): only in code (`Dialect::code_ranges`), one statement at a time
  *after* the script is split, and the statement is split again. A value that ends its statement,
  starts another or opens a string or a comment is refused. The guard (`check_run`), the
  read-only check and the run are given that text.
- **One client's lines are already recognised, and refused.** PostgreSQL's splitter reads a
  psql meta-command (`\i`, `\set`, `\copy`) as a line of its own where a statement could begin,
  and the session refuses it through `Dialect::refuse_statement`. Making three readers agree on
  that one line took SDB-436 and SDB-437, two of the bugs ADR-065 lists.
- **Every file the core reads or writes for a tab was picked by the user** in the system's
  dialog (ADR-036, ADR-034), named on the command line (`launch.rs`) or is a tab being restored.
  Reading refuses more than 16 MiB and anything that is not text (`read_sql_file`).
- **Agents send one statement per call** (`run_query`, `execute_write`); they have no scripts
  and no variables.

The five commands are not one risk. `@set`, `@unset` and `@echo` stay inside the run. `@include`
makes the core read a file that script text names, and what it reads is sent to a server.
`@export` makes the core write a file where script text says, holding what a query returned:
with a path and a query of its author's choosing, a script someone else wrote could put a file
of its own making in the user's start-up folder. Scripts in a team's repository come from other
people (ADR-022).

## Decision
1. **Scripts get client commands in two steps: `@set`, `@unset` and `@echo` first, then
   `@include`, confined. `@export` is not built.** A command is the editor's: only a run the
   user starts from an editor tab resolves one.
2. **A command is a line, recognised by the dialect's scanner (ADR-065), not by a pass before
   splitting.** It is a *meta-command line* region, the kind ADR-065's point 2 already names: a
   line whose first character is `@` followed by an ASCII letter, where a statement could
   begin (nothing but white space before it on its line, and no code yet in the current
   statement). It runs to the end of its line, needs no `;`, and is one region with no strings
   or comments inside it, as SDB-436 settled for psql's lines.
   - The card proposed resolving commands before the script is split. That cannot be done
     safely: whether `@set` starts a line of a string, of a `$tag$` body, or of a statement
     (`SELECT` + newline + `@set := 1` is MySQL; `@set = 1` after `SELECT` is T-SQL) is exactly
     what only the dialect's scanner knows. A pass that guessed would be the second reader of
     one dialect that ADR-065 removes.
   - A letter must follow the `@`, so Spanner's statement hint (`@{…} SELECT`) stays SQL.
   - **A dialect gets client commands when it has moved onto its scanner** (ADR-065's order:
     PostgreSQL first). Until then its `@` lines are what they are today. Languages that are
     not SQL (ADR-056) have none.
   - The same five words on every dialect. On Oracle, where SQL\*Plus reads `@file` as "run
     this file", the line is ours too: SpelaionDB does not run SQL\*Plus's commands.
3. **Refused wherever it is not resolved, and an unknown one is refused everywhere.** A
   command line that reaches a session unresolved is refused before anything is sent, by the
   same `refuse_statement` that refuses psql's lines. So is an `@word` line whose word is not a
   command: it is never sent to the server as SQL. Everything that runs statements without
   being the editor's run refuses by default: agents' tools, the streaming export's new run
   (ADR-034), "run an SQL file" (ADR-049), session setup (ADR-037) and plans.
4. **Commands are resolved by the core after splitting and before the guard and the read-only
   check**, which read the statements that result, as they read the text
   `substitute_variables` returns today. Resolution walks the script in order and sends
   nothing: all of it is known before the first statement runs.
   - What runs is what was checked. Every included file is read once per resolution, which
     records its SHA-256 (ADR-036's content hash); a run whose files no longer match what the
     guard's question was asked about is refused.
   - Nothing depends on the page: the run's own `check_run` and the session's read-only
     refusal (ADR-053, point 3) read the resolved statements whatever the page sent.
   - Running a selection, or the statement under the cursor, resolves only that text. A
     command outside it does not apply.
5. **`@set name = value` and `@unset name`** change a `${name}` variable for the statements
   after them, in this run only.
   - A run starts with the tab's values over its connection's, as today. `@set` replaces a
     value from its line on; `@unset` removes it, and a later `${name}` then has no value and
     the run is refused before it starts.
   - The name and the value follow SDB-383's rules (`is_variable_name`, `is_variable_value`).
     The value is the rest of the line, trimmed, taken as it is: no `${…}` inside it is
     expanded, and nothing is read from the environment, a file or a query. Where it is used,
     `substitute_variables`'s refusals apply unchanged.
   - **Running a script never changes the variables panel or the store.** A script someone
     else wrote must not leave a value behind for the next thing the user runs in that tab.
   - A value in a script is text in a file and, once written into a statement, in the
     history: the guide says a variable is not for passwords.
6. **`@echo text`** writes its text to the Messages tab when the run reaches it. The text is
   the script's own, written as it is: never a value from a result, so masking (ADR-051) has
   nothing to hide. It is not a statement: the run's summary (ADR-035) does not count it.
7. **`@include path`** puts the statements of another `.sql` file in the script at that line.
   - **Only from a file tab.** A console has no folder for a path to be relative to: refused.
   - **The path is relative, written with `/`, and names a `.sql` file inside the script's own
     tree.** The tree is the open folder when the tab's file is inside it, otherwise that
     file's own folder. Both the tree's root and the file named are made canonical (symbolic
     links and `..` resolved) before they are compared, and the canonical name must still end
     in `.sql`. An absolute path, a drive letter, a `\`, a `~` or a `${…}` in the path is
     refused. Paths in an included file are relative to that file, inside the same tree.
   - **Read with `read_sql_file`'s rules** (16 MiB, text only, its encodings). At most 8
     levels deep, 100 files and 64 MiB of text in one run. A file that includes itself, at
     any depth, is refused; the same file twice in a row is not.
   - **Each file is a script of its own.** It is split alone, in the dialect's starting state:
     a string it does not close, or a `DELIMITER` it sets, never reaches the text after the
     `@include` line. A `@set` in it stays in force afterwards, as it would written in place.
   - **Every file is read before anything runs**, so a missing or refused file stops the run
     before its first statement, and no file is read again during the run or after an "Ask"
     pause (ADR-035).
   - **A repository script may include without a question of its own (ADR-022).** Trust is
     about what a repository can make the app do with secrets and commands, and an include
     touches neither: it reaches only `.sql` text of the tree the script came from, which
     whoever wrote the script could have written into it. The included statements go through
     the guard, the connection type's confirmations and the read-only check like typed text
     (ADR-036, point 8). The guard's question names the file and line of each statement it
     asks about, the Messages tab names each file as the run reaches it, and an error says
     which file and line it came from. Trust snapshots do not grow to cover scripts.
8. **`@export` is not built, and its line is refused with a sentence that says what to do
   instead** (run the query, then Export).
   - It is the only command that makes text someone else wrote create a file. The form that
     would be safe has the user pick the folder at every run, while the script gives only a
     file name: never a path, never overwriting (ADR-034, point 8), the extension set by the
     format. That is a question on every run for what the Export button already does.
   - DBeaver's options are a JSON object of its own internal names (producer, processor,
     consumer): a script written for DBeaver would not run unchanged whatever we built.
   - The need behind it (one run, several result files) is better met by an action of the
     app, "export every result of this run to a folder", which needs no script syntax and is
     not decided here.
9. **Read-only connections (ADR-053).** No command writes to the database, so `@set`, `@unset`,
   `@echo` and `@include` are allowed there. The statements that result are checked as always:
   one write, wherever it came from, refuses the whole script before anything runs.
10. **Agents never run a command.** The MCP's tools take one statement and call neither the
    resolver nor the variables. `classify_read_only` and the write check (`write.rs`) refuse a
    statement that starts with a command line themselves, without relying on the SQL parser
    failing on it.
11. **Tests that try to get around it, before each step ships:**
    - `tests/properties.rs`: a command line hidden in every place a statement can go is never
      run by an agent's tool, on every dialect; a `@set` value never turns one statement into
      two or hides a statement from the guard.
    - `@include`: paths that try to leave the tree (`..`, symbolic links, on Windows also
      junctions, UNC and `\\?\` paths, drive-relative paths, alternate data streams and
      device names), a link named `x.sql` to a file that is not, cycles and the limits; a
      destructive statement in an included file is asked about with its file named; a write
      in one refuses a read-only connection's script.

## Options not taken
- **All five, as DBeaver has them.** `@export` with a path from the script is an arbitrary
  file write for whoever wrote the script. Point 8 says why its safe form is not worth a
  command.
- **None: variables stay in the panel.** Nothing to build and nothing to get wrong. But a
  script shared in a repository cannot say which values it needs, and a team's common `.sql`
  cannot be reused without pasting it. This is the alternative if the scanner (ADR-065) slips:
  the commands wait for it either way.
- **Only `@set`, `@unset` and `@echo`.** The closest call. They need no file access and are
  most of what people use. `@include` costs a path check that is security code on three
  platforms. It is kept because scripts next to `.spelaion/` in a team's repository are what
  ADR-036 was written for, and because confined to `.sql` files of the script's own tree it
  reaches nothing the script's author could not write in the script.
- **Each engine's own client lines instead** (psql's `\i` and `\set`, SQL\*Plus's `@file`,
  sqlcmd's `:r` and `:setvar`, mysql's `source`). Scripts in the wild use these, not
  DBeaver's. But they are five syntaxes with five meanings (psql's `\set` feeds `:name`, which
  here is a bound parameter), and a script would mean one thing in psql and another here.
- **A question before every run that includes a file.** A question on every run is one
  people stop reading. The guard already asks about what is risky, and it reads the included
  statements.
- **`@set` writes the tab's variables**, so its value shows in the panel and outlives the
  run. Then one script changes what the next run does, with nothing on screen saying so.
- **A command needs a `;`**, so no splitter changes and the resolver reads statements that
  start with `@`. A quote in a value (`@set name = O'Brien`) would open a string for the
  splitter and swallow the statements after it: the splitter has to know the line.

## What would make us revisit
- **Users ask for unattended report scripts** (several queries, several files, no clicks):
  `@export` in the form of point 8, with the folder chosen once per tab instead of per run,
  after "export every result of this run" exists and proves not to be enough.
- **Users bring multi-file scripts written for psql, SQL\*Plus or sqlcmd:** a dialect may
  read its vendor's include line (`\i`, `@file`, `:r`) as this `@include`, with the same
  limits. Never the vendors' variables.
- **An included file is used to hide what a script does** (a report, or a review that finds
  the Messages tab is not enough): a connection type's safeguard that asks before a run that
  includes files, added like ADR-029's others, only ever stricter.
- **ADR-065's scanner is not built, or stops at PostgreSQL:** commands stay on the dialects
  that have it; no dialect gets them through a second reader.
- **`@include` is not used** once it has been out for two releases: it is the part with a
  security surface, and removing it is cheaper than keeping its path rules correct.

## What follows as work items
None exists yet; each is created when this is accepted.

1. **Command lines in the scanner, refused by default** (S, after SDB-453): the `@` line as
   a meta-command region in the scanner type and in PostgreSQL, `refuse_statement` for it,
   the agents' classifiers' own refusal and the property tests of point 11. Later dialects
   get the region as they move (SDB-454 to SDB-456).
2. **`@set`, `@unset` and `@echo` in the editor's runs** (M): the resolver in the core, the
   run reading it before `check_run`, the Messages tab, and the guide page.
3. **`@include`** (L): the path rules and limits in a file of their own, added to
   `.github/CODEOWNERS` in the same pull request, with the origin of each statement in the
   guard's question, in errors and in the Messages tab.
4. **Refuse `@export` with its sentence** (S, with item 1).
