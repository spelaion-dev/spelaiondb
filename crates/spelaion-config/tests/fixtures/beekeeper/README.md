# Beekeeper Studio fixtures

What a real Beekeeper Studio wrote, read by `tests/beekeeper.rs` (and by the smoke test's
Beekeeper Studio import step). Nothing here is a real connection: every address is the
repository's Docker test servers (`testing/docker`), the bastion's service name or a made-up
host, and no password was ever typed into Beekeeper.

- `beekeeper-6.2.2.sql`: the `connection_folder` and `saved_connection` tables of Beekeeper's
  own database (`%APPDATA%\beekeeper-studio\app.db`), as SQL: the table definitions exactly as
  Beekeeper created them (from `sqlite_master`) and every row as it wrote it. The tests load it
  into a new `app.db`.
- `.key`: an encryption key file in Beekeeper's format (see below). It is **not** the key of the
  install the tables come from.
- `passwords.json`: fake passwords (`fake-password-123`, `fake-passphrase`,
  `fake-ssh-password`) encrypted with that `.key`, which the tests write into the
  `password`, `sshKeyfilePassword` and `sshPassword` columns.

## How it was made
- Beekeeper Studio 6.2.2 for Windows, Community edition (no account, no sign-in), from the
  official winget source (`winget install --id beekeeper-studio.beekeeper-studio -e`), first
  launch, default settings. Beekeeper adds its own "Demo Database" (a SQLite file it ships) on
  first launch.
- Through Beekeeper's own New Connection form, filled in and **Save**d, never connected
  (connecting the SSH one would have written the test bastion's host key into the user's
  `known_hosts`). Every password field was left empty ("Save Passwords" left ticked, its
  default).

  | Name | Type | Settings |
  |---|---|---|
  | Orders TLS | Postgres | 127.0.0.1:15432, user `spelaion`, database `spelaion`; Enable SSL with CA Cert, Certificate and Key File from `testing\docker\keys`, Reject Unauthorized (default, ticked); green label |
  | PG via bastion | Postgres | host `postgres`, port 5432, user and database `spelaion`; SSH Tunnel: 127.0.0.1:2221, Key File auth, user `spelaion`, private key `testing\docker\keys\id_ed25519` (no passphrase), keepalive 30 |
  | Shop MySQL | MySQL | 127.0.0.1:13306, user and database `spelaion`; Enable SSL without files; Read Only Mode; red label |
  | Shop MariaDB | MariaDB | 127.0.0.1:13307, user `root`, database `spelaion` |
  | Reports MSSQL | SQL Server | 127.0.0.1:11433, user `sa`, database `master`; Trust Server Certificate (default, ticked) |
  | Shop file | SQLite | `examples\demo\data\shop.db` |
  | Warehouse | Amazon Redshift | `dw.example.internal`, user `analyst`, database `warehouse` (an engine SpelaionDB cannot open) |

  Then a folder (the sidebar's new folder button) named `Production`, and "Orders TLS" and
  "PG via bastion" dragged into it. DuckDB was not saved: the Community edition asks for the
  paid edition before its form opens; a test writes a DuckDB row by hand.
- Beekeeper was closed (File → Exit), `app.db` copied, and the two tables dumped with Python's
  `sqlite3` module (the copy opened read-only); every password column was empty in every row.

### The key and the passwords
Beekeeper encrypts saved passwords with its `simple-encryptor` library (HMAC-SHA256, then
AES-256-CBC, both under SHA-256 of a key) using a random key it generates per install and keeps
in `.key` next to `app.db`, itself encrypted with a key built into Beekeeper
(`src/common/encryption_key.ts` in its open source repository). SpelaionDB reads them the same
way Beekeeper does.

The `.key` of the install above was not copied: publishing it would weaken that install's own
encryption. Instead, `.key` and `passwords.json` were made with Beekeeper's own code: the
`simple-encryptor` module (3.0.0) taken from that install's `app.asar`, run under Node with the
same steps as `encryption_key.ts` (a random 32-byte hex key, encrypted with the built-in key)
and its `EncryptTransformer` (`encrypt(password)`). The real install's `.key` was checked to
decrypt with an independent implementation of the format before that.

## What was edited by hand
- The user's folder in paths: `C:\Users\<name>\Documents\spelunkdb\` became
  `C:\Users\tester\spelaiondb\` (the three certificate paths, the SSH key path and the SQLite
  file), and `C:\Users\<name>\AppData\` became `C:\Users\tester\AppData\` (Beekeeper's demo
  database).
- Nothing else. The SQL file adds three comment lines at the top. Beekeeper's other tables
  (settings, tabs, query history, its migrations…) are not copied: they hold nothing about
  connections.

## Not covered
- Beekeeper on macOS and Linux. They keep `app.db` and `.key` in `~/Library/Application
  Support/beekeeper-studio` and `~/.config/beekeeper-studio` (Electron's `userData`), as far as
  is known; no file they wrote was available.
- The portable build keeps its data in `beekeeper_studio_data` next to the executable: the folder
  has to be picked by hand.
- The paid edition: DuckDB connections and cloud workspaces (connections stored on Beekeeper's
  servers, not in `app.db`, are not imported; a local row with a workspace id is listed).
- Saved passwords typed into Beekeeper itself: the ones tested were encrypted with Beekeeper's
  own module instead (above). Jump hosts, socket and Windows-authentication connections, nested
  folders, domains and the other settings the reader lists were not saved in Beekeeper either:
  tests write them by hand.
