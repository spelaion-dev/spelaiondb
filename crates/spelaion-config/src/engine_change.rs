//! Changing the engine of a connection while keeping its settings (ADR-030), like DBeaver's
//! "Change connection driver". The core decides what still applies: the form shows the user
//! what is kept, changed, dropped and left to fill in before anything is saved, and saving
//! goes through [`crate::update_connection`] like any other edit (only the changed keys are
//! rewritten, and a repository connection goes through trust).

use crate::draft::{CloudToken, ConnectionDraft, PasswordChoice};
use crate::model::{ConnectionFile, Engine, TlsModeConfig};
use crate::trust::FieldChange;

/// What [`ConnectionDraft::with_engine`] did to each setting. Field names are those of the
/// connection file (`host`, `tls.cert`, `kubernetes`…); values are what the form shows, and
/// a password is only ever its reference (`keychain`, `prompt`, `env:VAR`…), never a secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineChange {
    pub from: Engine,
    pub to: Engine,
    /// Settings that apply as they are.
    pub kept: Vec<String>,
    /// Settings with a new value (the port that was the old engine's default, for example).
    pub changed: Vec<FieldChange>,
    /// Settings the new engine has no use for: they are removed from the file on saving.
    pub dropped: Vec<String>,
    /// Settings the new engine needs and the connection does not have yet.
    pub required: Vec<String>,
}

fn filled(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// How the form shows a password choice: the reference only.
fn password_label(choice: &PasswordChoice) -> Option<String> {
    match choice {
        PasswordChoice::None => None,
        PasswordChoice::Keychain => Some("keychain".into()),
        PasswordChoice::Prompt => Some("prompt".into()),
        PasswordChoice::Env(var) => Some(format!("env:{}", var.trim())),
        PasswordChoice::Other(reference) => Some(reference.clone()),
        PasswordChoice::CloudToken(token) => Some(
            match token {
                CloudToken::AwsRds => "aws-rds",
                CloudToken::AzureAd => "azure-ad",
                CloudToken::GoogleCloudSql => "gcloud-sql",
            }
            .into(),
        ),
    }
}

/// Engines that accept a client certificate or a cloud token (ADR-022).
fn passwordless(engine: Engine) -> bool {
    match engine {
        Engine::Postgres | Engine::Mysql | Engine::Mariadb => true,
        Engine::Sqlserver | Engine::Sqlite | Engine::Duckdb => false,
    }
}

impl EngineChange {
    fn new(from: Engine, to: Engine) -> Self {
        Self {
            from,
            to,
            kept: Vec::new(),
            changed: Vec::new(),
            dropped: Vec::new(),
            required: Vec::new(),
        }
    }

    fn keep_if(&mut self, field: &str, present: bool) {
        if present {
            self.kept.push(field.into());
        }
    }

    fn drop_if(&mut self, field: &str, present: bool) {
        if present {
            self.dropped.push(field.into());
        }
    }

    fn change(&mut self, field: &str, before: Option<String>, after: Option<String>) {
        if before != after {
            self.changed.push(FieldChange {
                field: field.into(),
                before,
                after,
            });
        }
    }
}

impl ConnectionDraft {
    /// The same connection for another engine, and what that did to each setting.
    ///
    /// - Between database servers, the server, credentials, database, tunnel, Kubernetes,
    ///   TLS, keep-alive and local port stay. The port moves to the new engine's default if
    ///   it was the old engine's default; a port of its own stays. A client certificate or
    ///   a cloud token is dropped for SQL Server, which takes neither (the password is then
    ///   asked when connecting).
    /// - A server and a file engine (SQLite, DuckDB) mean different things by `database`: it
    ///   is cleared and must be filled in. Leaving a server drops every server setting;
    ///   reaching one asks for the host and the user, keeps the password in the keychain as a
    ///   new connection would, and turns on the suggested keep-alive.
    /// - SQLite ↔ DuckDB keep the file, except a SQLite file on a server (ADR-018): DuckDB
    ///   opens local files only, so the path on the server, the tunnel and `sqlite3` go.
    /// - The name, folder, environment, read-only mode, agents' access and every other setting
    ///   stay as they are.
    pub fn with_engine(&self, engine: Engine) -> (ConnectionDraft, EngineChange) {
        let (from, to) = (self.engine, engine);
        let mut report = EngineChange::new(from, to);
        let mut next = ConnectionDraft {
            engine: to,
            ..self.clone()
        };
        if from == to {
            return (next, report);
        }
        let (server_from, server_to) = (!from.is_file(), !to.is_file());
        let tunnel = filled(&self.tunnel);
        let remote_sqlite = from == Engine::Sqlite && tunnel.is_some();
        let database = filled(&self.database);
        let tls_set = self.tls_mode != TlsModeConfig::default();
        let cert_set = filled(&self.tls_cert).is_some() || filled(&self.tls_key).is_some();

        if server_from && server_to {
            report.keep_if("host", filled(&self.host).is_some());
            match self.port {
                Some(port) if Some(port) != from.default_port() => report.kept.push("port".into()),
                _ => {
                    next.port = None;
                    let label = |e: Engine| e.default_port().map(|p| p.to_string());
                    if from.default_port() == to.default_port() {
                        report.keep_if("port", self.port.is_some());
                    } else {
                        report.change("port", label(from), label(to));
                    }
                }
            }
            report.keep_if("database", database.is_some());
            report.keep_if("user", filled(&self.user).is_some());
            match &self.password {
                PasswordChoice::CloudToken(_) if !passwordless(to) => {
                    next.password = PasswordChoice::Prompt;
                    report.change(
                        "password",
                        password_label(&self.password),
                        password_label(&next.password),
                    );
                }
                other => report.keep_if("password", password_label(other).is_some()),
            }
            report.keep_if("tunnel", tunnel.is_some());
            report.keep_if("local_port", self.local_port.is_some() && tunnel.is_some());
            report.keep_if("kubernetes", self.kubernetes.is_some());
            report.keep_if("keepalive", self.keepalive.is_some_and(|s| s > 0));
            report.keep_if("connect_timeout", self.connect_timeout.is_some());
            report.keep_if("tls.mode", tls_set);
            report.keep_if("tls.ca", filled(&self.tls_ca).is_some());
            if passwordless(to) {
                report.keep_if("tls.cert", cert_set);
            } else {
                report.drop_if("tls.cert", cert_set);
                next.tls_cert.clear();
                next.tls_key.clear();
            }
        } else if server_from {
            // A server becomes a file: nothing of the server applies.
            report.drop_if("host", filled(&self.host).is_some());
            report.drop_if("port", self.port.is_some());
            report.drop_if("user", filled(&self.user).is_some());
            report.drop_if("password", password_label(&self.password).is_some());
            report.drop_if("tunnel", tunnel.is_some());
            report.drop_if("local_port", self.local_port.is_some() && tunnel.is_some());
            report.drop_if("kubernetes", self.kubernetes.is_some());
            report.drop_if("keepalive", self.keepalive.is_some_and(|s| s > 0));
            report.drop_if("connect_timeout", self.connect_timeout.is_some());
            report.drop_if("tls.mode", tls_set);
            report.drop_if("tls.ca", filled(&self.tls_ca).is_some());
            report.drop_if("tls.cert", cert_set);
            next.host.clear();
            next.port = None;
            next.user.clear();
            next.password = PasswordChoice::None;
            next.tunnel.clear();
            next.local_port = None;
            next.kubernetes = None;
            next.keepalive = None;
            next.connect_timeout = None;
            next.tls_mode = TlsModeConfig::default();
            next.tls_ca.clear();
            next.tls_cert.clear();
            next.tls_key.clear();
            report.change("database", database, None);
            next.database.clear();
            report.required.push("database".into());
        } else if server_to {
            // A file becomes a server: the path means nothing to it.
            report.change("database", database, None);
            next.database.clear();
            if remote_sqlite {
                report.dropped.push("tunnel".into());
                report.drop_if("sqlite3", filled(&self.sqlite3).is_some());
            }
            next.tunnel.clear();
            next.sqlite3.clear();
            next.local_port = None;
            next.port = None;
            next.kubernetes = None;
            // What a new connection to a server starts with.
            if self.password == PasswordChoice::None {
                next.password = PasswordChoice::Keychain;
                report.change("password", None, password_label(&next.password));
            }
            if self.keepalive.is_none_or(|s| s == 0) {
                next.keepalive = Some(ConnectionFile::SUGGESTED_KEEPALIVE);
                report.change(
                    "keepalive",
                    None,
                    Some(ConnectionFile::SUGGESTED_KEEPALIVE.to_string()),
                );
            }
            report.keep_if("tls.mode", tls_set);
        } else if remote_sqlite {
            // SQLite on a server becomes DuckDB, which opens local files only.
            report.change("database", database, None);
            next.database.clear();
            report.required.push("database".into());
            report.dropped.push("tunnel".into());
            report.drop_if("sqlite3", filled(&self.sqlite3).is_some());
            next.tunnel.clear();
            next.sqlite3.clear();
        } else {
            report.keep_if("database", database.is_some());
            report.drop_if("sqlite3", filled(&self.sqlite3).is_some());
            next.sqlite3.clear();
        }

        if server_to {
            // What the new engine's connections cannot do without (as the file checks).
            if filled(&next.host).is_none() && next.kubernetes.is_none() {
                report.required.push("host".into());
            }
            if filled(&next.user).is_none() {
                report.required.push("user".into());
            }
            if to == Engine::Postgres && filled(&next.database).is_none() {
                report.required.push("database".into());
            }
        }
        report.keep_if("description", filled(&self.description).is_some());
        report.keep_if("tags", !self.tags.is_empty());
        report.keep_if("env", filled(&self.env).is_some());
        report.keep_if("read_only", self.read_only);
        report.keep_if("mcp", self.mcp_exposed);
        (next, report)
    }
}
