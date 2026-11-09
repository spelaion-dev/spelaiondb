# TablePlus fixtures

Files a real TablePlus wrote, read by `tests/tableplus.rs` and by the smoke test's TablePlus
import step. Nothing here is a real connection: every address is the repository's Docker test
servers (`testing/docker`) or a made-up file, and every password is made up.

## How they were made
- TablePlus 27.2.0 (build 412) for Windows, installed with
  `winget install --id TablePlus.TablePlus -e`, used in its free trial, default settings.
- Seven connections were created by hand through TablePlus's own connection forms and saved
  with **Save** (never Test or Connect), with every password left empty:

  | Name | Engine | Address / file | Extras |
  |---|---|---|---|
  | Orders PG | PostgreSQL | 127.0.0.1:15432, db `spelaion`, user `spelaion` | tag **production**, red; SSL mode VERIFY-CA with key / certificate / CA files from `testing\docker\keys`; bootstrap command `SET search_path TO app, public;`; in a group **Prod** |
  | Shop MySQL | MySQL | 127.0.0.1:13306, db `spelaion`, user `spelaion` | tag development; SSL mode REQUIRED |
  | Shop MariaDB | MariaDB | 127.0.0.1:13307, db `spelaion`, user `root` | tag staging; SSL mode VERIFY-SERVER-CERT |
  | Reports MSSQL | SQL Server | 127.0.0.1:11433, db `master`, user `sa` | tag testing |
  | Local file | SQLite | `C:\data\local.db` | tag local |
  | Analytics | DuckDB | `C:\data\sales.duckdb` | tag local; Read Only ticked |
  | PG via bastion | PostgreSQL | `postgres:5432`, db `spelaion`, user `spelaion` | tag local; Over SSH: 127.0.0.1:2221, user `spelaion`, Use SSH Key with `testing\docker\keys\id_ed25519` (no passphrase) |

- Then a person (not the agent, which may not type passwords into another application) typed
  the made-up password `fake-password-123` into Orders PG and saved it, and exported every
  connection (Export Connections → Export all…, both "Include … password" boxes ticked) with the
  made-up export password `fixture-export-pw`. TablePlus requires an export password.

## The files
- `com.tinyapp.TablePlus/Data/Connections.plist` and `ConnectionGroups.plist`: TablePlus's own
  storage, copied from `%LOCALAPPDATA%\com.tinyapp.TablePlus\Data` before the password was
  typed (typing it only changed TablePlus's encrypted `.tpmaster` store, not these files, which
  were the same afterwards). The `.tpmaster` store, where TablePlus keeps passwords and SSH keys,
  was not copied: SpelaionDB does not read it.
- `export.json`: the export, decrypted (TablePlus encrypts it in RNCryptor's v3 format; see
  `tableplus.rs`), as TablePlus wrote it: one line of JSON.
- `export.tableplusconnection`: `export.json` encrypted again in the same format with the same
  password (`fixture-export-pw`), by a script that follows RNCryptor's v3 specification (random
  salts and IV). The file TablePlus wrote could not be kept as it was, because of the edits
  below; SpelaionDB's reader was checked against that original file too (all seven connections,
  Orders PG's password) before it was replaced.

## What was edited by hand
- The user's folder in paths: `C:\Users\<name>\Documents\spelunkdb\` became
  `C:\Users\tester\spelaiondb\` in Orders PG's three certificate paths, in `Connections.plist`
  and in the export.
- In the export, **PG via bastion's `PrivateKeyData`**: TablePlus put the whole private key in
  the export (even though its export dialog says SSH/SSL private keys are not included). It was
  the Docker bastion's test key, but a private key does not belong in the repository, so its
  contents were replaced by `removed from the fixture` between the key's `BEGIN` / `END` lines.
- Nothing else: the property lists' line endings (CRLF as TablePlus wrote them) are normalised to
  LF by the repository's `.gitattributes`.

## Not covered
- TablePlus for macOS. It keeps its connections in the same `Connections.plist` under
  `~/Library/Application Support/com.tinyapp.TablePlus/Data`, as far as is known, but no file a
  Mac wrote was available; the reader also accepts a binary property list, untested against a
  real one. Its passwords are in the macOS keychain, which is not read.
- TablePlus's custom tags, SSH password authentication, safe mode and other settings these
  connections do not use: the reader lists what it does not import, but those keys were only
  seen empty here.
