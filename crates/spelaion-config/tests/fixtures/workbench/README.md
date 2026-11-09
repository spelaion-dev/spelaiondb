# MySQL Workbench fixtures

The `connections.xml` a real MySQL Workbench wrote, read by `tests/workbench.rs` and by the
smoke test's MySQL Workbench import step. Nothing here is a real connection: every address is
the repository's Docker test servers (`testing/docker`), the bastion's service name or a named
pipe, and the one password a person stored in Workbench is made up (and is not in this file).

## How it was made
- MySQL Workbench 8.0.47 Community for Windows, from the official winget source (`winget
  download --id Oracle.MySQLWorkbench -e --version 8.0.47`, then its MSI), default settings.
  (`winget install --id Oracle.MySQLWorkbench` now installs Workbench 26, a different
  application that keeps connections in a database of its own; this reader does not cover it.)
- Four connections were created through Workbench's own "Setup New Connection" dialog and saved
  with **OK** (never Test Connection, and never opened), every password left unset:

  | Name in Workbench | Method | Address | Extras |
  |---|---|---|---|
  | Shop MySQL | Standard (TCP/IP) | 127.0.0.1:13306, user `spelaion`, default schema `spelaion` | none: Use SSL left at "If available" |
  | Prod/Shop MariaDB TLS | Standard (TCP/IP) | 127.0.0.1:13307, user `root`, default schema `spelaion` | "Yes" to Workbench's offer to put it in the group **Prod**; Use SSL **Require and Verify CA** with key / certificate / CA files from `testing\docker\keys`; Advanced: timeout 20, SQL_MODE `ANSI_QUOTES`, Others `OPT_LOCAL_INFILE=1` |
  | Prod/Orders via bastion | Standard TCP/IP over SSH | SSH 127.0.0.1:2221, user `spelaion`, key file `testing\docker\keys\id_ed25519`; MySQL `mysql:3306`, user `spelaion`, default schema `spelaion` | in the group **Prod** |
  | Local pipe | Local Socket/Pipe | pipe `MySQL`, user `root` | SpelaionDB cannot open it: listed as not importable |

- Workbench writes `connections.xml` when it closes (File → Exit). Then a person (not the agent,
  which may not type passwords into another application) opened Workbench again and stored the
  made-up password `fake-password-123` for Shop MySQL with "Store in Vault…", and closed it. That
  changed nothing in the connections' settings: Workbench only added an `owner` link to each,
  marked one `isDefault` and wrote new `_ptr_` addresses. This second file, copied from
  `%APPDATA%\MySQL\Workbench`, is the fixture.
- The password went to Workbench's vault, `workbench_user_data.dat`. In a local check that is
  not part of the repository, that file was decrypted with the Windows account's DPAPI key: it
  holds one entry, keyed by the connection's `hostIdentifier` (`Mysql@127.0.0.1:13306`) and its
  user (`spelaion`), the same service and account Workbench uses in the macOS keychain and the
  reader asks the keychain for.

## What was edited by hand
- The user's folder in paths: `C:\Users\<name>\Documents\spelunkdb\` became
  `C:\Users\tester\spelaiondb\` in the three certificate paths and the SSH key path.
- Nothing else: Workbench's object ids and `_ptr_` attributes (memory addresses it writes) are
  as it wrote them.

## Not copied
- `server_instances.xml`: Workbench's server administration settings (remote management over
  SSH), not part of a connection.
- `workbench_user_data.dat`: Workbench's password vault on Windows, encrypted with the Windows
  account's key (DPAPI). SpelaionDB does not read it, and it would only open on the machine and
  account that wrote it.

## Not covered
- Workbench for macOS and Linux. They write the same `connections.xml` (in
  `~/Library/Application Support/MySQL/Workbench` and `~/.mysql/workbench`), as far as is known,
  but no file they wrote was available. On macOS the passwords are read from the keychain under
  the connection's `hostIdentifier` and user, which is what Workbench uses as service and
  account on every system; that was checked only with a fake keychain in the tests.
- Workbench's LDAP, Kerberos and other connection methods, and its SSH password
  authentication: the reader lists what it does not import, but none of those was saved here.
