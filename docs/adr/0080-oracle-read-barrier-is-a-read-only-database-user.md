# ADR-080: Oracle's read barrier is a read-only database user: read-only connections and agents' reads only where the database itself keeps the session's user read-only

- **Status:** Accepted (not implemented). The note of 2026-11-28 contradicts point 8: read
  this decision again before anything is built on it.
- **Date:** 2026-11-24
- **Builds on:** ADR-008 (the layers: classifier, the engine's own barrier, limits), ADR-020
  (each engine's barrier, and servers checked when a session opens), ADR-053 (read-only
  connections), ADR-059 (Oracle; its notes of 2026-11-15 and 2026-11-23)
- **Amends:** ADR-059, point 2 (which barrier Oracle's read-only connections and agents rest
  on)
- **Decides:** SDB-457

## Context
Oracle has no read-only mode and no agents (`Engine::has_read_barrier` is false for it;
`read_only` is refused when the file loads). ADR-059's point 2 expected `SET TRANSACTION READ
ONLY` to be the barrier. SDB-339 tried it and the other session settings against Oracle Free
23.26, and its evidence is kept in `tests/oracle.rs`
(`no_session_setting_stops_a_function_that_undoes_it`) and in ADR-059's note of 2026-11-23:

- A function declared `PRAGMA AUTONOMOUS_TRANSACTION` writes and commits from inside a
  `SELECT`. `SET TRANSACTION READ ONLY` does not reach it.
- `ALTER SESSION DISABLE COMMIT IN PROCEDURE` and `ALTER SESSION SET READ_ONLY = TRUE` each
  stop that function, and neither stops one that first runs the opposite `ALTER SESSION`
  through `EXECUTE IMMEDIATE`, which needs no privilege. The classifier cannot see it: it is
  inside a function, or a view, that the statement only names.
- **A read-only database user held** (`ALTER USER … READ ONLY`, which a DBA sets). Under it,
  all of these failed with ORA-28194: a definer's-rights function owned by a writable user,
  functions that undid both session settings first, a write through a database link, and a
  scheduler job created from the query (the job ran and failed).
- Under such a user, the session also refused the `ROLLBACK` after such a call (ORA-28194).
  Agents' reads end with one (`rolled_back`, in `readonly.rs`).

Two things were not part of that evidence, and matter here:

- **Read-only users are for local users of a pluggable database, in Oracle Database 23ai and
  later**, by Oracle's description of the feature (23.26, the release tested, is the one
  Oracle now calls 26ai). Most Oracle servers in production today are older.
- **Oracle's own write-up says that changing a user does not change the sessions already
  connected.** What the dictionary says of a user and what a session enforces can therefore
  differ. Neither point was tested here: item 1 below does.

The classifier has no Oracle arm, and ADR-020 already has the pattern this needs: what a
PostgreSQL or MySQL connection gets is decided by what the server answers when a session
opens, never by the connection's file, and the default is closed (`server_family.rs`).

## Decision
1. **On Oracle, read-only connections and agents' reads are offered only where the session's
   database user is read-only in the database.** No session setting is the barrier, and none
   will be taken for one: `tests/oracle.rs` keeps saying why.
2. **The barrier is the database's, not ours.** SpelaionDB does not make a user read-only and
   cannot: a DBA does, with `ALTER USER <name> READ ONLY`. What SpelaionDB does is refuse to
   call a session read-only unless the database is already keeping it so. The form, the
   connection test and the refusals say exactly that, with the statement the DBA runs and
   the release it needs.
3. **Checked on each session, right after it connects, and for that session's life.** Two
   answers are needed, and both must say read-only:
   - **the dictionary's:** the session's own user is marked read-only, asked of the `SYS`
     view by its full name, so no object in the user's schema can answer for it;
   - **the session's own behaviour:** a write the core tries on purpose is refused with
     ORA-28194. The dictionary alone is not enough, because a session connected before the
     DBA's change keeps what it had. The write is one that changes nothing if it is not
     refused, chosen by the experiment in item 1 below.
   - A session is never promoted later: one that failed the check stays without the barrier
     until it is closed. A new session is checked again, after a reconnect too.
4. **Everything else is closed.** A server before 23ai has no such user and no such column:
   the question fails, and a failed question is a no. So is an answer the core does not
   recognise, a common user (Oracle marks only local users), and a check that could not run.
   The refusal says which of these it was. No version number is trusted in place of the two
   answers.
5. **What changes for each thing that reads `has_read_barrier` today.** The engine-wide
   answer is no longer enough for Oracle: it is a fact of a session, read where
   `server_family.rs` reads the PostgreSQL and MySQL servers'. Each caller either asks the
   session or keeps refusing Oracle:
   - **`read_only = true` in a connection file** loads. The session opens only if the check
     passes; otherwise it does not open, and the error says what the DBA has to set
     (ADR-053, point 4: if the mode cannot be had, the session does not open).
   - **"Read-only for this session"** (SDB-395) is offered only on a session that passed.
   - **Agents' reads** (`run_query`): an exposed Oracle connection is visible to agents, and
     every read runs on a session that passed. One that did not is refused with a code of
     its own and `retry: false` (`agent_state`), recorded in the audit log.
   - **Agents' writes** never run on Oracle: a read-only user cannot write, and ADR-019's
     writes use the connection's own user. `runs_agent_writes` stays false for it.
   - **Streaming exports, column statistics and the data search** (ADR-034, ADR-043) run
     only on a session that passed, and keep refusing otherwise, with the same sentence.
6. **Our layers stay on top, as on every engine (ADR-008), and none of them is the barrier:**
   - **The classifier gets an Oracle arm.** It refuses by name what reaches outside the
     database, which a read-only user does not stop: `UTL_HTTP`, `UTL_TCP`, `UTL_SMTP`,
     `UTL_MAIL`, `UTL_FILE`, `UTL_INADDR`, `DBMS_LDAP`, `HTTPURITYPE` and the other URI
     types, `DBMS_PIPE`, `DBMS_ALERT`, `DBMS_LOCK`, `DBMS_SCHEDULER`, `DBMS_JOB`, and any
     name with `@link`. The list is settled with its tests (item 3). It refuses `SELECT …
     FOR UPDATE`, as on every engine. As with DuckDB's names (ADR-020), the names give a
     clear message; they are not the guarantee, since a view can hide a call.
   - **The session-setup allow list** (ADR-037) for Oracle refuses `ALTER SESSION ENABLE
     COMMIT IN PROCEDURE`, `ALTER SESSION SET READ_ONLY = FALSE` and `ALTER SESSION SET
     CONTAINER`.
   - **`ALTER SESSION DISABLE COMMIT IN PROCEDURE` when the session opens, and `SET
     TRANSACTION READ ONLY` around each agent's read.** A function can undo the first and
     the second does not reach an autonomous transaction, so they add nothing a read-only
     user lacks. They stay for the day a DBA makes the user writable again while a session
     is open, and because safeguards only add up.
   - `ALTER SESSION SET READ_ONLY = TRUE` is **not** set: it is one more setting a function
     undoes, and one more thing that refuses the session's `ROLLBACK`.
7. **A read's ending that the database refuses closes the session.** After a statement that
   was refused with ORA-28194, the `ROLLBACK` that ends the read may be refused too.
   - The error reported is the read's own. A refused ending never replaces it and is never
     dropped: it goes to the audit log and the app log.
   - The session is not used again. It is closed, which ends its transaction on the server,
     and the next read opens a new session and checks it (point 3). Nothing the agent or
     the user ran is run again.
   - An editor tab on a read-only Oracle connection shows a refused `ROLLBACK` as the error
     it is, and the transaction's state stays what the driver reports (ADR-031).
8. **What it covers is this database.** A read-only user cannot change the database the
   session is connected to. What a link reaches in another database is that database's to
   refuse: the classifier refuses the `@link` names it can see, and the tests of item 2
   find out what a link hidden in a view does. The form says "this database".
9. **Not offered until the tests pass** (ADR-020): `has_read_barrier` and the configuration's
   refusal stay as they are until `tests/read_only_connection.rs` and
   `tests/agents_engines.rs` pass against a read-only user in `testing/docker` and refuse a
   writable one.

## Options not taken
- **Keep refusing: no read-only mode and no agents on Oracle** (the most reasonable
  alternative). Nothing to build, nothing to get wrong, and it is what `dev` does. Oracle is
  the engine most asked for (SDB-37), and a team that already gives its tools a read-only
  user gets nothing for it. The cost of saying yes is that it only helps on 23ai and later,
  which is few servers today.
- **A weaker read-only mode for writable users**, with the classifier and the session
  settings. It would look like the other engines' and not be: one function call gets around
  it. ADR-041 and ADR-020 refuse a weaker mode under the same name.
- **Trust the user's privileges** (only `CREATE SESSION` and `SELECT` grants) on servers
  before 23ai. A definer's-rights function runs with its owner's privileges, so the session's
  own grants prove nothing.
- **The dictionary's answer alone.** A session that connected before the DBA's change would
  pass while it can still write.
- **Mark a session read-only by itself when its user is read-only.** The classifier refuses
  more than the database does (what it cannot parse), so a user who did not ask for
  read-only mode would lose statements the database allows.
- **Drop the layers on top**, since the user is already read-only. They cost one statement a
  session, and the classifier's names are the only thing between an agent and `UTL_HTTP`.

## What would make us revisit
- **A database open read-only** (a standby, a pluggable database opened read-only, 23ai's
  hybrid read-only mode) is another barrier that is the database's, and the first two exist
  before 23ai. Each becomes a second accepted answer in point 3 only with its own tests that
  try to get around it. This is the way to reach the Oracle servers most people run.
- **Oracle gives a session setting a function cannot undo:** it replaces the user's check as
  the barrier, as ADR-059's point 2 hoped.
- **The check passes and a write goes through in any test or report:** Oracle goes back to no
  read-only mode and no agents the same day, and this ADR is marked superseded.
- **The refused `ROLLBACK` makes agents' reads too slow** (a new session after each refused
  call): a cheaper ending is looked for, with the same rule that a session whose ending is
  not confirmed is not reused.
- **`tests/oracle.rs`'s evidence test fails:** Oracle changed what the session settings do,
  and point 6's reasons are read again.

## What follows as work items
None exists yet; each is created when this is accepted. Nothing here was built or run for this
decision: the evidence is SDB-339's.

1. **A read-only user in `testing/docker`, and the experiment that fixes the check** (S):
   the user made by Oracle's startup script; which dictionary view and column answer on the
   oldest 23ai release offered; which write is refused with ORA-28194 and changes nothing
   when it is not; how a refused `ROLLBACK` leaves the session; whether a session connected
   before `ALTER USER … READ ONLY` passes or fails each answer.
2. **The check, the session's fact and the reads' ending** (M, `readonly.rs`, so it waits
   for an owner): points 3, 4, 5 and 7, with get-around tests in
   `tests/read_only_connection.rs` and `tests/agents_engines.rs`: the functions of SDB-339, a
   definer's-rights function, both session settings undone, a link written through directly
   and hidden in a view and in a synonym, a scheduler job, `ALTER USER` on itself, a common
   user, a session connected before the change, and a writable user refused everywhere.
3. **The classifier's Oracle arm and the session-setup allow list** (M, `classify.rs` and
   `session_setup.rs`, protected): point 6, in `tests/properties.rs` too.
4. **The form, the connection test and the guide** (S): what the DBA sets, the release it
   needs, that it covers this database, and the connection test's step that reports the
   check (ADR-032).
5. **Exports, column statistics, the data search and the session toggle on Oracle** (S,
   after item 2): each asks the session, with a test that a writable user is still refused.

## Notes

**2026-11-28: the experiment of item 1 (SDB-474), and a write that contradicts point 8.**
`testing/docker` now has a read-only user: `spelaion_ro`, a local user of FREEPDB1 that
`oracle/startup/02-spelaion-ro.sql` creates and sets `READ ONLY` on every start. Everything
below was run against Oracle Free 23.26.3 and is kept as a test in `tests/oracle.rs`; nothing
was turned on (`has_read_barrier`, the configuration's refusal and `agent_view` are unchanged).

- **The ways around a session setting are refused**
  (`the_read_only_user_refuses_every_way_around_that_writes`). Under the read-only user each of
  these fails with ORA-28194 and writes nothing: a direct `INSERT`; a function that writes in
  an autonomous transaction with the session's own rights, and one with the rights of the
  writable user that owns it; functions that first undo `DISABLE COMMIT IN PROCEDURE` and
  `READ_ONLY = TRUE`; a write through a database link, directly and from such a function; and
  a scheduler job created from the query. The job is created all the same, runs, and its own
  write fails with ORA-28194: the scheduler's records of the job are the only thing left. The
  same functions write under the writable user.

The five answers:

1. **The dictionary's answer is `SYS.USER_USERS.READ_ONLY`** (`YES` or `NO`), which every user
   may read (`the_dictionary_says_whether_the_sessions_user_is_read_only`). `DBA_USERS` and
   `CDB_USERS` have the same column, for a DBA. It must be asked by its full name: a view
   called `USER_USERS` in the user's own schema answers for the short name, and the test makes
   one that says `YES` for a writable user. On a release before 23ai the column does not exist
   and the question fails with ORA-00904. That last part is inferred, not run:
   `testing/docker` has no older release, so the test shows ORA-00904 for a column 23.26 lacks.
   Only 23.26 was tried, not the oldest 23ai release.
2. **The write the check tries is `ROLLBACK`, right after the session connects**
   (`rollback_is_the_write_a_read_only_session_refuses_and_a_writable_one_ignores`). A
   read-only session refuses it with ORA-28194, and is left with no transaction and no lock. A
   writable session that has just connected has nothing to roll back: it changes nothing, opens
   no transaction and holds no `TX` or `TM` lock (`v$lock`). `COMMIT` and `SAVEPOINT` are
   refused too. They were not chosen: `COMMIT` would keep anything already open, and a
   `SAVEPOINT` opens a transaction with a `TX` lock on a writable session. So does
   `SELECT … FOR UPDATE`, even when it selects no row (`TX` and `TM`). DML that touches no row
   does the same (seen by hand). "Changes nothing" holds only while no transaction is open, so
   the check must run before anything that could open one, the session setup (ADR-037)
   included.
3. **A read-only session refuses every `ROLLBACK` and `COMMIT`, not only after a refused
   statement** (`a_refused_rollback_ends_nothing_and_closing_the_session_does`). A statement
   it refuses leaves no transaction (`DBMS_TRANSACTION.LOCAL_TRANSACTION_ID` is null and the
   driver reports idle), so there is nothing to end. A read through a database link does open
   a transaction, which neither `ROLLBACK` nor `COMMIT` can end. The driver reports it open,
   the session still reads, and closing the session ends it. For point 7 this means the
   `ROLLBACK` that ends every agent's read is always refused, so as written, point 7 closes the
   session after every read. The cheaper ending that "What would make us revisit" asks for is
   there to be weighed: a session with no open transaction has nothing to end. That is for the
   reading again below, not decided here.
4. **A session keeps what its user was when it connected, and the dictionary says what the
   user is now** (`a_session_keeps_what_its_user_was_when_it_connected`). A session that
   connected before `ALTER USER … READ ONLY` reads `YES`, its `ROLLBACK` goes through, and it
   writes and commits. One that connected before `ALTER USER … READ WRITE` reads `NO` and still
   refuses. Each fails one of the two answers, so point 3 needs both, as it says.
5. **A link to a writable user lets a read-only session write in this same database**
   (`a_link_to_a_writable_user_lets_a_remote_function_write`). Writing to a table through the
   link is refused (ORA-28194), whether the statement names the link or a view or a synonym
   hides it, and reading through it works. But calling a function of the writable user through
   the link runs it in the link's session, which is not read-only. A function with an
   autonomous transaction commits its write there. That happened when it was called directly,
   from a view the read-only user may read, and through a synonym; in the last two the
   statement names no link. The link was a public one, made by a DBA, back into the same
   pluggable database as the writable `spelaion`. A writable user's view over its own private
   link was not tried, and neither was a remote function without an autonomous transaction.

**Answer 5 contradicts point 8**, which says a read-only user cannot change the database the
session is connected to. It also meets "What would make us revisit": in this test both of point
3's answers say read-only (`YES` and a refused `ROLLBACK`) and a write goes through. Card
SDB-474 records it, and nothing more is built on this decision (items 2 to 5) until it is read
again. Questions for that reading, not decided here: whether the check must also find that
the session can reach no database link, and how, since a view or a synonym another user owns
hides the link from the session's `ALL_DB_LINKS`; whether a DBA's promise that no link reaches
a writable user can count as part of the barrier; and how point 7's ending changes after answer 3.

## Sources (checked 2026-11-24)
- ADR-059's notes of 2026-11-15 and 2026-11-23, `crates/spelaion-core/tests/oracle.rs` and pull
  request #467 (SDB-339): everything said above to have been tried.
- Oracle's description of read-only users and sessions:
  <https://blogs.oracle.com/css-delivery/read-only-at-the-right-scope-pdbs-users-and-sessions-in-oracle-database-26ai/>,
  <https://docs.oracle.com/en/database/oracle/oracle-database/23/refrn/READ_ONLY.html> and
  <https://www.oracle-base.com/articles/23/read-only-pdb-users-23> (local users only; ORA-28194
  on `SELECT … FOR UPDATE`; sessions already connected).
