# ADR-092: Oracle's read barrier is a read-only user in a database with no database links; an agent's read on it ends by the driver's transaction state, never by a ROLLBACK

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-28 (accepted the same day: Pablo chose the recommendation, (a) with (i),
  over (c))
- **Supersedes:** ADR-080, points 3, 6, 7, 8 and 9, and its work items. Its points 1, 2, 4 and
  5 stand.
- **Builds on:** ADR-080's note of 2026-11-28 (SDB-474), ADR-020 (each engine's barrier, and
  nothing offered until its get-around tests pass), ADR-031 (the transaction state comes from
  the driver)
- **Decides:** SDB-542

## Context
ADR-080 made Oracle's read barrier a user the database keeps read-only (`ALTER USER … READ
ONLY`, 23ai and later). It is checked when each session opens, with two answers: the
dictionary's (`SYS.USER_USERS.READ_ONLY`) and the session's own (a write that is refused).
SDB-474 gave `testing/docker` such a user, `spelaion_ro`, and ran the experiment. Two findings
contradict the decision as written.

- **A database link lets a read-only session write in its own database.** A link opens a
  session of its own on the other side, as the link's user, and that session is not read-only.
  Everything below is kept in `crates/spelaion-core/tests/oracle.rs` and ran against Oracle
  Free 23.26.3:
  - Through a public link to the writable user `spelaion`, a remote function with an
    autonomous transaction commits. That happens when it is called directly, from a view the
    read-only user may read, or through a synonym
    (`a_link_to_a_writable_user_lets_a_remote_function_write`).
  - Through the same link, a remote procedure that commits, called from the session's own
    PL/SQL block, writes with no autonomous transaction at all. A remote function without one
    is refused inside a query (ORA-14551)
    (`a_writable_users_code_reaches_its_private_link_where_the_session_cannot_see_it`).
  - **A writable user's private link is reached by that user's own code.** The read-only user
    needs no `CREATE DATABASE LINK` and no grant on the link. The link does not resolve from
    the session's own statements, even inside that user's view or synonym (ORA-04054). But the
    writable user's definer's-rights code resolves the link in its owner's schema and writes
    through it. That holds for a function, for a procedure, and for a function that names the
    link only inside `EXECUTE IMMEDIATE`. The last one leaves no trace in `ALL_DEPENDENCIES`
    (same test).
  - Writing to a table through a link is refused (ORA-28194), directly and through a view or a
    synonym.
- **What the session can see of links.** `SYS.ALL_DB_LINKS` shows public links and the user's
  own, not other users' private links (same test). `SYS.DBA_DB_LINKS` shows every link of the
  database, but the read-only user can read it only if a DBA allows it. Without that it gets
  ORA-00942. With `SELECT_CATALOG_ROLE` it sees both kinds (tested). The narrow grant, `GRANT
  SELECT ON SYS.DBA_DB_LINKS`, can only be given by `SYS`: `SYSTEM` gets ORA-01031 (seen by
  hand, not kept as a test).
- **A read-only session refuses every `ROLLBACK` and `COMMIT`**, even with nothing open
  (`rollback_is_the_write_a_read_only_session_refuses_and_a_writable_one_ignores`). A
  statement it refuses leaves no transaction. A plain read leaves none either, and the driver
  reports the session idle. A read through a link opens a transaction, and so does `SET
  TRANSACTION READ ONLY`. The session cannot end either of them; only closing it does
  (`a_refused_rollback_ends_nothing_and_closing_the_session_does`,
  `set_transaction_read_only_traps_a_read_only_session_in_its_snapshot`). In that read-only
  transaction every later read sees the data as of its start, so an agent would read stale
  data for as long as the session lives (same test).
- The rest of ADR-080 held against the tests. Under the read-only user, these all fail with
  ORA-28194 and write nothing, while the same functions write under their owner: functions with
  the session's rights and with the owner's, functions that undo `DISABLE COMMIT IN PROCEDURE`
  and `READ_ONLY = TRUE` first, and a scheduler job (it is created and runs, and its write
  fails). A session keeps what its user was when it connected, and needs both answers
  (`the_read_only_user_refuses_every_way_around_that_writes`,
  `a_session_keeps_what_its_user_was_when_it_connected`).

## Options for point 8 (what the barrier covers)
- **(a) A read-only user, and a check that the database has no database link at all,
  asked of `SYS.DBA_DB_LINKS`.** The check needs the DBA to let the read-only user read that
  view. A missing grant (ORA-00942) is a no, like every failed question (ADR-080, point 4).
  It covers every link the tests found a way through: public ones, the user's own, and other
  users' private links reached by their code, static or dynamic. It cannot be narrowed to
  "no link to a writable user". A link names a user in another database, whose state we
  cannot read, and a loopback link is only an address that happens to point back here.
  - **Not enough: the session's own `SYS.ALL_DB_LINKS`.** It misses exactly the private links
    that a writable user's definer's-rights code reaches (tested). Reading
    `SYS.ALL_DEPENDENCIES` for links behind views and code misses the dynamic case (tested).
  - **When to check:** when the session opens, after the two answers of point 3, and again
    before each agent's read, since a writable user can create a link later. It is one query on
    the session that will run the read; its cost was not measured. A link created while a read
    runs is not seen until the next read. That window needs a writable user acting at that
    moment, not only the agent.
  - **Cost:** it excludes every database that uses links at all, and asks the DBA for one more
    grant. Only `SYS` can give the narrow one. `SELECT_CATALOG_ROLE` is broader: it lets the
    agent read every schema's dictionary entries.
  - **Not covered, and not tested:** other ways definer's-rights code can reach a session that
    is not read-only. Examples are `UTL_HTTP` or `UTL_TCP` to a service that writes back (these
    need a network access control entry granted to the code's owner), external procedures, and
    Java stored procedures. The classifier's names (ADR-080, point 6) do not see inside that
    code.
- **(b) Refuse every statement that names a remote object.** The dialect's scanner reads the
  statement's text. It cannot see a link behind a view, a synonym or a public synonym, or
  inside a function the statement only calls, and tests show writes through each of those.
  It stays what ADR-080's point 6 made it, a clearer message, and is not a barrier.
- **(c) Oracle keeps no read-only mode and no agents.** This is what `dev` does. Nothing to
  build or get wrong. Oracle is the engine most asked for (SDB-37), and with (a) the barrier
  would already be narrow: 23ai or later, a DBA's `READ ONLY`, no links, one more grant.
- **(d) `OPEN_LINKS = 0` for the pluggable database.** Every session there could then use no
  link. `V$PARAMETER` reports it as settable per pluggable database but static (seen by hand),
  so it needs a restart. The read-only user cannot read it without another grant. Not tried:
  it would restart the shared test database. It is a candidate second answer to "no link"
  later, with its own tests.

## Options for point 7 (how an agent's read ends)
- **(i) By the driver's transaction state (ADR-031).** After each read the core asks the
  driver. If the session is idle, there is nothing to end: no `ROLLBACK` is sent and the
  session is used again. If it is open, or the state cannot be read, the session is closed,
  which ends its transaction on the server; the reason goes to the audit log and the app log,
  and the next read opens a new session and checks it. The tests show the driver's state
  agrees with the database's (`DBMS_TRANSACTION`) in each case tried: idle after a read and
  after a refused statement, open after a read through a link.
- **(ii) Close the session after every read.** Always correct, and it costs a connection and
  the check for every read. ADR-080's "What would make us revisit" already asked for something
  cheaper.
- **(iii) Send the `ROLLBACK` and take ORA-28194 as success.** Rejected. The refusal is the
  same whether or not a transaction is open, so it would reuse a session that still holds one.

## Decision
1. **Point 8 becomes option (a).** On Oracle, read-only connections and agents' reads are
   offered only on a session whose user the database keeps read-only (ADR-080, point 3's two
   answers) and whose database has no database link, as `SYS.DBA_DB_LINKS` says when the
   read-only user asks it.
   - The links answer is asked when the session opens and before each agent's read. Any link,
     an unreadable view or an answer the core does not recognise is a no, and the refusal says
     which. The form, the connection test and the guide (item 4) give the DBA the statements:
     `ALTER USER … READ ONLY` and `GRANT SELECT ON SYS.DBA_DB_LINKS TO …` (run as `SYS`), and
     they say that a database with links is not offered.
2. **Point 3 gains that third answer**, with the same rules as the other two: never promoted
   later, checked again on every new session.
3. **Point 6: no `SET TRANSACTION READ ONLY` on Oracle.** A read-only session can never end
   that transaction, and every later read in it sees stale data. `ALTER SESSION DISABLE COMMIT
   IN PROCEDURE` when the session opens stays: it opens no transaction (the tests ran it on the
   read-only user).
4. **Point 7 becomes option (i).** The error reported is still the read's own, and a refused
   ending is never sent in the first place. An editor tab still shows the driver's state
   (ADR-031). A refused `ROLLBACK` the user types there is shown as the error it is.
5. **Point 9 (nothing offered until the tests pass) gains these get-around tests**, in
   `tests/read_only_connection.rs` and `tests/agents_engines.rs`: each way through a link
   above, with the check refusing the session; a link created after the session opened, refused
   at the next read; a database with no link, offered; and the untested channels named under
   (a), each either refused by a check or shown not to write. If one of those writes and no
   dictionary answer can rule it out, Oracle falls back to (c): no read-only mode and no agents.

**Chosen: (a) with (i)**, the recommendation, by Pablo on 2026-11-28 ("Read-only + no links").
The alternative was (c).

(a) is the smallest change that answers every way through the tests found. The database
enforces each part: the user's `READ ONLY` and a dictionary with no link in it. SpelaionDB only
refuses unless both hold.

What would send Oracle to (c): a get-around test of item 2 that writes through a channel no
dictionary answer can rule out (a definer's `UTL_HTTP` call, an external procedure, Java), or
evidence that the 23ai databases people use with SpelaionDB nearly all have links, so (a)
would reach almost nobody.

## Options not taken
Options (a) with only `ALL_DB_LINKS`, (b) alone, (d) for now, (ii) and (iii), as weighed
above.

## What would make us revisit
- Oracle gives a way to stop a session's links that a function cannot undo, or to make a link
  inherit its caller's read-only state: it replaces the links answer.
- `OPEN_LINKS = 0`, or a database open read-only (ADR-080's "What would make us revisit"),
  proven with its own tests, becomes a second accepted answer for the pluggable database.
- The check passes and a write goes through in any test or report: Oracle goes back to no
  read-only mode and no agents the same day (as in ADR-080).
- The links answer before each read proves too slow: it moves to the session's opening only,
  and the window it leaves is weighed again.

## What follows as work items
SDB-475 to SDB-478 (ADR-080's items 2 to 5) are rewritten to this decision:
- **Item 2** (the check, the session's fact and the reads' ending) adds the links answer,
  drops `SET TRANSACTION READ ONLY` for Oracle, and ends reads by the driver's state.
- **Item 4** names the grant and the no-links condition.
- **Items 3 and 5** are unchanged except that they wait for item 2.

Until item 2's get-around tests pass, nothing turns on read-only mode or agents on Oracle.

## Notes

**2026-11-28: Java writes, so a fourth answer: no Java outside Oracle's own schemas (SDB-476;
Pablo chose "No user Java + full image").** The channel tests of point 5 found that a
writable user's Java stored function writes under the read-only user. It opens a JDBC
connection back into this database as its owner, and that new session is not read-only
(`a_writable_users_java_writes_under_the_read_only_user_once_it_may_open_a_socket`). It needs
only a `java.net.SocketPermission` for its owner. Oracle's default Java policy already gives
any socket to the roles `JAVAUSERPRIV`, `JAVASYSPRIV`, `EJBCLIENT` and `JMXSERVER`, and network
access control entries play no part.

- **The answer:** read-only mode and agents on Oracle are refused while any Java object
  exists outside Oracle's own schemas. The count covers every `JAVA%` object type in
  `SYS.DBA_OBJECTS` whose owner is not marked `ORACLE_MAINTAINED = 'Y'` in `SYS.DBA_USERS`; an
  owner missing from `DBA_USERS` counts as not Oracle's. Both views are asked by their full
  `SYS` names, and the answer must be zero.
- **When:** like the links answer, when the session opens and again before each agent's read,
  on the session that will run it.
- **The grant:** the read-only user reads both views only under a DBA's grant (`SELECT` on
  each, given by `SYS`, or `SELECT_CATALOG_ROLE`). Without it the question fails, and a failed
  question is a no.
- **Tested** (`the_java_answer_finds_a_users_java_and_none_on_a_clean_database`): it is zero on
  `testing/docker`'s clean database, which holds tens of thousands of Oracle's own Java classes.
  It counts the writable user's Java source and class once that user loads them, and is zero
  again once they are dropped.
- **Why this and not a check of `SocketPermission`:** a Java policy check would have to follow
  every permission that reaches outside the session, not only sockets. That includes file
  permissions (a class can write files another process reads) and the permission to run an
  operating-system process (a class could start SQL*Plus as a writable user; neither of these
  was tried), and every role that carries them, directly or through other roles. Refusing any user Java covers all of
  them, and every grant made later, with one count. The cost: a database whose users keep Java
  of their own is not offered.

`UTL_HTTP`, `UTL_TCP` and external procedures could not run in `testing/docker`'s lite image
(its README says why). As Pablo decided, they are tested once on the full Oracle Free image,
an optional service of `testing/docker`, before anything is turned on.

## Sources
- `crates/spelaion-core/tests/oracle.rs` (the tests named above), against `testing/docker`'s
  Oracle Free 23.26.3 and its read-only user (`oracle/startup/02-spelaion-ro.sql`). They need
  `SPELAION_TEST_ORACLE`.
- ADR-080 and its note of 2026-11-28; pull request #593 (SDB-474).
- Seen by hand in SQL*Plus on the same container, not kept as tests: `SYSTEM` cannot grant
  `SELECT ON SYS.DBA_DB_LINKS` (ORA-01031); `OPEN_LINKS` is static and settable per pluggable
  database (`V$PARAMETER`).
