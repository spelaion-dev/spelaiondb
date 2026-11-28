# ADR-059: Oracle through Oracle's own pure-Rust thin driver; a JDBC bridge only for engines no open native driver reaches

- **Status:** Accepted (the Oracle plugin is implemented, SDB-318; the JDBC bridge is
  implemented for H2 and HSQLDB, SDB-317: see the 2026-11-28 note)
- **Date:** 2026-11-15
- **Builds on:** ADR-005 (driver contract), ADR-020 (each engine's barriers), ADR-032 (the
  connection test names what is missing), ADR-041 (engines as plugins; a plugin may be written
  in any language and may link what the core cannot), ADR-042 (plugin repositories and pins),
  ADR-055 (the Plugins page)

## Context
SDB-153 asks how an Oracle plugin talks to Oracle before any driver is written, and SDB-177
(35 relational engines that DBeaver and DataGrip reach through JDBC) and SDB-178 (H2, HSQLDB,
Derby, Access, DBF) wait on the same answer: whether there is a JDBC bridge. The options the
card names, as they stand on 2026-11-15:

- **A driver with no vendor library.** Oracle now maintains one for Rust: `oracledb`
  (github.com/oracle/rust-oracledb), a pure-Rust implementation of Oracle's network protocol in
  "thin" mode, ported from python-oracledb's thin mode, dual-licensed UPL-1.0 or Apache-2.0,
  TLS through rustls, with an optional Arrow feature on the same Arrow version as ours (60). It
  is a pre-release (26.0.0-beta.6 on crates.io). Thin mode's known limits, inherited from
  python-oracledb: Oracle 12.1 and later only, no Native Network Encryption (TLS instead), no
  operating-system or external authentication.
- **Oracle's client library** (Instant Client, through the `oracle` crate and ODPI-C): the
  most complete, but a native library per platform that the user installs or that we would
  ship under Oracle's own terms, which are not ours to relicense and have changed over time.
- **A JDBC bridge:** a plugin that runs a JVM and loads JDBC drivers. Oracle's JDBC driver is
  one of them, and so are the only ways to open H2, HSQLDB and Derby files (they exist only as
  Java code) and the practical way to reach the long tail of SDB-177.

## Decision
**Oracle is a native plugin (`spelaiondb-plugin-oracle`) built on Oracle's `oracledb` thin
driver: nothing for the user to install, nothing we cannot redistribute. A JDBC bridge is a
plugin of its own (`spelaiondb-plugin-jdbc`), for engines that no open, maintained native
driver reaches; Oracle does not go through it.**

1. **Oracle, native and thin.** The plugin is a Rust plugin like the others: the driver
   contract over `oracledb`, its type zoo against Oracle Database Free in its repository's CI
   (Oracle's own `container-registry.oracle.com/database/free` image). In this repository:
   `engine = "oracle"`, its dialect (PL/SQL blocks and the `/` terminator in the splitter,
   `"…"` names, `q'[…]'` strings, comments), its classification, the guard, the form (service
   name or SID, TLS, wallets later) and the pin. The pre-release is pinned exactly and moved
   deliberately; the plugin's tests are the gate for each move.
2. **Oracle's barriers** are decided against the engine, as ADR-020 asks: `SET TRANSACTION
   READ ONLY` makes Oracle refuse DML in that transaction, and DDL commits on its own, as on
   MySQL, so the classifier refusing DDL before it is sent is the layer for it. Read-only
   connections and agents are offered only once tests that try to get around both pass
   against Oracle Free (`tests/agents_engines.rs`, `tests/read_only_connection.rs`).
3. **What the thin driver cannot do is said, not worked around:** a server older than 12.1,
   Native Network Encryption and operating-system authentication are refused by the
   connection test with that reason (ADR-032). If users need them, Instant Client is an
   optional second mode of the same plugin, found on the user's machine and never shipped
   (its own ADR).
4. **The JDBC bridge, when it is built,** follows these rules:
   - A Java program speaking the plugin protocol from `docs/plugin-protocol.md` (rows as
     Arrow, through Arrow's Java library), checked by the conformance set like any plugin.
     What the core starts and pins is a small native launcher that checks the hash of the
     bridge's own jar before it starts the JVM, so the pin still covers our code.
   - **The Java runtime is the user's:** found like the backup tools are (ADR-049), named by
     the connection test when it is missing. Shipping a runtime (a `jlink` image per
     platform, tens of megabytes) is decided later, by how many users it would serve.
   - **JDBC drivers are the user's, except those whose licence lets us ship them** (H2,
     HSQLDB and Derby are permissively licensed); a driver the user must download is said in
     the form, with where to get it.
   - **The core builds every JDBC URL** from the form's fields; a user never types one, so no
     driver option that runs code (H2's `INIT`, HSQLDB's and H2's Java functions and triggers
     defined in a file someone sent) is ever passed. Each engine's dangerous settings are
     listed and turned off by the bridge, with tests that try to get around them.
   - **An engine through the bridge has what JDBC metadata gives,** and no read-only mode nor
     agents unless the core has a dialect for it and the engine's own barrier is tested
     (ADR-041, ADR-020). The form says it is reached through a JDBC driver.
   - Sorting SDB-177's list into native plugin, bridge only, or not doing is SDB-177's work;
     this decision only says the bridge exists for the second group.

## Alternatives
- **Oracle through Instant Client.** The most complete driver and the most mature, and a
  native library per platform that every user installs, or that we ship under Oracle's terms
  rather than ours. Kept as an optional mode if the thin driver's limits matter to users.
- **Oracle through the JDBC bridge.** One plugin would serve Oracle and the long tail, and
  Oracle users would need a Java runtime and the bridge's weaker metadata, for the engine most
  asked for (SDB-37). Oracle deserves the native plugin; the bridge is for engines that have
  no better way.
- **Our own implementation of Oracle's protocol.** What `oracledb` now is, maintained by the
  vendor; writing another would be the least maintainable choice.
- **No bridge at all.** H2, HSQLDB and Derby would never open, and most of SDB-177 would stay
  out of reach; DBeaver and DataGrip reach them through JDBC.

## Consequences
- An Oracle plugin needs no download besides itself, and its licence is compatible with ours.
- We depend on a pre-release driver until it is stable; the exact pin and the type zoo bound
  that risk, and Instant Client stays the way out.
- A JVM-based plugin is a second toolchain in our plugin repositories (a JDK in CI, a
  launcher per platform) and the first plugin not written in Rust; the protocol was written to
  allow it (ADR-041), and this is where that is tested.
- SDB-177 and SDB-178 can start their sorting with this answer; the bridge itself is an item
  of its own.

## Revisit if
- `oracledb` stalls in pre-release or misses what users need: Instant Client as the plugin's
  main mode.
- A JDBC driver that matters cannot be loaded without running code from its configuration:
  that engine stays out of the bridge.

## Notes

**2026-11-15: the plugin carries a patched fork of `oracledb`.** Reading the pre-release
before writing the plugin showed four things it could not do, and SpelaionDB needs all of them:

- It could not cancel a call from another thread.
- It checked TLS against the address it connects to, so TLS failed through a tunnel.
- It printed every packet to standard output when `RSO_DEBUG_PACKETS` was set.
- It always built aws-lc.

Like the PostgreSQL, MySQL and SQL Server plugins, the Oracle plugin vendors the release as
published and patches it, each patch marked and tested there (`vendor/oracledb/PATCHED.md` in
`spelaion-dev/spelaiondb-plugin-oracle`). The fork is ours to keep: by the maintainers' decision
no issues are opened upstream. At each move to a new release, a patch is dropped when that
release does the same itself.

**2026-11-15: no read-only mode and no agents on Oracle yet.** Point 2's barrier does not stand
up to a test that tries to get around it. A function declared `PRAGMA AUTONOMOUS_TRANSACTION`
writes and commits from inside a `SELECT`, even in a `SET TRANSACTION READ ONLY` transaction,
and the classifier cannot tell such a call from any other function call. So the configuration
refuses `read_only` on Oracle and agents never see it, until a barrier passes those tests, for
example `ALTER SESSION DISABLE COMMIT IN PROCEDURE`, which stops an autonomous transaction from
committing. Exports, column statistics and the data search, which need the same barrier, are
not available on Oracle either.

**2026-11-23: no session setting is that barrier, so Oracle still has no read-only mode and
no agents (SDB-339).** The candidates were tried against Oracle Free 23.26, in a session that
starts with each of them, calling functions that write and commit in an autonomous
transaction from a `SELECT`:

- `ALTER SESSION DISABLE COMMIT IN PROCEDURE` refuses the function's `COMMIT` and its
  `ROLLBACK` (ORA-00034), and so the other ways out we tried: DDL in an autonomous block (it
  commits), `DBMS_JOB.SUBMIT` and `DBMS_SCHEDULER.CREATE_JOB` (they commit), and a write
  through a database link to the same database. A write through a link outside an autonomous
  transaction is refused anyway (ORA-14551, no DML inside a query).
- **But a function can undo it.** `EXECUTE IMMEDIATE 'ALTER SESSION ENABLE COMMIT IN
  PROCEDURE'` needs no privilege, runs from inside the query, and the function then commits
  its write, through a database link too. The classifier cannot refuse it: it is inside a
  function, or a view, the statement only names.
- `ALTER SESSION SET READ_ONLY = TRUE` (23ai) refuses the function's `INSERT` (ORA-28193),
  and a function undoes it the same way, with `ALTER SESSION SET READ_ONLY = FALSE`. It also
  refuses the session's own `ROLLBACK` after a failed statement, and Oracle older than 23ai
  does not have it.
- `SET TRANSACTION READ ONLY` does not reach the autonomous transaction at all.

`tests/oracle.rs` (`no_session_setting_stops_a_function_that_undoes_it`) keeps that evidence:
if it fails, Oracle has changed what these settings do, and this note must be read again.

The only thing we found that held is **a read-only database user** (23ai, `ALTER USER …
READ ONLY`, which a DBA sets): every function above failed under it (ORA-28194), a
definer's-rights function owned by a writable user included, after undoing both session
settings, and so did a scheduler job it created, which ran and failed with the same error.
That is the database's barrier, not ours, it needs 23ai, and its session refused the
`ROLLBACK` after such a call too (ORA-28194), which our reads end with. Offering read-only connections and
agents on Oracle only where the session's user is read-only in the database, checked when the
session opens, would be a decision of its own, with tests against such a user in
`testing/docker`; it is a Backlog item (SDB-457), not part of this note.

**2026-11-24: decided in ADR-080.** Point 2's barrier is replaced: read-only connections and
agents' reads are offered on Oracle only where the database keeps the session's user read-only,
checked on each session, and closed everywhere else. Not implemented yet: until its tests pass,
Oracle still has no read-only mode and no agents.

**2026-11-28: the JDBC bridge, for H2 and HSQLDB (SDB-317).** `spelaion-dev/spelaiondb-plugin-jdbc`
follows point 4:

- **What the core starts and pins is a native launcher** that carries the bridge's jar inside
  itself, writes it to the user's cache folder and checks its SHA-256 there before every start,
  then starts the user's Java runtime (17 or later) on it. The runtime is found as the backup
  tools are, by the launcher: `JAVA_HOME`, `java` on `PATH`, the usual install folders, newest
  first. Without one, the launcher serves the protocol itself and refuses every connection
  saying what is missing, which is what the connection test shows.
- **The bridge is Java**, speaking the protocol (1.4) with rows as Arrow through Arrow's Java
  library, checked by the conformance set. H2 and HSQLDB's drivers are inside it.
- **The core builds every URL** (`spelaion-core`'s `jdbc.rs`): the engine's `file:` prefix and
  the file's absolute path, refusing a `;`, a relative path, a shared folder or a control
  character before the plugin sees it; the bridge refuses the same and adds its own settings.
- **What a file could make it run is turned off**, each with tests that try to get around it,
  H2 and HSQLDB as published being their control: no class a file names is loaded (H2's
  `h2.allowedClasses`, HSQLDB's `hsqldb.method_class_names`); H2's source compiler and its
  linked tables' connections, and HSQLDB's loading of a trigger's class, are replaced in the
  bridge by code that does none of it.
- **Derby is left out**, as "Revisit if" says: its Java routines, and the jars a database
  carries with `derby.database.classpath`, have no setting that turns them off.
- **Not closed yet:** H2's and HSQLDB's own functions that write files or connect elsewhere
  (`FILE_WRITE`, `CSVWRITE`, `LINK_SCHEMA`…) are not Java code a file defines, and still run when
  a view, a default or a check of a file someone sent calls them. Whether the bridge refuses
  them, and how, is left to an item of its own (SDB-543).
- **Files with a user other than the administrator, or a password,** do not open from the form
  yet: a connection of these engines is a file, which the form gives no user or password, and
  logs in as `sa` (`SA`) without one.
