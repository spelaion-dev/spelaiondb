//! Connection URLs (`postgres://user:pass@host:5432/db?sslmode=require`, `mysql://…`,
//! `mariadb://…`, their `jdbc:` forms and `sqlite:path`), to fill the connection form from
//! what a hosting provider or a colleague hands over. The password, if there is one, comes
//! back as a [`Secret`] and never goes to a file from here.

use spelunk_driver_api::Secret;
use url::Url;

use crate::model::{Engine, TlsModeConfig};

/// What a connection URL says. Missing parts stay empty for the form's defaults.
#[derive(Debug)]
pub struct UrlConnection {
    pub engine: Engine,
    pub host: String,
    pub port: Option<u16>,
    /// Database name; for SQLite, the file path.
    pub database: String,
    pub user: String,
    pub password: Option<Secret>,
    pub tls_mode: Option<TlsModeConfig>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum UrlError {
    #[error("not a connection URL this app knows (postgres://, mysql://, mariadb://, sqlite:)")]
    UnknownScheme,
    #[error("the URL could not be read: {0}")]
    Invalid(String),
    #[error("unknown SSL mode \"{0}\"")]
    UnknownTlsMode(String),
}

/// Reads a connection URL.
pub fn parse_connection_url(text: &str) -> Result<UrlConnection, UrlError> {
    let text = text.trim();
    let lower = text.to_ascii_lowercase();
    let text = if lower.starts_with("jdbc:") {
        &text[5..]
    } else {
        text
    };
    let scheme = text
        .split_once(':')
        .map(|(s, _)| s.to_ascii_lowercase())
        .ok_or(UrlError::UnknownScheme)?;
    let engine = match scheme.as_str() {
        "postgres" | "postgresql" => Engine::Postgres,
        "mysql" => Engine::Mysql,
        "mariadb" => Engine::Mariadb,
        "sqlite" | "sqlite3" => return Ok(sqlite(text)),
        _ => return Err(UrlError::UnknownScheme),
    };
    let url = Url::parse(text).map_err(|e| UrlError::Invalid(e.to_string()))?;
    let mut user = decode(url.username());
    let mut password = url.password().map(decode);
    let mut tls_mode = None;
    for (key, value) in url.query_pairs() {
        match key.to_ascii_lowercase().as_str() {
            // JDBC puts the credentials in the query.
            "user" if user.is_empty() => user = value.into_owned(),
            "password" if password.is_none() => password = Some(value.into_owned()),
            "sslmode" | "ssl-mode" | "ssl_mode" => tls_mode = Some(tls(&value)?),
            "ssl" | "usessl" if value.eq_ignore_ascii_case("true") => {
                tls_mode = tls_mode.or(Some(TlsModeConfig::Require));
            }
            _ => {}
        }
    }
    Ok(UrlConnection {
        engine,
        // IPv6 hosts come as `[::1]`; the form wants the address.
        host: url
            .host_str()
            .unwrap_or("")
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string(),
        port: url.port(),
        database: decode(url.path().trim_start_matches('/')),
        user,
        password: password.map(Secret::new),
        tls_mode,
    })
}

fn sqlite(text: &str) -> UrlConnection {
    let rest = text.split_once(':').map_or("", |(_, rest)| rest);
    // `sqlite:///C:/data/x.db`, `sqlite:///home/me/x.db` and `sqlite:x.db`.
    let path = match rest.strip_prefix("//") {
        Some(after) => {
            let windows = after.len() > 3 && after.as_bytes()[2] == b':' && after.starts_with('/');
            if windows { &after[1..] } else { after }
        }
        None => rest,
    };
    UrlConnection {
        engine: Engine::Sqlite,
        host: String::new(),
        port: None,
        database: decode(path.split('?').next().unwrap_or("")),
        user: String::new(),
        password: None,
        tls_mode: None,
    }
}

/// libpq's `sslmode` and MySQL's `ssl-mode` values.
fn tls(value: &str) -> Result<TlsModeConfig, UrlError> {
    Ok(
        match value.to_ascii_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" => TlsModeConfig::Disable,
            "allow" | "prefer" | "preferred" => TlsModeConfig::Prefer,
            "require" | "required" => TlsModeConfig::Require,
            "verify-ca" => TlsModeConfig::VerifyCa,
            "verify-full" | "verify-identity" => TlsModeConfig::VerifyFull,
            _ => return Err(UrlError::UnknownTlsMode(value.to_string())),
        },
    )
}

/// `%XX` escapes to bytes; a `%` not followed by two hex digits stays as it is, and invalid
/// UTF-8 is replaced: decoding never fails.
fn decode(part: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let bytes = part.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).copied().and_then(hex),
                bytes.get(i + 2).copied().and_then(hex),
            )
        {
            out.push(hi << 4 | lo);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_urls_with_escapes_and_ssl() {
        let c = parse_connection_url(
            "postgresql://app%40corp:p%C3%A4ss%2Fw@db.example.com:6543/shop%20eu?sslmode=verify-full&application_name=x",
        )
        .unwrap();
        assert_eq!(c.engine, Engine::Postgres);
        assert_eq!(c.host, "db.example.com");
        assert_eq!(c.port, Some(6543));
        assert_eq!(c.database, "shop eu");
        assert_eq!(c.user, "app@corp");
        assert_eq!(c.password.unwrap().expose(), "päss/w");
        assert_eq!(c.tls_mode, Some(TlsModeConfig::VerifyFull));

        let c = parse_connection_url("postgres://localhost").unwrap();
        assert_eq!(
            (c.host.as_str(), c.port, c.database.as_str()),
            ("localhost", None, "")
        );
        assert!(c.password.is_none() && c.tls_mode.is_none());
    }

    #[test]
    fn mysql_mariadb_and_jdbc() {
        let c = parse_connection_url("mysql://root@[::1]:3307/app?ssl-mode=REQUIRED").unwrap();
        assert_eq!(
            (c.engine, c.host.as_str(), c.port),
            (Engine::Mysql, "::1", Some(3307))
        );
        assert_eq!(c.tls_mode, Some(TlsModeConfig::Require));
        let c = parse_connection_url("MariaDB://u:p@h/db").unwrap();
        assert_eq!(c.engine, Engine::Mariadb);
        let c = parse_connection_url(
            "jdbc:postgresql://pg.internal:5432/sales?user=reporter&password=s%26cret&ssl=true",
        )
        .unwrap();
        assert_eq!((c.engine, c.user.as_str()), (Engine::Postgres, "reporter"));
        assert_eq!(c.password.unwrap().expose(), "s&cret");
        assert_eq!(c.tls_mode, Some(TlsModeConfig::Require));
        let c = parse_connection_url("jdbc:mysql://h:3306/db?sslMode=VERIFY_IDENTITY").unwrap();
        assert_eq!(c.tls_mode, Some(TlsModeConfig::VerifyFull));
    }

    #[test]
    fn sqlite_paths() {
        let path = |u: &str| parse_connection_url(u).unwrap().database;
        assert_eq!(path("sqlite:///C:/data/shop.db"), "C:/data/shop.db");
        assert_eq!(path("sqlite:///home/me/shop.db"), "/home/me/shop.db");
        assert_eq!(path("sqlite:shop%20copy.db"), "shop copy.db");
        // Stray `%` and non-ASCII text survive decoding.
        assert_eq!(path("sqlite:100%é%2.db"), "100%é%2.db");
    }

    #[test]
    fn bad_urls_are_explained() {
        assert_eq!(
            parse_connection_url("redis://h").unwrap_err(),
            UrlError::UnknownScheme
        );
        assert_eq!(
            parse_connection_url("just text").unwrap_err(),
            UrlError::UnknownScheme
        );
        assert_eq!(
            parse_connection_url("postgres://h/db?sslmode=sometimes").unwrap_err(),
            UrlError::UnknownTlsMode("sometimes".into())
        );
        assert!(matches!(
            parse_connection_url("postgres://h:99999/db"),
            Err(UrlError::Invalid(_))
        ));
    }
}
