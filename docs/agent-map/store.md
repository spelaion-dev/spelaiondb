# Store

- `crates/spelaion-store`: local SQLite (audit, query history, saved queries, personal settings,
  the user's editor snippets and the template of new scripts in `editor_snippets.rs`, kept as
  personal settings and checked by `spelaion-core`'s `editor_snippets.rs` when saved and when
  read: bounded, plain text by `spelaion-config::check_plain_text`, the rule of a connection's
  description; they are text the editor inserts, never run),
  the export dialog's last choices in `export.rs` (SDB-66: each value checked against its
  known values and bounds when saved and when read, an unusable one read as its default),
  the explorer's filters and bookmarks, the editor's open tabs per workspace and recent files in
  `tabs.rs`, each table's filter history and default filter in `table_filters.rs`; later the
  schema cache). A new migration is appended to `MIGRATIONS` and its test finds it by its text,
  never by "the last one". `spelaion-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
