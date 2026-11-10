//! The server's settings, variables and status counters, read once for the viewer next to the
//! process monitor. Every statement is a read checked by [`crate::observe::read`].

use crate::Session;
use crate::observe::read;

/// What a [`ServerSetting`] is, in the engine's own terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// A configuration parameter (PostgreSQL's `pg_settings`, SQL Server's `sys.configurations`).
    Setting,
    /// A global system variable (MySQL and MariaDB).
    Variable,
    /// A global status counter (MySQL and MariaDB).
    Status,
    /// A fixed property of the server (SQL Server's `SERVERPROPERTY`).
    Property,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSetting {
    pub name: String,
    pub value: String,
    pub kind: SettingKind,
    pub category: Option<String>,
    pub description: Option<String>,
    /// Where the value comes from (PostgreSQL's `source`), or a value set but not yet in use.
    pub source: Option<String>,
}

/// Reads every setting the server exposes, in name order within each kind.
pub async fn server_settings(session: &Session) -> Result<Vec<ServerSetting>, String> {
    let reads: &[(&str, SettingKind)] = match session.dialect().name() {
        "postgres" => &[(POSTGRES_SETTINGS, SettingKind::Setting)],
        "mysql" => &[
            ("SHOW GLOBAL VARIABLES", SettingKind::Variable),
            ("SHOW GLOBAL STATUS", SettingKind::Status),
        ],
        "sqlserver" => &[
            (SQLSERVER_CONFIGURATIONS, SettingKind::Setting),
            (SQLSERVER_PROPERTIES, SettingKind::Property),
        ],
        _ => return Err("the settings viewer is not supported for this engine".into()),
    };
    let mut settings = Vec::new();
    for (sql, kind) in reads {
        let rows = read(session, sql).await?;
        // SHOW GLOBAL … answers Variable_name and Value.
        let name = rows
            .column("name")
            .or_else(|_| rows.column("Variable_name"))?;
        let value = rows.column("value")?;
        let optional = |column: &str| rows.column(column).ok();
        let (category, description, source) = (
            optional("category"),
            optional("description"),
            optional("source"),
        );
        let mut part: Vec<ServerSetting> = rows
            .rows
            .iter()
            .filter_map(|row| {
                Some(ServerSetting {
                    name: row[name].clone()?,
                    value: row[value].clone().unwrap_or_default(),
                    kind: *kind,
                    category: category.and_then(|i| row[i].clone()),
                    description: description.and_then(|i| row[i].clone()),
                    source: source.and_then(|i| row[i].clone()),
                })
            })
            .collect();
        part.sort_by_key(|s| s.name.to_lowercase());
        settings.extend(part);
    }
    Ok(settings)
}

// current_setting() is the value as SHOW writes it (`128MB`, not `16384` of unit `8kB`);
// pg_settings only lists what the user may read, so it never refuses one of these.
pub(crate) const POSTGRES_SETTINGS: &str = r#"
SELECT name, current_setting(name) AS value,
       category, short_desc AS description,
       CASE WHEN pending_restart THEN source || ' (pending restart)' ELSE source END AS source
FROM pg_catalog.pg_settings"#;

pub(crate) const SQLSERVER_CONFIGURATIONS: &str = r#"
SELECT CONVERT(nvarchar(256), name) AS name, CONVERT(nvarchar(256), value_in_use) AS value,
       CASE WHEN is_advanced = 1 THEN 'Advanced' ELSE 'Basic' END AS category,
       CONVERT(nvarchar(512), description) AS description,
       CASE WHEN CONVERT(nvarchar(256), value) <> CONVERT(nvarchar(256), value_in_use) THEN 'configured ' + CONVERT(nvarchar(256), value) + ', not in use yet' END AS source
FROM sys.configurations"#;

pub(crate) const SQLSERVER_PROPERTIES: &str = r#"
SELECT p.name, CONVERT(nvarchar(512), SERVERPROPERTY(p.name)) AS value
FROM (VALUES ('BuildClrVersion'), ('Collation'), ('Edition'), ('EngineEdition'), ('InstanceDefaultBackupPath'),
             ('InstanceDefaultDataPath'), ('InstanceDefaultLogPath'), ('IsClustered'), ('IsFullTextInstalled'),
             ('IsHadrEnabled'), ('IsIntegratedSecurityOnly'), ('IsSingleUser'), ('MachineName'), ('ProductLevel'),
             ('ProductUpdateLevel'), ('ProductVersion'), ('ServerName')) AS p(name)"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_is_a_read() {
        for (engine, sql) in [
            ("postgres", POSTGRES_SETTINGS),
            ("mysql", "SHOW GLOBAL VARIABLES"),
            ("mysql", "SHOW GLOBAL STATUS"),
            ("sqlserver", SQLSERVER_CONFIGURATIONS),
            ("sqlserver", SQLSERVER_PROPERTIES),
        ] {
            crate::classify_read_only(engine, sql)
                .unwrap_or_else(|e| panic!("{engine}: {e}\n{sql}"));
        }
    }
}
