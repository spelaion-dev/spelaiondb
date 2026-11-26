# ADR-089: Saved tasks are local data that run only when someone starts them, from the app or the CLI, with the confirmations of a run by hand

- **Status:** Accepted
- **Date:** 2026-11-26
- **Builds on:** ADR-004 (no daemon), ADR-009 (configuration and secrets), ADR-011 (local
  storage), ADR-022 (repository trust), ADR-029 (connection types), ADR-034 (streaming exports),
  ADR-038 (importing data), ADR-049 (backup and restore), ADR-053 (read-only connections),
  ADR-060 (lifecycles), ADR-075 (several folders open), ADR-084 (saved ER diagrams)

## Context
The same export, import or backup is often repeated every week with the same options, and each
time the user opens its dialog and fills it in again. SDB-33 saves any export (ADR-034), import
(ADR-038), dump or restore (ADR-049) or run of a `.sql` file (ADR-049 point 9) as a named task
that runs again from a Tasks view or from the command line (`spelaiondb task run NAME`), and
keeps a history of its runs with their logs.

The existing ADRs do not say:

- **where a task is kept**: the local store (`state.db`, ADR-011), like ER diagrams (ADR-084),
  or files in the project (`.spelaion/tasks/`), shared with the team;
- **how the CLI finds a task's connection and its secrets** when no window is open, nothing is
  connected and nobody may be at the keyboard (the user's own scheduler calling the CLI);
- **what "confirmed" means for a run that nobody sees**: ADR-038 and ADR-049 say the dialog is
  the confirmation of an import, a restore or a file run, every time;
- **what a run leaves behind**: a history and logs that must never hold a secret.

ADR-004 rules out the obvious shortcut: the app does not schedule anything and there is no
daemon. A task runs when a person starts it, or when the person's own scheduler (cron, Task
Scheduler, a CI job) runs the CLI.

## Decision
1. **A task is local data in the store**, in the personal folder's `state.db` (`spelaion-store`'s
   `tasks.rs`), never in a repository, a configuration file or an MCP answer; agents neither
   read nor run tasks. A task has a **name**, unique among the user's tasks (the CLI's `NAME`),
   and a **folder**, a label of `/`-separated parts that only groups tasks in the Tasks view.
   Both are bounded plain text (`check_plain_text`, the rule of a connection's description).
   A different personal folder (`--user-dir`, a profile) has its own tasks.

2. **A task is its options, never a copy of what they produce or what unlocks them.** It is one
   JSON document with a version (`spelaion-core`'s `task/`), written from and read into the
   core's own request types, never stored as the UI sent it:
   - **its connection**: the connection's key (ADR-075: the repository's canonical root, or
     none for a personal connection, and its id), and, for a personal connection, the
     repository folders it was saved with (see point 4);
   - **its kind and request**: an export (a query, a table with its filter and order, or
     explorer tables; the export options of ADR-034 and SDB-391; an output folder and a file
     name pattern), an import from a file (ADR-038's request: the file, how it is read, the
     target table and the column mapping), a dump or a restore with the vendors' tools
     (ADR-049's requests, the chosen tools folder included) or with SQLite's and DuckDB's own
     commands, or the run of a `.sql` file (the file's path, not its text: the file is the
     record, as in ADR-049 point 9).
   Paths are absolute. A task never holds a password, a passphrase, a token or a one-time code:
   there is no field for one, and saving is refused when any text of the document (an SQL
   query, a filter, a path, a tool's extra arguments) contains one of the connection's resolved
   secrets, and when a tool's extra arguments hold a `key=value` named like a secret
   (`--password=…`), with the connection test's `Redactor` (ADR-032) as the judge. A test
   saves tasks from connections with passwords and reads every column of the store back.

3. **The core checks a document when it is saved and every time it is read back**: the
   version, the name and folder, the request through the checks a run by hand applies before
   anything starts (the export's and the backup's file name patterns, the import's types and
   mapping, the tools' extra arguments split by the core), absolute paths. A document that
   fails when read is listed as unreadable and runs nothing: it is never applied in part. A
   newer version than this program reads is unreadable, not guessed.

4. **The app and the CLI resolve a task's connection the same way, in the core**
   (`task::resolve`): the folder of a repository connection is loaded alone with the personal
   layer (ADR-075), and runs only while the trust store trusts it as it is (ADR-022), as the
   app and `spelaiondb mcp` require. A personal connection is resolved with the repository
   folders the task recorded when it was saved, each loaded alone and used only to tighten
   (`Resolver::with_open_folders`: their connection types' safeguards, their `known_hosts`,
   and the network profile only a folder has), so a task resolves the same whether the window
   has other folders open or none, and from the CLI, which opens no folder. A recorded folder
   that is gone, or no longer trusted, is left out and the run's log says so; a connection
   whose key no longer exists fails the run with that sentence. Editing a task can point it at
   another connection.

5. **Secrets and dialogs.** Secrets are resolved as for any connection (ADR-009: keychain, an
   environment variable, a secret manager), never read from the task.
   - **In the app**, a password the connection asks for (`prompt`, or not in the keychain yet)
     is asked in the usual dialog, or taken from an open session of the same connection, and
     the tunnel's questions (an unknown host, a passphrase, a second factor) go to the window's
     usual dialogs.
   - **The CLI never asks**, like `spelaiondb mcp`: a connection that needs a person (a
     `prompt` password, a host key not yet known, a passphrase without an agent, a second
     factor) fails the run with a sentence saying how to run it unattended (keep the password
     in the keychain, `env:` or `op://`; connect once from the app to learn the host key). A
     changed host key is the blocking error it always is.

6. **A task runs with the barriers and confirmations of a run by hand, decided in the core.**
   Before anything is sent, the core prepares the run (`task::prepare`) from the same functions
   the dialogs call (`rerun_check` and `check_script` for an export of a query, `plan_import`
   and the guard for an import, `preview_restore`, `plan_sql_file`) and says whether it **needs
   a yes**: exactly when the run by hand would ask. An export needs one only where the
   connection's type confirms execution (ADR-034 point 6); an import where the guard lists
   risks (ADR-038 point 7: replacing a table, Production's data changes); a restore and a file
   run always (ADR-049 point 6); a dump never. A read-only connection refuses an import, a
   restore and a file run in the core whatever the yes (ADR-053).
   - **In the app**, Run shows what the dialog would have shown (the statements, the risks,
     the restore's warning about the vendor's client) in the usual confirmation, every time it
     needs one.
   - **In the CLI**, `spelaiondb task run NAME` prints the same and refuses a run that needs a
     yes unless the command says `--yes`. Writing `--yes` in a scheduler's command is the
     user's own confirmation, given once for that command; it is never stored in the task.
   - The core refuses an unconfirmed run that needed a yes, whatever the adapter did.

7. **Nothing runs on its own.** A task runs when Run is clicked in the app or the CLI is
   started. The app never starts one at a time, at start, on reconnect (ADR-037) or after a
   failure; a failed run is not retried. There is no daemon and no schedule in SpelaionDB
   (ADR-004): `docs/guide` shows how to call the CLI from cron and from Windows' Task
   Scheduler.

8. **A run is a job of its own, in a session of its own.** It opens a new connection from the
   task's resolved connection (never a tab's session), for the run only, closed at its end. It
   is a job like the export, import and backup it runs (ADR-060: one cancel, one ending), with
   their progress. Cancel in the app, or Ctrl+C in the CLI, stops it exactly as the dialog's
   Cancel would: an export or a dump leaves nothing (ADR-034, ADR-049), an import keeps only
   whole batches (ADR-038). The CLI exits with 0 only when the run succeeded.

9. **History and logs.** Each run is recorded in the store (`task_runs`) when it starts and
   again when it ends: where it ran (the app or the CLI), when it started and ended, its ending
   (one state type of the core: running, succeeded, failed, cancelled or refused; a run whose
   ending never came, because its program was stopped, reads as unfinished) and its summary
   (rows and bytes, files written, statements done). Its log is the lines the run produced (the
   resolution, the plan, the tool's output, the error), each through the run's `Redactor`
   (the connection's and its tunnel's secrets, the password typed for the run, the dialogs'
   answers) before it is kept, and bounded (2,000 lines, 256 KiB; the rest is counted, not
   kept). The newest 100 runs of each task are kept; deleting a task deletes its runs. The app
   and the CLI write to the same store (SQLite in WAL mode, ADR-011), so the history shows both
   kinds of run.

10. **Where it lives.**
    - The export, import and backup dialogs have **Save as task…** beside their Run button: it
      asks a name and a folder and saves the dialog's request as it would run, after the core's
      checks. Saving under an existing name asks before replacing that task.
    - A **Tasks** view lists the tasks by folder with their kind, connection and last run:
      Run, Edit (name, folder, connection, the paths, and a query export's SQL; the other
      options are changed by saving again from the dialog), Duplicate, Delete, and the run
      history with each run's log.
    - The CLI: `spelaiondb task list`, `spelaiondb task run NAME [--yes]` and
      `spelaiondb task history NAME [--log]`. Creating and editing tasks stays in the app.

## Consequences
- A task is the user's own: a teammate does not see it, and it does not follow the user to
  another machine. Settings portability (SDB-31) and sharing are later decisions.
- A task that needs a password typed every time runs only from the app; from a scheduler the
  password has to be in the keychain, an environment variable or a secret manager.
- Two runs of the same task at once (the app and a scheduled CLI) are not prevented: exports
  and dumps never overwrite a file, but two imports load the rows twice. The history shows
  both.
- A file the task reads (an import's source, a restore's archive, a `.sql` file) is read when
  the task runs, as it is then; its columns may have changed, and the run then fails with the
  import's own sentence.

## Alternatives
- **Files in the project (`.spelaion/tasks/`)**, shared with the team: a repository's task
  would be a write chosen by someone else (an import, a restore, a `.sql` file) that the user
  runs with a click or from cron. It would need trust pinned to its content like connections
  (ADR-022), and a personal connection has no repository. The document of point 2 is
  self-contained, so files can come later without changing what is kept here.
- **A schedule inside the app**: runs only while the app is open, surprises the user when it
  starts, and is half a daemon; ADR-004 rejected the whole one. The user's own scheduler and
  the CLI already do it, visibly.
- **Storing the confirmation in the task** ("never ask again"): a restore or a table replaced
  would then run with nobody having agreed to that run; `--yes` is in the user's own command
  instead, and the app asks every time.
- **Asking for passwords on the terminal in the CLI**: useful at a keyboard, useless from a
  scheduler, and a new dependency; the keychain already serves both.
- **Resolving a personal connection with whatever folders are open at run time**: the same
  task would then resolve differently from the app and from the CLI, which opens none.

## Revisit if
- Teams ask to share tasks: a task then is exported as a file and read from a trusted
  repository's `.spelaion/tasks/`, pinned by trust, with this local copy as the user's own.
- People run tasks at a terminal often enough to want the CLI to ask for a password there.
- Two runs of a task at once turn out to hurt: a run would then take a lock on its task.
