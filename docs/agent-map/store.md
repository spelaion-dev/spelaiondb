# Store

- `crates/spelaion-store`: local SQLite (audit, query history, saved queries, personal settings,
  the user's editor snippets and the template of new scripts in `editor_snippets.rs`, kept as
  personal settings and checked by `spelaion-core`'s `editor_snippets.rs` when saved and when
  read: bounded, plain text by `spelaion-config::check_plain_text`, the rule of a connection's
  description; they are text the editor inserts, never run),
  the export dialog's last choices in `export.rs` (SDB-66: each value checked against its
  known values and bounds when saved and when read, an unusable one read as its default),
  the explorer's filters and bookmarks, the editor's open tabs per workspace and recent files in
  `tabs.rs`, each table's filter history and default filter in `table_filters.rs`, saved ER
  diagrams in `er_diagrams.rs` (ADR-084: a name and a document per workspace and connection,
  never interpreted here; `spelaion-core`'s `er/saved.rs` writes and checks the document; a
  moved connection keeps them), saved tasks and their runs in `tasks.rs` (ADR-089: a name unique
  ignoring case, a folder label and a document `spelaion-core`'s `task/` writes and checks; each
  run a row written when it starts and again when it ends, from the app or the CLI, the newest
  100 per task kept, its log cut at a line to 256 KiB; a question is not a run:
  `forget_task_run`); later the schema cache). A new migration is appended to `MIGRATIONS` and its test finds it by its text,
  never by "the last one". `spelaion-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
  Which personal settings move to another computer with the settings file (SDB-31) is
  `portable.rs`: exact keys and key families that are preferences of the app, and the keys that
  name this computer's folders and programs (`recent_folders`, `backup.folder`, the backup tool
  folders), which never move. A new personal setting must be put on one side or the other:
  `every_key_the_store_writes_is_portable_or_of_this_computer` fails until it is.
  `replace_portable_settings` replaces the portable rows whole in one transaction (the triggers
  still refuse what they do not know) and never turns the MCP read-only mode off.
