# DataGrip fixtures

Files a real DataGrip wrote, read by `tests/datagrip.rs` and by the smoke test's DataGrip
import step. Nothing here is a real connection: every address is the repository's Docker test
servers (`testing/docker`) or a made-up file.

## How they were made
- DataGrip 2026.3 (build DB-263.7307.216) on Windows 11, freshly installed, default settings.
  Its password storage was left at its Windows default, **In KeePass** (2026.3 offers no
  native keychain on Windows), so `options/security.xml` was never written.
- In the default project (`~/DataGripProjects/default`), seven data sources were created by
  hand through DataGrip's own "Data Source" dialogs:

  | Name | Engine | Address / file | Extras |
  |---|---|---|---|
  | Orders PG | PostgreSQL | 127.0.0.1:15432, db `spelaion`, user `spelaion` | made-up password saved "Forever" (in DataGrip's KeePass, not in these files); SSL on, mode Require, CA / client certificate / client key files |
  | Shop MySQL | MySQL | 127.0.0.1:13306, db `spelaion`, user `spelaion` | |
  | Shop MariaDB | MariaDB | 127.0.0.1:13307, db `spelaion`, user `root` | |
  | Reports MSSQL | SQL Server | 127.0.0.1:11433, db `master`, user `sa` | |
  | Local file | SQLite | `C:\data\local.db` | |
  | Analytics | DuckDB | `C:\data\sales.duckdb` | |
  | PG via bastion | PostgreSQL | `postgres:5432`, db `spelaion`, user `spelaion` | SSH tunnel through a new SSH configuration: 127.0.0.1:2221, user `spelaion`, key pair `testing\docker\keys\id_ed25519` |

- DataGrip was closed with File → Exit, and these files were copied as written:
  - `DataGripProjects/default/.idea/dataSources.xml` and `dataSources.local.xml`, from the
    project;
  - `JetBrains/DataGrip2026.3/options/sshConfigs.xml` and `recentProjects.xml`, from
    `%APPDATA%\JetBrains\DataGrip2026.3\options`.

## What was edited by hand
Only the user's folder in paths: `C:\Users\<name>\Documents\spelunkdb\` became
`C:\Users\tester\spelaiondb\` in the three certificate paths of `dataSources.local.xml` and in
the key path of `sshConfigs.xml`. Nothing else was changed: the line endings (CRLF in the
project's files) are normalised to LF by the repository's `.gitattributes`. `recentProjects.xml`
names the project as `$USER_HOME$/DataGripProjects/default`, as DataGrip wrote it.
