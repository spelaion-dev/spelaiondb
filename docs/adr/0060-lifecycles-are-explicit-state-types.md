# ADR-060: Every lifecycle is one explicit state type, owned by the core

- **Status:** Accepted (being implemented one machine at a time, SDB-365 to SDB-377: see
  "Order")
- **Date:** 2026-11-19
- **Builds on:** ADR-002 (the core holds the rules; the app, the CLI and the MCP are
  adapters), ADR-010 (results live in the core), ADR-012 (CI is the reviewer), ADR-041 (the
  driver contract travels as messages)
- **Represents, without changing their meaning:** ADR-028 (lost sessions), ADR-031
  (transactions), ADR-035 (script runs), ADR-044 (pending edits)

## Context
An audit of the whole repository (2026-11-16, commit `2804533`) looked for lifecycles: things
that are in one of a few states and move between them. It found about fifty.

- **About twenty are already explicit**: one enum, matched exhaustively, with its transitions
  in one place. They need nothing: `SessionHealth` (`health.rs`), `ScriptStop` (`script.rs`),
  `StepStatus` (`conn_test.rs`), `TableStatus` (`data_search.rs`), `HostFile`
  (`spelaion-ssh`'s `hostkeys.rs`), `InstallState` (`spelaion-config`'s `mcp_clients.rs`),
  `AgentAccess` (`write.rs`), `StartupAction` (`startup.rs`), and the job dialogs' `Phase`
  unions in the page.
- **About thirty are implicit**, in one of three shapes:
  - several booleans and `Option`s that together mean a state and can contradict each other
    (`TransactionStatus`'s six fields, a stored result's `complete: bool`, a data search's
    `cancelled` and `stopped`, which can both be true);
  - a status carried as text (a tunnel "down" with the reason `"opening"`, an audit decision,
    an import choice where any unknown word means "import");
  - one question ("is it busy?", "is a transaction open?", "was it cancelled?") answered in
    several places by different code, in the core and again in the page.
- **Three of them misbehave today:** cancelling "Run SQL file…" set to continue on errors ran
  the rest of the file (SDB-365, fixed in 0.11.0); a tunnel hop lost while a later hop opens is
  reported as up (SDB-366); a result cut short by a cancel, an error or a timeout looks whole
  (SDB-367).
- **Every new job has invented its own flags.** Seven kinds of job say "running, cancelled,
  finished, failed" in seven ways, with three cancel types that have the same fields and five
  ways of saying "cancelled". SDB-365 was a cancel type that went around the session.

The code here is written by agents and nobody reads it line by line (AGENTS.md, ADR-012). The
compiler can check an enum's transitions and every place that reads it; it cannot check that
three flags agree. A state held in three flags is exactly what our only reviewers cannot review.

## Decision
**Every lifecycle is one type, owned by the core and passed through by the adapters. Work that
runs shares one vocabulary. The implicit machines are made explicit one at a time, each in its
own card and pull request, with behaviour unchanged unless the card names a bug.**

1. **A lifecycle is one type.** A Rust enum, matched exhaustively (AGENTS.md's "no `_ =>`"),
   whose variants carry the data that only exists in that state (a reason, a ticket, a count of
   rows, bytes received of a total). Its transitions are written in one place: the actor or the
   type that owns it, never assembled by whoever reads it. No pair of flags that can disagree,
   no `Option` whose emptiness means two different things, no status as free text. When a
   state only exists in the page (a dialog's phase, a value being loaded), it is a TypeScript
   discriminated union, and a `switch` over it ends in a `never` check so a new variant fails
   the build.
2. **The core owns it; the adapters pass it through.** The desktop adapter, the CLI and the MCP
   server hand on the core's type: to the page as a generated type (`bindings.ts`), to an agent
   as a stable code with the sentence beside it (SDB-374). The page does not rebuild a state
   from events or re-derive it from flags; it reads the state it is given, through one set of
   selectors when several components ask the same question (SDB-371). Where a state changes
   while something watches it (a session's health, its transaction, its run), the core
   publishes it on a watch, sent only when it changes, as `Session::health()` and
   `Session::transaction()` already are.
3. **One vocabulary for work that runs.** Every job in the core (export, import, backup and
   restore, file run, data search, column statistics, connection test) takes one cancel type,
   made from the session it stops, so attaching it cannot be forgotten and a cancel that came
   early is not lost; and ends with one generic ending: **done** (with what it returns),
   **failed** (why), **cancelled** (with what was already committed or written), **stopped at
   a limit**. A new job has nothing to invent (SDB-368).
4. **A value from outside is never read as a known one.** A state read from the store, a file,
   an older build or the plugin wire is parsed into the enum at the boundary; a value it does
   not know is kept as unknown or refused, never taken for a valid one (SDB-376). The store's
   migrations still only add (ADR-011).
5. **One machine at a time.** Each implicit machine is its own card and pull request. Its first
   commit is a test that pins what happens today; behaviour does not change unless the card
   names a bug, and then the test that shows the bug fails first. Bindings are regenerated in
   the same pull request. A card that touches a security file (`.github/CODEOWNERS`) waits for
   an owner, as always.
6. **Anything added to the driver contract travels as a message** (ADR-041). None of the cards
   below changes the plugin wire.

### What this does not change
This decision says how existing states are represented, not what they are:

- **ADR-028.** A lost session is still noticed, shown with its reason, and reconnected only on
  a click, with nothing re-run. `SessionHealth` already is the explicit type; what changes is
  that the tab's "lost" (SDB-371) and the tunnel's "down" (SDB-366) read it as a state instead
  of a flag, and the first reason wins everywhere, as it does in `SessionHealth`.
- **ADR-031.** The transaction state still comes from the driver, read after every statement
  and every commit or rollback; the UI never infers it. `TransactionStatus` becomes an enum in
  which "not followed" cannot carry a state and "switched by smart commit" cannot exist without
  smart commit (SDB-372). What closing asks is unchanged; it is asked in one function.
- **ADR-035.** The "ask" pause and its ticket stay the core's, answered the same way; a cancel
  still always ends the run; the stops it names stay. The pause and a cancel on its way become
  states of a published run state (SDB-370), a statement's ending is one enum instead of
  `cancelled` and `timed_out`, and "the session was lost while it waited" is told apart from
  "its tab closed" in the summary.
- **ADR-044.** Pending edits still live in the core with the stored result of their page, and
  freeing the result still drops them. A freed result becomes a typed error instead of three
  texts (SDB-367), and the page keeps "saving edits" in one place instead of three (SDB-371).

## The machines
One card per machine or per group of machines that change together. The audit's findings and
the file and line references are in each card.

| Card | Machine(s) made explicit | Depends on |
|---|---|---|
| SDB-365 | "Run SQL file…" cancel: it now goes through `Session::cancel` and stops the rest of the file (bug, done in 0.11.0) | none |
| SDB-366 | SSH tunnel state: opening, up, down (reason), closed, first reason wins; the Kubernetes tunnel's state; the tunnel's shape (SSH hops, proxy only, SSM only) as one enum (bug) | none |
| SDB-367 | A stored result's end: loading, whole, or cut short (cancelled, failed, timed out, connection lost); a freed result as a typed error; the grid row cache's failed block (bug) | none |
| SDB-368 | The jobs' cancel token and ending; the read barrier's sequence as one function; the adapter's seven outcome types as one generic type | SDB-365 |
| SDB-370 | A session's run state as a watch: idle, running (which statement), paused (ticket), cancelling; a statement's ending as one enum; the script summary always says how it ended | SDB-368 (same ending words) |
| SDB-372 | `TransactionStatus` as an enum; "must closing ask" in one function | none |
| SDB-369 | The desktop's registry of running work: ids made in Rust, entries removed when their handle drops, a cancel that names the kind | SDB-368 (holds its cancel type) |
| SDB-371 | A tab's connection as one union (disconnected, connecting, connected, lost, idle-closed) and its run mirroring the core's; one module of selectors with a check script | SDB-370, SDB-372 |
| SDB-373 | A plugin process's state; a plugin's build state; installing and install failed held by the adapter | none |
| SDB-374 | The MCP's refusals and failures as a stable code with "can run again"; "usable" and "read-only for every agent" asked once | SDB-368, SDB-370; reads SDB-373's reason when it is there |
| SDB-375 | Dialogs: one `Loaded<T>`, one hook for "preview, then apply", one for a job's phase; the data search, the grid's find and the connection import as unions | SDB-368 (the phase mirrors its ending) |
| SDB-376 | Statuses that cross a boundary as text (import choice, audit decision, history status, the active tab, bookmark kind, update check, imported passwords) parsed into enums, unknown kept as unknown | none |
| SDB-377 | The app's own lifecycle: installing an update, switching folder (a guard that restores the state when dropped), quitting | none |

## Order
1. **What misbehaves today, first:** SDB-365 (done, in 0.11.0), SDB-366 and SDB-367. They do not depend on
   each other or on anything below; each fixes its bug with the new type.
2. **The vocabulary of work that runs:** SDB-368, after SDB-365, before any card that holds a
   cancel or an ending.
3. **The rest**, in this order where they depend on each other:
   - the core's session state: SDB-370 (after SDB-368) and SDB-372 (any time);
   - the desktop that holds or mirrors it: SDB-369 (after SDB-368), then SDB-371 (after SDB-370
     and SDB-372);
   - SDB-374 for agents, after SDB-368 and SDB-370;
   - SDB-375 for the dialogs, after SDB-368;
   - SDB-373, SDB-376 and SDB-377 depend on nothing here and can go at any point.

Cards with no dependency between them may run at the same time, one pull request each, as long
as two of them do not change the same files.

## Left alone on purpose
- **The plugin protocol's wire types** (`Message`'s five `Option`s, the order of `ExecEvent`s):
  a breaking change needs a new major protocol version, and the order is already checked at run
  time.
- **The trust store's file format** (trusted, legacy, pinned): a security file, and no change
  the user would see.
- **`thread_cancel`:** implicit, but consistent under one lock with drop guards.
- **Engine swaps after a plugin change:** sessions already open keep the old engine. That is
  intended; SDB-377 writes it down, it does not change it.

## Alternatives
- **A state machine library** (`statig` in Rust, XState in the page): a second way of writing
  what an enum and `match` already do under the rule "exhaustive `match` over our own enums",
  and a new dependency in both halves of the app. The compiler already checks an enum; it would
  check a library's macros less directly.
- **One rewrite of everything:** a pull request touching fifty lifecycles cannot be shown not
  to break anything, and CI is the only reviewer. One machine per pull request, behind a test
  that pins today's behaviour, can.
- **Leaving it and fixing each bug alone:** every new job has invented its own flags so far
  (seven kinds of job, five ways of saying "cancelled"), and SDB-365 came from exactly that.
  The next job would add an eighth.

## What would change this
- If a machine turns out to need hierarchical or parallel states that an enum and one `match`
  express badly (a dialog that runs several jobs at once, each with its own phase), that
  machine may use a reducer or a small library behind the same rule: one type, owned in one
  place, read exhaustively.
- If a card shows that making a state explicit must change behaviour the user relies on, that
  card stops and asks; it does not change it under this ADR.
