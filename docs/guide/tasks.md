# Saved tasks

The same export, import or backup often runs every week with the same options. Save it once as
a task, then run it again with one click from the **Tasks** view, or from the command line,
where your own scheduler can call it. SpelaionDB never runs a task on its own: there is no
schedule inside the app and no background service (ADR-089).

## Saving a task

These dialogs have **Save as task…** beside their main button:

- **Export** (Export… in the results, a table data tab, or tables in the explorer): only an
  export that runs the query again, or reads the table again, can be saved. The rows loaded in
  the grid and a selection belong to that result only. A task names the folder its files go to;
  each run writes a new file named by the dialog's file name pattern (`{table}`, `{date}`…) and
  never writes over one: the second is `orders (2).csv`.
- **Import data…**, from a file (pasted text cannot be read again). The file is read when the
  task runs, as it is then.
- **Back up…** and **Restore…**, with the database's own tools or SQLite's and DuckDB's
  commands.
- **Run SQL file…**: the task keeps the file's path, not its text.

Give it a name and, if you like, a folder (`Backups/Weekly`) that groups tasks in the view.
Names are unique: saving under a name another task has asks before replacing that task.

A task keeps options, never a password, a passphrase or a token. Saving is refused if any of
its text (a query, a filter, a path, a tool's extra arguments) holds one of the connection's
secrets, or a tool argument such as `--password=…`.

## The Tasks view

The Tasks button in the activity bar opens the view: the tasks by folder, and for the one
selected its kind, connection, the file or folder it writes or reads, its query, and its runs.

- **Run** runs it now, in a session of its own. A task asks first exactly where the same run by
  hand would: an import that replaces a table, any run on a connection whose type confirms
  execution (Production, for example), a restore, a DuckDB import and a file run always. The
  question shows what will run. A connection that asks for its password asks for it, as
  connecting does.
- **Edit…** changes the name, the folder, the connection, the path and a query export's query.
  To change other options, open the dialog again, choose them and save under the same name.
- **Duplicate** copies a task under the name "… (copy)". **Delete…** deletes it and its runs;
  the files its runs wrote stay.
- **Runs** lists the newest 100 runs, from the app and from the command line, each with how it
  ended and its log (passwords and other secrets are masked in it).

## From the command line

```
spelaiondb task list
spelaiondb task run "Weekly orders"
spelaiondb task run "Restore staging" --yes
spelaiondb task history "Weekly orders" --log
```

`task run` runs the task the way Run does and exits with 0 only when it succeeded. It never
asks anything:

- A task that would ask first runs only with `--yes`, the confirmation you write into the
  command. Without it the run is refused, and the history says so.
- A connection whose password is asked every time (`prompt`) cannot run there: keep the
  password in the keychain, an environment variable (`env:`) or 1Password (`op://`). An SSH
  host key it has not seen fails too: connect once from the app to accept it.
- Ctrl+C cancels the run as Cancel would: an export or a dump leaves no file behind.

A repository connection runs only while the repository is trusted as it is. A personal
connection is resolved with the repository folders that were open when the task was saved, so
it runs the same from the app and from the command line.

## Running tasks on a schedule

Use your system's scheduler to call the command line. On Linux and macOS, with cron (every
Monday at 07:00):

```
0 7 * * 1  spelaiondb task run "Weekly orders" >> ~/weekly-orders.log 2>&1
```

On Windows, with the Task Scheduler (PowerShell, once):

```
$action = New-ScheduledTaskAction -Execute "spelaiondb.exe" -Argument 'task run "Weekly orders"'
$trigger = New-ScheduledTaskTrigger -Weekly -DaysOfWeek Monday -At 7am
Register-ScheduledTask -TaskName "SpelaionDB weekly orders" -Action $action -Trigger $trigger
```

The scheduler runs as you, with your keychain and your personal folder, so the run finds the
same tasks and secrets. Its runs appear in the task's history in the app.
