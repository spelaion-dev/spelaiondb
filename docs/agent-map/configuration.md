# Configuration

- `crates/spelaion-config`: TOML files from `.spelaion/` and the personal directory,
  variables, secret references and repository trust (ADR-009; `trust.rs` pins each repository
  connection and tunnel as trusted, and the resolver refuses one that changed, ADR-022: any
  change there needs tests that try to get around it). Every change to the trust store goes
  through `TrustStore::update`, one at a time, so none saves over another it did not see
  (`tests/trust_updates.rs`); `TrustStore::load` is for reading. If you change `model.rs`,
  regenerate the schema: `SPELAION_UPDATE_SCHEMA=1 cargo test -p spelaion-config`.
  A connection's identity is `identity.rs`'s `ConnectionKey` (ADR-075, SDB-495): its folder
  (the canonical root, none for a personal connection; `new` refuses a mismatch) and its
  `ConnectionId`, whose text form is `ConnectionId::parse` / `Display` and never holds a folder.
  The store's keys (`store_scope`, `folder_scope`, `repository_scope`) and the connection's
  keychain name (`keychain_account`, behind `accounts::connection_account`) come from it: never
  build one from `workspace_root` by hand. Never put a database in it (ADR-074).
  `draft.rs` turns the add/edit connection form into a validated file (secrets go to the
  keychain, never to the file); editing touches only the changed keys with `toml_edit`.
  Every password written to the keychain goes through `secrets::store`, which reads it back
  (a store that answers `Ok` and keeps nothing is an error); never call `keychain_set` directly.
  A connection's `description` is the user's free text (the explorer's tooltip, and what
  `list_connections` tells agents a database is for) and its `tags` are short labels the
  explorer filters connections by (`normalize_tags` in `draft.rs`): both bounded and without
  hidden characters (`load.rs`), and like every key part of what a trusted repository pins.
  `definition.rs` (SDB-426) is what a connection's session is opened from, without secrets
  (its file, its network profile, `user.toml`, the connection types, the repository and its
  trust), compared before a session is made from another one; nothing secret is read for it.
  Poolers and bastions (SDB-396): a PostgreSQL connection's `pooler` (`session` or
  `transaction`), MySQL's and MariaDB's `cleartext_password` and the `databases` a MySQL,
  MariaDB or SQL Server connection shows load only on those engines (`load.rs`); a pooler in
  transaction mode refuses `[session]` and `read_only`, which would hold for one transaction.
  The first two reach the driver only as `ConnectParams::options` (`engine_options.rs`), so an
  older plugin that speaks protocol 1.0 refuses the connection instead of ignoring them.
  On PostgreSQL `databases` means something else (ADR-074, SDB-491): the other databases of
  its server the connection opens, its own `database` always included; `all_databases = true`
  (PostgreSQL only) opens every database the server lists. Without either it is its own
  database alone, as before, and neither key is written. `load.rs` refuses both together,
  `all_databases` on another engine, and either behind a `pooler`. The resolved connection
  carries the choice as `OtherDatabases` (`None`, `Named`, `All`; always `None` off
  PostgreSQL), which is what the core reads; `databases` keeps feeding `shown.rs`. On
  PostgreSQL the form's names are case-sensitive, changing the engine keeps `databases` only
  between engines where it means the same, a paste or a copy leaves both keys out (they widen
  what opens), and an import never turns `all_databases` on (DBeaver's "Show all databases"
  is listed as not imported). Trust pins both like every field (`tests/postgres_databases.rs`).
  `tunnel_draft.rs` does the same for SSH tunnels (hops matched by position; a stored hop
  password is forgotten when that hop points to another server).
  Every file the configuration writes goes through `atomic_file` (temporary file + rename, the
  old mode kept, a symbolic link stays a link; new files owner-only in the personal folder and
  the umask's in the repository, `edit::new_file_mode`); never `std::fs::write` over a user's file.
  The core's exports (audit, history, plans, data files) stream through its `AtomicWriter`
  (a uniquely named temporary file, the same guarantees).
  Importing connections from other clients (ADR-039): `import.rs` is the flow every client
  shares (the `ImportedConnection` model, drafts, duplicates found by name or by the database
  they reach, replace in place, target folder, what was not mapped, passwords read from another
  app's keychain only on import); a client is a reader that returns `ImportedConnection`s.
  A client's startup SQL goes into `ImportedConnection::session` (SDB-104, ADR-037; TablePlus's
  bootstrap commands first, a shell script never): the desktop's scan and import pass every
  imported connection through the core's `vet_imported_setup` before it is shown or saved
  (where the read-only barrier applies, what could write is taken out and listed in
  `unmapped`), and the saved file goes through the form's own setup check. DBeaver's bootstrap
  (SDB-99, `dbeaver.rs`'s `session_of`): its queries and "ignore errors", its default schema
  (its default catalog only on MySQL and MariaDB, where it is the database `USE` picks) and its
  JDBC isolation level where `IsolationLevel::available_on` allows it; the rest is listed. The
  import's report marks the connections that brought setup statements.
  On Linux a reader's default search also looks in the client's Flatpak and Snap folders
  (`linux_dirs.rs`; native first, each package path citing the manifest it was checked against);
  HeidiSQL's registry only exists on Windows (`heidisql_has_registry`), elsewhere its import
  starts by choosing an exported file. `import_sources.rs` (SDB-446) decides where each client
  is read from (`ImportPlaces::this_system`: the `SPELAION_<CLIENT>_DIR` overrides the smoke test
  sets, `SPELAION_HEIDISQL_REGISTRY=off`, else the client's own place), reads it
  (`read_import`, which the desktop's scan and import call) and gives the import's list its
  state per client (`import_source`: found with a count, not found, needs a file, encrypted,
  failed). The list reads no saved password (`SavedSecrets::Skip`: DBeaver's credentials,
  HeidiSQL's passwords and Beekeeper's key are not opened; `tests/import_sources.rs`); a new
  reader that reads saved secrets takes `SavedSecrets` too. `tests/reader_properties.rs` (SDB-156) changes each
  client's real files at random (and Beekeeper's database values) and checks that no reader,
  nor `paste_connection`, panics; a new reader gets a property there.
  `dbeaver.rs` reads DBeaver's connections and saved credentials (passwords only as `Secret`,
  to the keychain; its connection types map to the closest built-in type); `datagrip.rs` reads
  DataGrip's (and other JetBrains IDEs') XML data sources and SSH configurations, tested against
  files a real DataGrip wrote (`tests/fixtures/datagrip`); `tableplus.rs` reads TablePlus's own
  `Connections.plist` (never its encrypted `.tpmaster` store) and its exported file, encrypted in
  RNCryptor's format with the password the user typed (the export's passwords only as `Secret`;
  the SSH key it carries is never kept), tested against files a real TablePlus wrote
  (`tests/fixtures/tableplus`).
  `workbench.rs` reads MySQL Workbench 8.0's `connections.xml` (groups from `Group/Name`, SSH
  as a one-hop tunnel; passwords only from the macOS keychain on import, never its Windows DPAPI
  vault: the workspace forbids `unsafe`), tested against a file a real Workbench wrote
  (`tests/fixtures/workbench`).
  `pgadmin.rs` reads pgAdmin 4's exported servers JSON (no passwords in it: the preview says they
  are asked) or its own `pgadmin4.db`, opened read-only and never selecting its encrypted password
  columns; a database row is turned into the export's shape so one mapping serves both. Tested
  against files a real pgAdmin wrote (`tests/fixtures/pgadmin`).
  `beekeeper.rs` reads Beekeeper Studio's `app.db` read-only (connections, nested folders);
  its database and SSH passwords are decrypted the way Beekeeper does, with the install's
  `.key` (opened with the key built into Beekeeper), and only ever held as `Secret`s; other
  saved secrets are listed, never decrypted. Tested against tables a real Beekeeper wrote
  (`tests/fixtures/beekeeper`).
  `heidisql.rs` reads HeidiSQL's exported settings file and, on Windows, its registry
  sessions, with one reader for both: the format is untrusted (bounded inputs, malformed
  settings fail closed) and no password reaches an error or the preview.
  ADR-030: `copy.rs` duplicates a connection next to the original (its keychain password copied
  inside the core, a team copy only from a trusted original) and reads pasted clipboard text as
  untrusted input (a personal connection through the form's checks, password always asked, no
  `${env:…}`, nothing that runs a program); `engine_change.rs` maps a connection to another
  engine (kept, changed, dropped, to fill in); `name_pattern.rs` renders the name of new
  connections from the personal pattern. Any change to `copy.rs` needs tests that try to get
  around it.
  `relocate.rs` moves a connection to another folder (its file, its keychain password, and the
  trust pin as it was, so a move never makes a changed or new repository connection usable);
  any change there needs tests that try to get around it (`tests/moves.rs`).
  `types.rs` resolves a connection's type (ADR-029): `env` names one by id (built-in `dev`,
  `test`, `prod`/`production`, or one in `.spelaion/connection-types.toml`, honoured only as
  trusted, or in the user's `connection-types.toml`); looks come from the user first, and
  safeguards only add up across layers, so nothing can loosen Production. Any change there
  needs tests that try to get around it (`tests/connection_types.rs`); `types_edit.rs` writes
  the user's types.
