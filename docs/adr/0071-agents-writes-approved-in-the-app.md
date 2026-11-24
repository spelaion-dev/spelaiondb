# ADR-071: A person approves an agent's write in the app, where the connection, its type or the user asks for it

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Extends:** ADR-019 (agents' writes: its `"confirm"`, left for later), ADR-029 (connection
  types: one more safeguard, which only adds up), ADR-033 (the MCP page: its Security and
  Activity tabs)
- **Builds on:** ADR-004 (no daemon), ADR-008 (the layers), ADR-060 (a lifecycle is one enum),
  SDB-397 (one app per personal folder, reached over a local port)
- **Decides:** SDB-38

## Context
An agent writes through `execute_write` only where the connection says `[mcp] writes =
"allow"` (ADR-019). The answer is all or nothing: `spelaion_core::agent_access` (`write.rs`)
returns `ReadWrite` or `ReadOnly(reason)`. The third value of the file format, `"confirm"`, is
read and then refused as `ConfirmUnsupported`, and `spelaiondb check` warns that it "is not
supported yet". ADR-019 left it for the day there was a way to ask a person, and ADR-019 and
ADR-033 both named that day: when MCP clients commonly support elicitation.

What exists today, checked on `dev`:

- **`spelaiondb mcp` is a process of its own**, started by the AI client, with its own tunnels
  and sessions (ADR-004). ADR-004 planned, for a "v0.3" that was never built, that the app would
  serve MCP over a local channel and that writes would be confirmed in an app dialog.
- **The app already listens for other processes of the same user.** Since SDB-397
  (`spelaion-core`'s `instance.rs`) the first app of a personal folder listens on a free port of
  `127.0.0.1` and writes the port and a random token to `desktop-instance.json` in that folder,
  readable only by its owner. A second launch sends one line of JSON with the token and gets
  `ok` or `no`. An app that could not listen runs alone (`Claim::Alone`).
- **AI clients already ask before a tool call.** `execute_write` is marked destructive, and
  most clients show such a call and wait for the user. That prompt is the client's:
  the user, or the client's own configuration, can turn it off for a tool, and nothing in a
  connection file or a connection type can require it.
- **The SDK can ask through the client.** The pinned `rmcp` (3.9.0) has `elicit` for the
  server's side. Its own documentation says that protocol version `2026-07-28` forbids requests
  from the server to the client, and that a server must then answer the call with an "input
  required" result that the client fulfils before calling again. So the protocol's way of
  asking is in the middle of a change.
- **On Production the editor asks the person** before any statement that is not a read
  (`confirm_data_changes`, ADR-029). An agent on the same connection, with `writes = "allow"`,
  is not asked by us at all.

Another client has an approval gate with three modes (off, writes, every query). Its dialog
shows the query, which can be edited, and a plan with estimated rows, and it has a timeout, a
notification and an option to bring the dialog to the front.

The card asks five hard questions: the channel without a daemon, what happens when the app is
closed, editing before approving, the audit, and how it adds up with connection types. Points
5 to 12 answer them.

## Decision
1. **Yes: a person can be asked to approve an agent's write in the app before it runs. Only
   writes.** The approval is one more safeguard. It never gives an agent anything it does not
   have today. What it adds for users is a third answer between "agents only read" and "agents
   write unattended", so connections that stay read-only today can let agents write.
2. **Three places can ask for it, and any one of them is enough** (safeguards only add up):
   - the connection: `[mcp] writes = "confirm"`, the value the file format already has;
   - its connection type: a new safeguard, `confirm_agent_writes`. It adds up across layers as
     the others do (`Safeguards::tighten`) and is part of the trust snapshot of team types, so
     a team can require it and no personal file can switch it off (ADR-029, points 5 and 6);
   - the user: a personal setting in the store, `mcp.approval` (`off` or `writes`), next to
     `mcp.read_only` and switched in the MCP page's Security tab. It is read on every call. If
     it cannot be read, or holds another value, approval is asked.

   `agent_access` stays the one place that decides, with three answers in place of two:
   read and write, write after approval, and read only with its reason. Every read-only reason
   wins over an approval: the user's read-only mode for every agent, the per-connection
   restriction, a `read_only` connection and an engine without agents' writes refuse as today.
   `list_connections` tells the agent which connections ask, before it tries.
3. **The built-in Production type does not turn `confirm_agent_writes` on.** ADR-019 let users
   choose unattended writes on production, and a safeguard cannot be taken back once a type has
   it. A team turns it on for its own Production in `connection-types.toml`. The connection
   form offers "write after I approve" and keeps its warning on unattended writes to a
   production connection. This is the close call of this decision: see "What would change
   this".
4. **An approval never widens what may run.** The statement goes through every barrier first
   (`check_write`: one statement, the allow list of kinds, never destructive, nothing that
   reaches outside). A statement a barrier refuses is refused at once, and the person is never
   asked about it. `Session::run_write` checks it again when it runs, as it does today. ADR-019
   and ADR-020 keep their rule: if agents need a statement the barriers refuse, that is a
   narrower exception decided on its own, not something a click lets through.
5. **The app is the only approver, and the server reaches it over the channel the app already
   has.** `spelaiondb mcp` reads `desktop-instance.json` in its personal folder, connects to
   the app's port and sends the token with a request. The app answers on that same connection.
   This builds the "writes are confirmed in an app dialog" half of ADR-004's plan and not the
   other half: the statement still runs in the `spelaiondb mcp` process, on its own session.
   There is no new process, nothing starts with the system, and the server never starts the
   app.
   - **The request carries what the dialog shows:** the statement, the connection's id, name
     and type, the workspace folder, the client's name and the server's audit session, a
     deadline, and a protocol version. The answer carries **only** approved or denied, the
     fingerprint of the statement the app showed, and an optional note when denied.
   - **SQL never travels from the app to the server.** Whoever holds the token can then, at
     most, say yes to something an agent already asked for. It cannot make the server run a
     statement of its own.
   - The request and its states are one core type (ADR-060): pending, then approved, denied,
     expired or withdrawn. The app's adapter shows it and passes the click on.
6. **It fails at once when nobody can answer, and it never waits for ever.**
   - No file, nobody listening, a wrong token, an app that does not know the request's version,
     or an app that runs alone: the write is refused immediately with its own code, and the
     sentence says to open SpelaionDB. Nothing is queued and nothing is retried (ADR-033).
   - The server waits at most the user's setting (`mcp.approval_wait`, 120 seconds by default,
     between 10 seconds and 10 minutes), then refuses with another code and withdraws the
     request. The dialog closes saying the agent stopped waiting.
   - If the app closes or the connection drops while waiting, the write is refused.
   - If the AI client cancels the call, the server withdraws the request. An approval that
     arrives after a refusal or a withdrawal runs nothing: the answer has nowhere to land.
7. **What was approved is what runs, and every check runs again after the click.** The server
   runs the write only if the fingerprint in the answer is the fingerprint of the statement it
   holds, as ADR-033 does for a client's file. After the approval and before the statement, the
   configuration, the trust store and `agent_access` are read again, as the server already does
   around opening the write session. A user who makes every agent read-only while a request is
   pending wins. Where approval is asked, the core starts a write only with an approval value
   that the exchange with the app creates for that statement, so no code path can skip the
   question.
8. **The statement cannot be edited in the dialog.** The person approves or denies, and a
   denial may carry a short note that goes back to the agent in the refusal. The agent then
   sends a corrected statement, which goes through the barriers and a new approval. Editing
   would need SQL to travel from the app to the server (point 5), the agent would believe it
   ran something else, and the audit would need two statements for one call.
9. **No transaction stays open while a person thinks.** The write is not started, shown and
   then held for a commit: its locks would wait on a person, on production. The first version
   shows no estimate of rows either. An estimated plan of a write, never one that executes it,
   may be added later where an engine can give it inside the read barrier. That needs an
   experiment per engine first (see "What follows").
10. **The dialog shows what will run, as text nobody else chose.**
    - It shows the whole statement as it will run, the connection with its type's badge, the
      workspace, the kind of statement and its target as the core's parser read them, the
      client's name marked as what the client says about itself, and the time left.
    - The statement is an agent's text. It is drawn as text (the window's policy, SDB-127),
      control and bidirectional characters are shown as visible escapes, and a long statement
      scrolls and is never cut. The summary comes from the core, not from the text's comments.
    - Focus starts on Deny. Approve is never the default button and has no key of its own, so
      a key meant for another window cannot approve.
    - The app raises a native notification (`tauri-plugin-notification` is already a
      dependency) and asks for attention in the taskbar or dock. Bringing the window to the
      front is a personal option, off by default. These preferences, the mode and the wait
      live in the Security tab. None of them changes what is approved.
    - A pending request is kept in memory only. The statement is written nowhere until the
      server records the call in the audit, as it does today.
11. **Requests are shown one at a time, oldest first, and few may wait.** Beyond a small cap,
    a new request is refused at once, so an agent in a loop cannot bury the person in dialogs.
    The dialog also offers "Deny and make every agent read-only", which flips ADR-033's switch.
    There is no "approve everything for a while": a user who wants no questions sets `"allow"`,
    and where a team's type asks, nobody can skip it.
12. **The audit says what was asked and what was answered.** The server writes the one entry
    per call it writes today, with the approval's outcome and when it was asked and answered.
    New refusal codes cover denied, no app to ask and no answer in time; none of them says
    `retry`, since a person has to act first. Pending requests appear at the top of the
    Activity tab, from the app's own state, with the same Approve and Deny. They are not audit
    entries, because an audit entry cannot change. "Who approved" is the person at the app of
    that personal folder: the audit is local, and there are no user accounts.
13. **Reads are not gated.** An agent explores with dozens of reads. A person asked about each
    one stops reading the questions, and then the gate on writes is worth less too. A read
    changes nothing and runs behind the engine's read barrier. What an agent may see is decided
    by `exposed`, by masking and by the database user.
14. **What it is and is not.** It stops the mistakes and the planted instructions of an agent
    that acts through `spelaiondb mcp`, and a team can require it. It is not a wall against a
    program that already runs as the user with a shell: that program can read the same files
    and keychain as `spelaiondb`. A read-only database user is still the only real guarantee
    (ADR-020).

## Checked against the ADRs it touches
- **ADR-004 (no daemon):** kept. The approver is the app the user opened, the channel is the
  one SDB-397 added, and without the app a connection that asks for approval refuses writes.
  Every other connection works without the app, as before.
- **ADR-019:** `"confirm"` gets its meaning. `"allow"` and `"deny"` are unchanged. A version
  of `spelaiondb` older than this decision keeps refusing `"confirm"`, which is the safe side.
- **ADR-029:** a new safeguard that only adds up, pinned by repository trust like the others.
  No existing type changes.
- **ADR-033:** the Security tab gets the mode, the wait and the notification preferences. The
  Activity tab gets the pending requests and each call's approval. The read-only switch still
  wins over everything.

## Options not taken
- **Ask through the AI client (MCP elicitation), as ADR-019 and ADR-033 expected.** It is the
  most reasonable alternative. It works with the app closed, which is ADR-004's own case
  (an agent in a terminal), and it needs no channel.
  - *Against it:* the answer comes from the program that hosts the agent. The server only
    receives a message saying "accepted" and cannot tell a person's click from a client that
    answers by itself. The clients already have their own prompt before a tool call, with the
    same limit, so this would add a second question in the same place. A gate that a team's
    type can require must not depend on how each person's client is set up. The protocol's way
    of asking is also changing between versions (see Context).
- **The app serves MCP and runs the agents' statements (all of ADR-004's "v0.3").** Tunnels
  would be shared, but agents would then work only with the app open, and the app would run
  agents' sessions next to the user's. The approval needs one question to cross, not the
  queries.
- **The local store as the channel** (the server writes a pending row in `state.db`, the app
  writes the answer). No socket, but the server cannot tell that the app is closed or has
  crashed without a heartbeat of its own, so it would wait for the whole timeout where it
  should fail at once.
- **Approval lets through what the barriers refuse** (a `DROP`, a `DELETE` without `WHERE`).
  The card names this as the value. A person reading a dialog is a weaker check than a rule,
  the statement in front of them was written by the thing being checked, and safeguards only
  add up. If a kind of statement is worth allowing under approval, it is decided by kind, in
  its own ADR, once the gate has been used.
- **Run the write, show the rows it changed, commit on approval** (ADR-008's first idea).
  The real figure, at the price of locks held while a person decides.
- **A mode that asks about every query.** See point 13.
- **Production asks by default,** or `confirm_data_changes` also covering agents' writes. It
  would make the product say one thing ("every change is confirmed") for people and agents
  alike. It also reverses ADR-019 for every user who chose unattended writes on production,
  with no way back for them. See below.
- **Not building it, and relying on the clients' own prompts.** They are a real gate for one
  person who keeps them on. They cannot be required by a team, they are not in our audit, and
  they differ in each client.

## What would change this
- **Production by default.** If the gate is used for a release and its questions are not a
  burden, the built-in Production type should turn `confirm_agent_writes` on, in a new ADR,
  because it tightens what ADR-019 allowed. If Pablo prefers that from the first version, only
  point 3 changes.
- **Most people who set `"confirm"` work without the app** (agents in a terminal, or on
  another machine). Then asking through the AI client becomes a second approver, as a
  personal choice that is named as the weaker one, once the protocol's way of asking has
  settled.
- **People keep denying with a note that only changes one word.** Then editing in the dialog
  earns its cost: the edited text would go through the barriers, and the agent would be told
  what ran.
- **People approve dozens of similar writes in a row.** Then a way to approve a pattern for a
  short time needs its own decision. Approving without reading is the failure this gate must
  not produce.
- **Teams ask for approval of reads** to control what leaves the database, not what changes.

## What follows (work items, created once this is accepted)
Several touch CODEOWNERS files (`write.rs`, `types.rs`, `trust.rs`, `spelaion-mcp`).
1. The policy in the core: three answers from `agent_access`, `writes = "confirm"`, the
   `confirm_agent_writes` safeguard with its trust snapshot and `spelaiondb check`, and the
   personal settings in the store. Tests that try to get around each source.
2. The approval exchange: the request, answer and withdrawal over the instance channel, its
   lifecycle type, the fingerprint, the version and the timeouts, without holding up a second
   launch's handover.
3. The MCP server: ask before `execute_write` runs, the new refusal codes and the audit's
   approval fields (a migration), `list_connections`, the tool's description and
   `docs/guide/mcp.md`.
4. The app: the approval dialog, the pending requests in the Activity tab, the Security tab's
   settings, the notification, the wording in `docs/ui-guidelines.md` and a smoke test step.
5. The connection form's third choice and the type form's new safeguard.
6. An experiment, before any estimate is shown: on each engine agents write to, whether an
   estimated plan of a write runs inside the read barrier without changing anything and gives a
   row estimate worth showing.
