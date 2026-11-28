# ADR-093: The JDBC bridge refuses H2's file and network functions, and keeps HSQLDB's inside its database's folder

- **Status:** Proposed
- **Date:** 2026-11-28
- **Amends:** ADR-059, point 4 ("each engine's dangerous settings are listed and turned off by
  the bridge"): not only settings, also the engines' own functions that reach outside
- **Builds on:** ADR-041 (a plugin is the wire; what may run is decided before it), SDB-317 (the
  bridge, `spelaion-dev/spelaiondb-plugin-jdbc`, and what it already turns off)
- **Decides:** SDB-543

## Context
SDB-317's bridge turns off every way an H2 or HSQLDB file can make the JVM run Java code it
defines: Java functions, triggers and aggregates named by class, H2's code written as source,
H2's linked tables, HSQLDB's Java triggers, and URLs with settings of their own (H2's `INIT`).
Each is tried by a test against the drivers as published.

What it left open, said in ADR-059's 2026-11-28 note: the engines' **own** functions that read
or write files or reach other machines. A file someone sent can call them from a stored object,
and the user runs them by opening the file, browsing it or reading a view. SpelaionDB cannot
tell a function the user typed from one a view calls: the core sends `SELECT * FROM v`, and the
view's function runs inside it.

### What reaches outside, and what was tried
Every claim below is backed by a test or says it was not run. The tests are
`tests/stored_functions.rs` on the plugin's branch `claude/sdb-543-file-functions` (commit
`809aa4a`): the file is written with the driver as published, then opened through the bridge as
SpelaionDB uses it, browsed (every catalog request the explorer makes), then read. "Before" is
the bridge of SDB-317; "after" is the prototype of the recommendation below. H2 2.5.252, HSQLDB
2.7.4, Windows 11, Temurin 21.

**H2**

| What | Where a file can put it | Before | After |
|---|---|---|---|
| `FILE_WRITE(data, path)`: writes any path | a view, a default, a generated column, a check | **writes** when the view is read or a row inserted (4 cases, tested) | refused, nothing written (tested) |
| `CSVWRITE(path, query)`: writes any path | a view, a default, a check | **writes** when the view is read (tested in a view only) | refused (tested) |
| `FILE_READ(path or URL)`: reads a file, `classpath:`, or any URL Java opens (`http:`, `ftp:`, `jar:`) | a view, a default, a check | **reaches the URL as the file opens**, before anything is browsed or read (tested in a view, with a local HTTP server) | refused (tested) |
| `CSVREAD(path or URL)`: reads a file or URL, already when a view naming it is compiled | a view (`FROM CSVREAD(…)`) | **reaches the URL as the file opens** (tested) | refused; the view is kept, invalid (tested) |
| `LINK_SCHEMA(…, url, …)`: connects to any JDBC URL (another H2 URL can carry an `INIT`) and links its tables | a view (`FROM LINK_SCHEMA(…)`) | **connects** when read: another H2 file is made (tested); a nested `INIT` then runs SQL there (not run here; SDB-317's linked-table test shows it does at connect with H2 as published) | refused (tested) |
| A local file through `FILE_READ` or `CSVREAD` | as above | same code path as a URL (not run with a local file) | refused with them |
| `SCRIPT TO`, `RUNSCRIPT FROM` (a file or URL), `BACKUP TO` | commands only: a view is a query and defaults, checks and generated columns are expressions; triggers are Java and already closed | not reachable from a stored object (H2's grammar read, not run) | unchanged: typed by the user, they do what the user asks |
| `CREATE LINKED TABLE`, Java functions and triggers, `INIT` | | closed by SDB-317 (tested there) | unchanged |

The control, tested on the same file: H2 as published reaches the address as the file opens and
writes the file when the view is read.

**HSQLDB**

| What | Where a file can put it | Before | After |
|---|---|---|---|
| `LOAD_FILE(path)`: reads a file into the result | a view, a default | **reads any path**: absolute, and `../` out of the database's folder (tested in a view and typed) | refused outside the database's folder ("Access is denied"), allowed inside (tested) |
| A text table (`SET TABLE t SOURCE 'path'`): a table kept in a CSV file | its definition, in the `.script` HSQLDB replays as it opens | **makes the file outside its folder as the database opens**, and writes rows there when one is inserted (tested) | refused outside its folder; a file written that way by HSQLDB as published opens, its table empty and no file made (tested); inside its folder unchanged (tested) |
| `SCRIPT 'file'`, `BACKUP DATABASE TO`, `PERFORM EXPORT/IMPORT` | commands only: HSQLDB's SQL routines run SQL-data statements | not reachable from a stored object (grammar read, not run); `BACKUP` writes anywhere even after the change (`Logger.getSecurePath(…, true, …)`, read, not run) | unchanged: typed by the user |
| A URL | HSQLDB reads files with `FileInputStream` only | none (read, not run) | |
| Java routines and triggers | | closed by SDB-317 (tested there) | unchanged |

HSQLDB's default is the surprise: `textdb.allow_full_path`, the system property that keeps
`LOAD_FILE` and text tables inside the database's folder, is off unless set.

## Decision (proposed)
**The bridge refuses H2's `FILE_READ`, `FILE_WRITE`, `CSVREAD`, `CSVWRITE` and `LINK_SCHEMA`
everywhere, typed or stored, and keeps HSQLDB's `LOAD_FILE` and text tables inside the
database's folder.**

1. **H2:** the four classes behind those functions (`org.h2.expression.function.FileFunction`,
   `CSVWriteFunction`, `org.h2.expression.function.table.CSVReadFunction`,
   `LinkSchemaFunction`) are replaced in the bridge's jar, as SDB-317 replaced the source
   compiler and the linked tables' connection, by classes written for the bridge that keep each
   function's name and type and refuse when it is evaluated, and before `CSVREAD` and
   `LINK_SCHEMA` read anything to compile a view. A view or a default that names one stays in
   the file and fails when used, saying the bridge reaches no file or address a database names.
2. **HSQLDB:** the launcher and the bridge set `textdb.allow_full_path=false`, as they set
   `hsqldb.method_class_names`: `LOAD_FILE` and text tables reach only the database's own
   folder, which is where HSQLDB keeps its own files anyway.
3. **Commands the user types** (`SCRIPT`, `RUNSCRIPT`, `BACKUP`, `PERFORM`) are left as they are:
   no stored object reaches them, and they are the user's. They are listed in the plugin's
   README.
4. **Every change of these is tested as the rest of the bridge is:** a file written with the
   driver as published, opened through the bridge, browsed and read, with the published driver
   as the control. The bridge refuses to start if any of the classes is not the bridge's.
5. **A new version of either driver** is checked for new functions that reach outside, as its
   patched classes are (the plugin's `PATCHED.md`).

## Alternatives
- **Refuse them only when a stored object calls them** (the user's typed `CSVREAD` keeps
  working). H2 builds the same expression for a function in a view, a default or a typed
  statement, and evaluates a view's expressions inside the typed statement that reads it; nothing
  records where an expression came from. Telling them apart means changing H2's parser and its
  views, far beyond replacing a class. Not prototyped; rejected for that cost.
- **Document them only** (what the plugin's README says today). It cannot cover the worst case:
  `FILE_READ` and `CSVREAD` of a URL reach out as the file opens, before the user reads or does
  anything.
- **Refuse writes, connections and URLs, but keep reading local files** (`FILE_READ` and
  `CSVREAD` of a path). With no URL, no `LINK_SCHEMA` and no write left, what a view reads is
  only shown to the user who opened it: nothing leaves the machine. It keeps H2's CSV reading for
  people who type it, and costs a replacement that tells a path from a URL (Java opens more URL
  schemes than `http:`; a drive letter has a colon too). Not prototyped. This is the alternative
  to choose if users of the bridge rely on typing `CSVREAD`.
- **A switch per connection, "this file is mine", that turns the functions back on.** A switch
  in the form is one click nobody reads before; it would need its own ADR if users ask.

## Consequences
- Opening, browsing or reading an H2 or HSQLDB file someone sent reaches no file and no address
  outside the database's folder through the engines' own functions, besides what SDB-317 closed.
- In SpelaionDB, H2's `CSVREAD`, `CSVWRITE`, `FILE_READ`, `FILE_WRITE` and `LINK_SCHEMA` are not
  available even when typed: SpelaionDB's own imports and exports read and write CSV instead.
  H2's full-text search was already out (it is Java triggers).
- Four more replaced classes to carry when H2 moves (seven in all); each has few methods, and the
  check at start and the tests catch a version that changed them.
- Part 2 of SDB-317 (the pin that makes the bridge reachable) waits for this to be accepted and
  implemented in the plugin.

## Revisit if
- Users ask for typed `CSVREAD` or `FILE_READ`: the third alternative.
- A new H2 or HSQLDB version adds a function or a stored object that reaches files or the
  network.
- HSQLDB's `BACKUP` or `PERFORM` become reachable from a stored object.
