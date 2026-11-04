//! Variables in values: `${user.<name>}` (from `user.toml`) and `${env:<NAME>}`.
//! `$$` is a literal `$`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn expand(
    value: &str,
    vars: &BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        if let Some(after) = rest.strip_prefix("$$") {
            out.push('$');
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let end = after
                .find('}')
                .ok_or_else(|| format!("unclosed variable in {value:?}"))?;
            let name = &after[..end];
            let resolved = if let Some(key) = name.strip_prefix("user.") {
                vars.get(key).cloned().ok_or_else(|| {
                    format!("variable ${{{name}}} is not defined: add it to the [vars] section of your user.toml")
                })?
            } else if let Some(key) = name.strip_prefix("env:") {
                env(key).ok_or_else(|| format!("environment variable {key} is not set"))?
            } else {
                return Err(format!(
                    "unknown variable ${{{name}}}: use ${{user.…}} or ${{env:…}}"
                ));
            };
            out.push_str(&resolved);
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The `${user.<name>}` variables in `value` that `vars` does not define, in order.
pub(crate) fn missing_user_vars(value: &str, vars: &BTreeMap<String, String>) -> Vec<String> {
    let mut missing = Vec::new();
    let mut rest = value;
    while let Some(pos) = rest.find('$') {
        rest = &rest[pos..];
        if let Some(after) = rest.strip_prefix("$$") {
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
            let Some(end) = after.find('}') else { break };
            if let Some(key) = after[..end].strip_prefix("user.")
                && !vars.contains_key(key)
            {
                missing.push(key.to_string());
            }
            rest = &after[end + 1..];
        } else {
            rest = &rest[1..];
        }
    }
    missing
}

/// Every text value of a configuration file (to look for variables in all its fields).
pub(crate) fn text_values(file: &impl serde::Serialize) -> Vec<String> {
    fn walk(value: toml::Value, out: &mut Vec<String>) {
        match value {
            toml::Value::String(text) => out.push(text),
            toml::Value::Array(items) => items.into_iter().for_each(|v| walk(v, out)),
            toml::Value::Table(table) => table.into_iter().for_each(|(_, v)| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(value) = toml::Value::try_from(file) {
        walk(value, &mut out);
    }
    out
}

/// A name `${user.<name>}` may use: letters, digits, `_`, `-` and `.`.
pub fn valid_var_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

/// Configuration path: `~/` is the home directory and relative paths hang from `base`
/// (the repository root), so the same config works on any machine.
pub fn resolve_path(value: &str, base: &Path) -> PathBuf {
    if let Some(rest) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// The user's home directory.
pub(crate) fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> BTreeMap<String, String> {
        BTreeMap::from([("db_user".to_string(), "ana".to_string())])
    }

    fn env(name: &str) -> Option<String> {
        (name == "REGION").then(|| "eu".to_string())
    }

    #[test]
    fn expands_user_and_env_variables() {
        assert_eq!(
            expand("${user.db_user}@db-${env:REGION}", &vars(), &env).unwrap(),
            "ana@db-eu"
        );
        assert_eq!(
            expand("no variables", &vars(), &env).unwrap(),
            "no variables"
        );
        assert_eq!(
            expand("cost: $$5 and $x", &vars(), &env).unwrap(),
            "cost: $5 and $x"
        );
    }

    #[test]
    fn missing_or_malformed_variables_explain_what_to_do() {
        let e = expand("${user.nobody}", &vars(), &env).unwrap_err();
        assert!(e.contains("[vars]"), "{e}");
        assert!(
            expand("${env:NOTHING}", &vars(), &env)
                .unwrap_err()
                .contains("NOTHING")
        );
        assert!(
            expand("${other}", &vars(), &env)
                .unwrap_err()
                .contains("unknown")
        );
        assert!(
            expand("${user.db_user", &vars(), &env)
                .unwrap_err()
                .contains("unclosed")
        );
    }

    #[test]
    fn paths_are_relative_to_the_repository() {
        let base = Path::new("/repo");
        assert_eq!(
            resolve_path("data/app.db", base),
            Path::new("/repo/data/app.db")
        );
        assert!(resolve_path("~/certs/ca.pem", base).ends_with("certs/ca.pem"));
    }
}
