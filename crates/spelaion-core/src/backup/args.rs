//! The argument lists of each tool (ADR-049). The password is never one of them: it goes in the
//! tool's environment ([`password_variable`]). Names are separate arguments or quoted for the
//! tool's own parser, never for a shell, because no shell ever runs.

use std::path::{Path, PathBuf};

use spelaion_driver_api::TlsMode;

use super::options::{
    ColumnStatistics, DumpContent, MysqlConsistency, MysqlDumpOptions, MysqlRestoreOptions,
    PgDumpOptions, PgFormat, PgRestoreOptions,
};
use super::tools::{ToolFlavor, ToolVersion};

/// Where and how a tool connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Endpoint {
    /// The address the tool connects to: the tunnel's local end or the connection's host.
    pub connect_host: String,
    pub port: u16,
    /// The connection's own host name, when `connect_host` is the tunnel's local end: the name
    /// the server's certificate carries.
    pub server_name: Option<String>,
    pub user: String,
    pub database: String,
    pub tls: TlsMode,
    pub ca_certificate: Option<PathBuf>,
    pub client_certificate: Option<PathBuf>,
    pub client_key: Option<PathBuf>,
    pub connect_timeout_secs: u64,
}

/// The environment variable a tool reads the password from.
pub(crate) fn password_variable(flavor: ToolFlavor) -> &'static str {
    match flavor {
        ToolFlavor::Postgres => "PGPASSWORD",
        ToolFlavor::Mysql | ToolFlavor::Mariadb => "MYSQL_PWD",
    }
}

/// A libpq connection string value: always quoted, `\` and `'` escaped.
fn conninfo_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        if c == '\\' || c == '\'' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

fn libpq_sslmode(tls: TlsMode) -> &'static str {
    match tls {
        TlsMode::Disable => "disable",
        TlsMode::Prefer => "prefer",
        TlsMode::Require => "require",
        TlsMode::VerifyCa => "verify-ca",
        TlsMode::VerifyFull => "verify-full",
    }
}

/// The libpq connection string, without a password. Through a tunnel `host` keeps the real name
/// (what `verify-full` checks) and `hostaddr` is the local end; a local end that is not an IP
/// address (a container's `host.docker.internal`) is the host itself.
pub(crate) fn pg_conninfo(endpoint: &Endpoint, database: &str) -> String {
    let mut pairs: Vec<(&str, String)> = Vec::new();
    let is_ip = endpoint.connect_host.parse::<std::net::IpAddr>().is_ok();
    match &endpoint.server_name {
        Some(name) if is_ip => {
            pairs.push(("host", name.clone()));
            pairs.push(("hostaddr", endpoint.connect_host.clone()));
        }
        Some(_) | None => pairs.push(("host", endpoint.connect_host.clone())),
    }
    pairs.push(("port", endpoint.port.to_string()));
    if !endpoint.user.is_empty() {
        pairs.push(("user", endpoint.user.clone()));
    }
    if !database.is_empty() {
        pairs.push(("dbname", database.to_string()));
    }
    pairs.push(("sslmode", libpq_sslmode(endpoint.tls).to_string()));
    if let Some(ca) = &endpoint.ca_certificate {
        pairs.push(("sslrootcert", ca.display().to_string()));
    }
    if let (Some(cert), Some(key)) = (&endpoint.client_certificate, &endpoint.client_key) {
        pairs.push(("sslcert", cert.display().to_string()));
        pairs.push(("sslkey", key.display().to_string()));
    }
    pairs.push(("connect_timeout", endpoint.connect_timeout_secs.to_string()));
    pairs.push(("application_name", "SpelaionDB".to_string()));
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", conninfo_value(v)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A name `pg_dump`'s `--schema` and `--table` take literally: double-quoted, so neither case
/// nor `*`, `?` or `.` mean anything.
fn pg_literal_pattern(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `pg_dump`'s arguments. Its output is its standard output, except the directory format,
/// written to `directory`.
pub(crate) fn pg_dump_args(
    endpoint: &Endpoint,
    options: &PgDumpOptions,
    directory: Option<&Path>,
    extra: &[String],
) -> Vec<String> {
    let mut args = vec![
        format!("--dbname={}", pg_conninfo(endpoint, &endpoint.database)),
        "--verbose".to_string(),
        format!("--format={}", options.format.letter()),
    ];
    if let (PgFormat::Directory, Some(dir)) = (options.format, directory) {
        args.push(format!("--file={}", dir.display()));
    }
    if let Some(level) = options.compression
        && options.format != PgFormat::Tar
    {
        args.push(format!("--compress={}", level.min(9)));
    }
    if let Some(encoding) = options.encoding.as_deref().filter(|e| !e.trim().is_empty()) {
        args.push(format!("--encoding={}", encoding.trim()));
    }
    match options.content {
        DumpContent::SchemaAndData => {}
        DumpContent::SchemaOnly => args.push("--schema-only".into()),
        DumpContent::DataOnly => args.push("--data-only".into()),
    }
    if options.inserts {
        args.push("--inserts".into());
    }
    if options.no_privileges {
        args.push("--no-privileges".into());
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.clean {
        args.push("--clean".into());
        args.push("--if-exists".into());
    }
    if options.create {
        args.push("--create".into());
    }
    for schema in &options.schemas {
        args.push(format!("--schema={}", pg_literal_pattern(schema)));
    }
    for (schema, table) in &options.tables {
        args.push(format!(
            "--table={}.{}",
            pg_literal_pattern(schema),
            pg_literal_pattern(table)
        ));
    }
    args.extend_from_slice(extra);
    args
}

/// `pg_restore`'s arguments for an archive of `format` (never plain): read from its standard
/// input, or from `directory` for the directory format.
pub(crate) fn pg_restore_args(
    endpoint: &Endpoint,
    options: &PgRestoreOptions,
    format: PgFormat,
    directory: Option<&Path>,
    extra: &[String],
) -> Vec<String> {
    let mut args = vec![
        format!("--dbname={}", pg_conninfo(endpoint, &endpoint.database)),
        "--verbose".to_string(),
        format!("--format={}", format.letter()),
        // Stop at the first error, as `psql` does with ON_ERROR_STOP.
        "--exit-on-error".to_string(),
    ];
    if options.clean {
        args.push("--clean".into());
        args.push("--if-exists".into());
    }
    if options.no_owner {
        args.push("--no-owner".into());
    }
    if options.create {
        args.push("--create".into());
    }
    args.extend_from_slice(extra);
    if let (PgFormat::Directory, Some(dir)) = (format, directory) {
        args.push(dir.display().to_string());
    }
    args
}

/// `psql`'s arguments for a SQL file read from its standard input: no `.psqlrc`, stop at the
/// first error.
pub(crate) fn psql_args(endpoint: &Endpoint, extra: &[String]) -> Vec<String> {
    let mut args = vec![
        format!("--dbname={}", pg_conninfo(endpoint, &endpoint.database)),
        "--no-psqlrc".to_string(),
        "--set=ON_ERROR_STOP=1".to_string(),
        "--echo-errors".to_string(),
    ];
    args.extend_from_slice(extra);
    args.push("--file=-".to_string());
    args
}

/// How MySQL's and MariaDB's clients connect, and a note when a check cannot be done.
pub(crate) fn mysql_connection_args(
    endpoint: &Endpoint,
    flavor: ToolFlavor,
) -> (Vec<String>, Option<String>) {
    let mut args = vec![
        format!("--host={}", endpoint.connect_host),
        format!("--port={}", endpoint.port),
        // `localhost` would mean a socket file.
        "--protocol=TCP".to_string(),
    ];
    if !endpoint.user.is_empty() {
        args.push(format!("--user={}", endpoint.user));
    }
    let mut note = None;
    let mut tls = endpoint.tls;
    if tls == TlsMode::VerifyFull && endpoint.server_name.is_some() {
        // The client checks the name it connected to, the tunnel's local end: only the
        // authority can be checked through a forward.
        tls = TlsMode::VerifyCa;
        note = Some(
            "the server's certificate name cannot be checked through the tunnel's local port: \
             only its authority is checked"
                .to_string(),
        );
    }
    match flavor {
        ToolFlavor::Mysql | ToolFlavor::Postgres => args.push(format!(
            "--ssl-mode={}",
            match tls {
                TlsMode::Disable => "DISABLED",
                TlsMode::Prefer => "PREFERRED",
                TlsMode::Require => "REQUIRED",
                TlsMode::VerifyCa => "VERIFY_CA",
                TlsMode::VerifyFull => "VERIFY_IDENTITY",
            }
        )),
        ToolFlavor::Mariadb => match tls {
            TlsMode::Disable => args.push("--skip-ssl".into()),
            TlsMode::Prefer => args.push("--skip-ssl-verify-server-cert".into()),
            TlsMode::Require => {
                args.push("--ssl".into());
                args.push("--skip-ssl-verify-server-cert".into());
            }
            TlsMode::VerifyCa | TlsMode::VerifyFull => {
                args.push("--ssl".into());
                args.push("--ssl-verify-server-cert".into());
            }
        },
    }
    if tls != TlsMode::Disable {
        if let Some(ca) = &endpoint.ca_certificate {
            args.push(format!("--ssl-ca={}", ca.display()));
        }
        if let (Some(cert), Some(key)) = (&endpoint.client_certificate, &endpoint.client_key) {
            args.push(format!("--ssl-cert={}", cert.display()));
            args.push(format!("--ssl-key={}", key.display()));
        }
    }
    (args, note)
}

/// `mysqldump`'s arguments; its output is its standard output. `views` are skipped when the
/// options leave views out (`db`, `view`); `server_is_mariadb` decides the column statistics.
pub(crate) fn mysqldump_args(
    endpoint: &Endpoint,
    tool: &ToolVersion,
    options: &MysqlDumpOptions,
    views: &[(String, String)],
    server_is_mariadb: bool,
    extra: &[String],
) -> Result<(Vec<String>, Option<String>), String> {
    let (mut args, note) = mysql_connection_args(endpoint, tool.flavor);
    args.push("--verbose".into());
    args.push("--default-character-set=utf8mb4".into());
    // Tablespace definitions need the PROCESS privilege and rarely belong in a backup.
    args.push("--no-tablespaces".into());
    match options.consistency {
        MysqlConsistency::SingleTransaction => {
            args.push("--single-transaction".into());
            args.push("--skip-lock-tables".into());
        }
        MysqlConsistency::LockAllTables => args.push("--lock-all-tables".into()),
        MysqlConsistency::NoLocks => args.push("--skip-lock-tables".into()),
    }
    args.push(
        if options.add_drop {
            "--add-drop-table"
        } else {
            "--skip-add-drop-table"
        }
        .into(),
    );
    if options.no_create {
        args.push("--no-create-info".into());
        args.push("--no-create-db".into());
    }
    if options.no_data {
        args.push("--no-data".into());
    }
    args.push(
        if options.extended_insert {
            "--extended-insert"
        } else {
            "--skip-extended-insert"
        }
        .into(),
    );
    if options.routines {
        args.push("--routines".into());
    }
    if options.events {
        args.push("--events".into());
    }
    args.push(
        if options.triggers {
            "--triggers"
        } else {
            "--skip-triggers"
        }
        .into(),
    );
    if options.hex_blob {
        args.push("--hex-blob".into());
    }
    // Only MySQL's own client has the option (MariaDB's does not know it).
    if tool.flavor == ToolFlavor::Mysql && tool.at_least(8, 0, 0) {
        match options.column_statistics {
            ColumnStatistics::Auto if server_is_mariadb => {
                args.push("--column-statistics=0".into())
            }
            ColumnStatistics::Auto => {}
            ColumnStatistics::Off => args.push("--column-statistics=0".into()),
            ColumnStatistics::On => args.push("--column-statistics=1".into()),
        }
    }
    if !options.include_views {
        for (db, view) in views {
            args.push(format!("--ignore-table={db}.{view}"));
        }
    }
    args.extend_from_slice(extra);
    let databases: Vec<&String> = if options.databases.is_empty() {
        vec![&endpoint.database]
    } else {
        options.databases.iter().collect()
    };
    if databases.iter().any(|d| d.is_empty()) {
        return Err("choose the database to back up".into());
    }
    if options.tables.is_empty() {
        args.push("--databases".into());
        // `--` would end the options, but mysqldump does not take it after `--databases`: a name
        // that starts with `-` is refused instead.
        for db in &databases {
            refuse_option_like(db)?;
            args.push((*db).clone());
        }
    } else {
        let [db] = databases.as_slice() else {
            return Err("tables can only be chosen in one database".into());
        };
        refuse_option_like(db)?;
        args.push((*db).clone());
        for table in &options.tables {
            refuse_option_like(table)?;
            args.push(table.clone());
        }
    }
    Ok((args, note))
}

fn refuse_option_like(name: &str) -> Result<(), String> {
    if name.starts_with('-') {
        Err(format!("the name {name:?} would be read as an option"))
    } else {
        Ok(())
    }
}

/// `mysql`'s arguments for a SQL file read from its standard input, and a warning when this
/// client cannot be kept from running the file's shell commands.
pub(crate) fn mysql_restore_args(
    endpoint: &Endpoint,
    tool: &ToolVersion,
    options: &MysqlRestoreOptions,
    extra: &[String],
) -> Result<(Vec<String>, Vec<String>), String> {
    let (mut args, note) = mysql_connection_args(endpoint, tool.flavor);
    let mut notes: Vec<String> = note.into_iter().collect();
    // Only `mysql` has it (`mysqldump` does not).
    args.push(format!(
        "--connect-timeout={}",
        endpoint.connect_timeout_secs
    ));
    if options.disable_foreign_keys {
        args.push("--init-command=SET FOREIGN_KEY_CHECKS=0".into());
    }
    match sandbox_flag(tool) {
        Some(flag) => args.push(flag.into()),
        None => notes.push(format!(
            "this {} client runs the file's shell commands (\\!) if it has any: restore only a \
             file you trust",
            tool.text
        )),
    }
    args.extend_from_slice(extra);
    let database = options
        .database
        .as_deref()
        .filter(|d| !d.is_empty())
        .unwrap_or(&endpoint.database);
    if !database.is_empty() {
        refuse_option_like(database)?;
        args.push(database.to_string());
    }
    Ok((args, notes))
}

/// The flag that keeps `mysql` from running shell commands, where this version has one.
fn sandbox_flag(tool: &ToolVersion) -> Option<&'static str> {
    let (major, minor, _) = tool.number;
    match tool.flavor {
        ToolFlavor::Mysql => {
            let has = match (major, minor) {
                (8, 0) => tool.at_least(8, 0, 40),
                (8, 4) => tool.at_least(8, 4, 3),
                (8, 1..=3) => false,
                (9, 0) => false,
                _ => tool.at_least(9, 1, 0),
            };
            has.then_some("--skip-system-command")
        }
        ToolFlavor::Mariadb => {
            let has = match (major, minor) {
                (10, 5) => tool.at_least(10, 5, 25),
                (10, 6) => tool.at_least(10, 6, 18),
                (10, 11) => tool.at_least(10, 11, 8),
                (11, 0) => tool.at_least(11, 0, 6),
                (11, 1) => tool.at_least(11, 1, 5),
                (11, 2) => tool.at_least(11, 2, 4),
                (11, 4) => tool.at_least(11, 4, 2),
                (10, _) | (11, 3) => false,
                _ => tool.at_least(11, 5, 0),
            };
            has.then_some("--sandbox")
        }
        ToolFlavor::Postgres => None,
    }
}

/// Splits typed extra arguments: whitespace separates them, `'…'` is literal and `"…"` takes
/// `\"` and `\\`. Nothing is expanded: there is no shell.
pub fn split_extra_args(text: &str) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(c) => current.push(c),
                        None => return Err("a quote (') is not closed".into()),
                    }
                }
            }
            '"' => {
                started = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(c @ ('"' | '\\')) => current.push(c),
                            Some(c) => {
                                current.push('\\');
                                current.push(c);
                            }
                            None => return Err("a quote (\") is not closed".into()),
                        },
                        Some(c) => current.push(c),
                        None => return Err("a quote (\") is not closed".into()),
                    }
                }
            }
            c => {
                started = true;
                current.push(c);
            }
        }
    }
    if started {
        args.push(current);
    }
    if args.iter().any(|a| a.contains('\0')) {
        return Err("an argument cannot hold a NUL character".into());
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::tools::ToolName;

    fn endpoint() -> Endpoint {
        Endpoint {
            connect_host: "127.0.0.1".into(),
            port: 50123,
            server_name: Some("db.internal".into()),
            user: "app".into(),
            database: "shop".into(),
            tls: TlsMode::VerifyFull,
            ca_certificate: Some(PathBuf::from("/certs/ca.crt")),
            client_certificate: None,
            client_key: None,
            connect_timeout_secs: 15,
        }
    }

    #[test]
    fn libpq_keeps_the_real_host_name_for_verify_full() {
        let info = pg_conninfo(&endpoint(), "shop");
        assert!(info.starts_with("host='db.internal' hostaddr='127.0.0.1' port='50123'"));
        assert!(info.contains("sslmode='verify-full'"));
        assert!(info.contains("sslrootcert='/certs/ca.crt'"));
        let mut odd = endpoint();
        odd.database = "it's \\ odd".into();
        odd.connect_host = "host.docker.internal".into();
        let info = pg_conninfo(&odd, &odd.database);
        assert!(info.starts_with("host='host.docker.internal' port="));
        assert!(info.contains(r"dbname='it\'s \\ odd'"));
    }

    #[test]
    fn pg_dump_names_are_literal_patterns() {
        let options = PgDumpOptions {
            schemas: vec!["Sales".into()],
            tables: vec![("public".into(), "a.b*\"c".into())],
            format: PgFormat::Tar,
            compression: Some(5),
            clean: true,
            ..PgDumpOptions::default()
        };
        let args = pg_dump_args(&endpoint(), &options, None, &[]);
        assert!(args.contains(&"--schema=\"Sales\"".to_string()));
        assert!(args.contains(&"--table=\"public\".\"a.b*\"\"c\"".to_string()));
        assert!(args.contains(&"--format=t".to_string()));
        assert!(
            !args.iter().any(|a| a.starts_with("--compress")),
            "tar has no compression"
        );
        assert!(args.contains(&"--if-exists".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("--file")));
    }

    #[test]
    fn mysql_verify_full_through_a_tunnel_checks_the_authority_only() {
        let tool = ToolVersion::parse(ToolName::Mysqldump, "mysqldump  Ver 8.4.13 for Linux");
        let (args, note) = mysqldump_args(
            &endpoint(),
            &tool,
            &MysqlDumpOptions::default(),
            &[],
            true,
            &[],
        )
        .unwrap();
        assert!(args.contains(&"--ssl-mode=VERIFY_CA".to_string()));
        assert!(note.is_some());
        assert!(args.contains(&"--column-statistics=0".to_string()));
        assert_eq!(args.last().unwrap(), "shop");
        assert!(args.contains(&"--databases".to_string()));
        let maria = ToolVersion::parse(
            ToolName::Mysqldump,
            "mariadb-dump from 11.4.13-MariaDB, client 10.19",
        );
        let options = MysqlDumpOptions {
            tables: vec!["orders".into()],
            include_views: false,
            ..MysqlDumpOptions::default()
        };
        let views = [("shop".to_string(), "v_orders".to_string())];
        let (args, _) = mysqldump_args(&endpoint(), &maria, &options, &views, true, &[]).unwrap();
        assert!(!args.iter().any(|a| a.starts_with("--column-statistics")));
        assert!(!args.iter().any(|a| a.starts_with("--ssl-mode")));
        assert!(args.contains(&"--ignore-table=shop.v_orders".to_string()));
        assert_eq!(args[args.len() - 2..], ["shop", "orders"]);
        let bad = MysqlDumpOptions {
            databases: vec!["--result-file=/etc/x".into()],
            ..MysqlDumpOptions::default()
        };
        assert!(mysqldump_args(&endpoint(), &maria, &bad, &[], true, &[]).is_err());
    }

    #[test]
    fn mysql_restores_sandboxed_where_the_client_can() {
        let options = MysqlRestoreOptions {
            database: None,
            disable_foreign_keys: true,
        };
        for (text, flag) in [
            ("mysql  Ver 8.4.13 for Linux", Some("--skip-system-command")),
            ("mysql  Ver 8.0.36 for Win64", None),
            // Windows' client starts with its path, whose folder holds another version number.
            (
                r"C:\Program Files\MySQL\MySQL Server 8.4\bin\mysql.exe  Ver 8.4.9 for Win64 on x86_64",
                Some("--skip-system-command"),
            ),
            ("mysql  Ver 9.4.0 for Linux", Some("--skip-system-command")),
            (
                "mariadb from 11.4.13-MariaDB, client 15.2",
                Some("--sandbox"),
            ),
            ("mysql  Ver 15.1 Distrib 10.6.12-MariaDB, for Linux", None),
        ] {
            let tool = ToolVersion::parse(ToolName::Mysql, text);
            let (args, notes) = mysql_restore_args(&endpoint(), &tool, &options, &[]).unwrap();
            assert!(args.contains(&"--init-command=SET FOREIGN_KEY_CHECKS=0".to_string()));
            match flag {
                Some(flag) => assert!(args.contains(&flag.to_string()), "{text}"),
                None => assert!(notes.iter().any(|n| n.contains("trust")), "{text}"),
            }
        }
    }

    #[test]
    fn extra_arguments_split_without_a_shell() {
        assert_eq!(
            split_extra_args(r#"  --jobs=4 'a b' "c \"d\" \\ e" $HOME;rm  "#).unwrap(),
            ["--jobs=4", "a b", r#"c "d" \ e"#, "$HOME;rm"]
        );
        assert_eq!(split_extra_args("''").unwrap(), [""]);
        assert!(split_extra_args("'open").is_err());
        assert!(split_extra_args("\"open").is_err());
        assert!(split_extra_args("").unwrap().is_empty());
    }
}
