//! What a backup or a restore asks of each tool: the dialog's choices, as the core receives them.

use std::path::PathBuf;

/// `pg_dump`'s output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgFormat {
    /// A SQL script (`psql` restores it).
    Plain,
    /// `pg_restore`'s compressed archive.
    Custom,
    /// A folder with one file per table.
    Directory,
    Tar,
}

impl PgFormat {
    pub(crate) fn letter(self) -> &'static str {
        match self {
            PgFormat::Plain => "p",
            PgFormat::Custom => "c",
            PgFormat::Directory => "d",
            PgFormat::Tar => "t",
        }
    }
}

/// What a dump holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DumpContent {
    #[default]
    SchemaAndData,
    SchemaOnly,
    DataOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgDumpOptions {
    /// Only these schemas (names, not patterns); empty: all of them.
    pub schemas: Vec<String>,
    /// Only these tables, as `(schema, table)`; empty: all of them.
    pub tables: Vec<(String, String)>,
    pub format: PgFormat,
    /// 0–9, for the custom and directory formats (plain: a gzip file); `None`: the tool's default.
    pub compression: Option<u8>,
    /// The dump's encoding; `None`: the database's.
    pub encoding: Option<String>,
    pub content: DumpContent,
    /// `INSERT` statements instead of `COPY`.
    pub inserts: bool,
    pub no_privileges: bool,
    pub no_owner: bool,
    /// `DROP` before each `CREATE` (`--clean --if-exists`).
    pub clean: bool,
    /// `CREATE DATABASE` and a connection to it first.
    pub create: bool,
}

impl Default for PgDumpOptions {
    fn default() -> Self {
        Self {
            schemas: Vec::new(),
            tables: Vec::new(),
            format: PgFormat::Custom,
            compression: None,
            encoding: None,
            content: DumpContent::SchemaAndData,
            inserts: false,
            no_privileges: false,
            no_owner: false,
            clean: false,
            create: false,
        }
    }
}

/// How `mysqldump` keeps the dump consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MysqlConsistency {
    /// One `REPEATABLE READ` transaction (InnoDB): nothing is locked.
    #[default]
    SingleTransaction,
    /// A global read lock for the whole dump.
    LockAllTables,
    /// No transaction and no locks.
    NoLocks,
}

/// `mysqldump`'s column statistics query, which MySQL 8's client runs and older servers and
/// MariaDB do not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColumnStatistics {
    /// Off against MariaDB, the tool's default otherwise.
    #[default]
    Auto,
    Off,
    On,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MysqlDumpOptions {
    /// The databases; empty: the connection's.
    pub databases: Vec<String>,
    /// Only these tables of the one database chosen; empty: all of them.
    pub tables: Vec<String>,
    pub consistency: MysqlConsistency,
    /// `DROP TABLE` before each `CREATE TABLE` (the tool's default).
    pub add_drop: bool,
    /// No `CREATE TABLE` (nor `CREATE DATABASE`).
    pub no_create: bool,
    pub no_data: bool,
    /// Many rows per `INSERT` (the tool's default).
    pub extended_insert: bool,
    pub routines: bool,
    pub events: bool,
    pub triggers: bool,
    /// Views are dumped with the tables (the tool's default); off: each one is skipped.
    pub include_views: bool,
    /// `DEFINER=…` removed from the file, so it restores under any account.
    pub strip_definer: bool,
    /// Binary columns as hexadecimal literals.
    pub hex_blob: bool,
    pub column_statistics: ColumnStatistics,
}

impl Default for MysqlDumpOptions {
    fn default() -> Self {
        Self {
            databases: Vec::new(),
            tables: Vec::new(),
            consistency: MysqlConsistency::SingleTransaction,
            add_drop: true,
            no_create: false,
            no_data: false,
            extended_insert: true,
            routines: true,
            events: false,
            triggers: true,
            include_views: true,
            strip_definer: false,
            hex_blob: false,
            column_statistics: ColumnStatistics::Auto,
        }
    }
}

/// A dump of one of the engines with native tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpOptions {
    Postgres(PgDumpOptions),
    Mysql(MysqlDumpOptions),
}

/// What a restore's input file is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreFormat {
    /// Read from the file: a custom or tar archive, a dump folder, or SQL.
    Auto,
    /// A SQL script, run by `psql` or `mysql`.
    Plain,
    Custom,
    Directory,
    Tar,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PgRestoreOptions {
    /// Drop each object before creating it (`--clean --if-exists`).
    pub clean: bool,
    pub no_owner: bool,
    /// Create the archive's database first, connecting to the connection's database to do it.
    pub create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MysqlRestoreOptions {
    /// The database the file runs in; `None`: the connection's (a file made with `--databases`
    /// chooses its own).
    pub database: Option<String>,
    /// `SET FOREIGN_KEY_CHECKS = 0` for the load.
    pub disable_foreign_keys: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreOptions {
    Postgres(PgRestoreOptions),
    Mysql(MysqlRestoreOptions),
}

/// A backup to a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpRequest {
    pub tools: super::ToolSource,
    pub folder: PathBuf,
    /// The file name pattern: `{database}`, `{connection}`, `{timestamp}`, `{date}`. The
    /// extension is the format's.
    pub name_pattern: String,
    /// More arguments for the tool, as typed (split by the core, never by a shell).
    pub extra_args: String,
    pub options: DumpOptions,
}

/// A restore from a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreRequest {
    pub tools: super::ToolSource,
    pub input: PathBuf,
    pub format: RestoreFormat,
    pub extra_args: String,
    pub options: RestoreOptions,
}
