# pgAdmin fixtures

What a real pgAdmin 4 wrote, read by `tests/pgadmin.rs` (and the export by the smoke test's
pgAdmin import step). Nothing here is a real connection: every address is the repository's
Docker test servers (`testing/docker`) or the bastion's service name, and no password was ever
typed into pgAdmin.

- `pgadmin-9.19-servers.json`: the file pgAdmin's **Tools → Import/Export Servers → Export**
  wrote.
- `pgadmin4-9.19.sql`: the `servergroup` and `server` tables of pgAdmin's own database
  (`%APPDATA%\pgAdmin\pgadmin4.db`), as SQL: the table definitions exactly as pgAdmin created
  them (from `sqlite_master`) and every row as it wrote it. The test loads it into a new
  `pgadmin4.db`.

## How it was made
- pgAdmin 4 9.19 for Windows (the desktop app), from the official winget source (`winget install
  --id PostgreSQL.pgAdmin -e`), first launch, default settings. pgAdmin asked for no master
  password: it only does when a password is saved, and none was.
- Through pgAdmin's own dialogs (Object Explorer → right click → Create → Server Group…, and
  Register → Server…), with **Connect now?** turned off and **Save**, never connecting:

  | Group | Server | Settings |
  |---|---|---|
  | Production (new group) | Orders TLS | 127.0.0.1:15432, maintenance database `spelaion`, user `spelaion`, role `reporting`, comment "Orders of the EU shop"; Parameters: the default SSL mode row deleted and added again as **verify-ca**, plus Root certificate, Client certificate and Client certificate key from `testing\docker\keys`; Tags: `eu` (pgAdmin's default green) |
  | Production | PG via bastion | host `postgres`, port 5432, maintenance database `spelaion`, user `spelaion`; SSH Tunnel: 127.0.0.1:2221, user `spelaion`, **Identity file** `testing\docker\keys\id_ed25519` (no passphrase, nothing to prompt), keep alive 30 |
  | Servers (pgAdmin's default group) | Local PG | 127.0.0.1:15432, maintenance database `spelaion`, user `spelaion`; nothing else |
  | Servers | Analytics service | no host, Service `analytics` (a pg_service.conf entry): SpelaionDB cannot open it, so it is listed as not importable |

  Every password field was left empty and "Save password?" off.
- Tools → Import/Export Servers → Export, both groups ticked, to a file outside the repository;
  that file is `pgadmin-9.19-servers.json`. Then `pgadmin4.db` was copied and its two tables
  dumped with Python's `sqlite3` module (opened read-only); the dump checked that both password
  columns were empty in every row.

## What was edited by hand
- The user's folder in paths: `C:\Users\<name>\Documents\spelunkdb\` became
  `C:\Users\tester\spelaiondb\` in the three certificate paths and the SSH key path, in both
  files.
- Nothing else. The SQL file adds three comment lines at the top; pgAdmin's other tables
  (`user`, `keys`, preferences, query history…) are not copied: they hold nothing about servers,
  and `keys` holds pgAdmin's own encryption key. The test loads the dump with foreign keys off,
  since the `server` table points to the `user` table left out.

## Not covered
- pgAdmin on macOS and Linux. They keep `pgadmin4.db` in `~/.pgadmin` (from pgAdmin's
  `config.py`), and their export is the same JSON, as far as is known, but no file they wrote was
  available.
- pgAdmin in server mode (the web application): its export is the same JSON, but paths may start
  with `<STORAGE_DIR>` (the preview says to fix them), and its database holds every user's
  servers, which the reader would all list. Only the desktop app was tried.
- Saved passwords: pgAdmin encrypts them (with the master password, or a key of its own) and
  SpelaionDB never reads them; a test sets the columns by hand to check they are never selected.
  Kerberos, password exec commands, shared servers, colours and the other settings the reader
  lists were not saved in pgAdmin either: tests write them by hand.
