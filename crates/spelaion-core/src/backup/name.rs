//! A backup's file name, from the dialog's pattern (ADR-049).

use chrono::{DateTime, Local};

/// The pattern the dialog starts with.
pub const DEFAULT_NAME_PATTERN: &str = "{database}_{timestamp}";

/// The longest file name written, before its extension.
const NAME_MAX: usize = 180;

/// Renders `pattern`: `{database}`, `{connection}`, `{timestamp}` (`20261111-143005`) and
/// `{date}` (`2026-11-11`) are replaced; any other `{…}` is refused. The result is safe on every
/// system: no path separators, reserved characters or names, no trailing dot or space.
pub fn render_file_name(
    pattern: &str,
    database: &str,
    connection: &str,
    now: DateTime<Local>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = pattern;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err("a { in the file name is not closed".into());
        };
        match &after[..close] {
            "database" => out.push_str(database),
            "connection" => out.push_str(connection),
            "timestamp" => out.push_str(&now.format("%Y%m%d-%H%M%S").to_string()),
            "date" => out.push_str(&now.format("%Y-%m-%d").to_string()),
            other => {
                return Err(format!(
                    "{{{other}}} is not a file name field: use {{database}}, {{connection}}, {{timestamp}} or {{date}}"
                ));
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    let mut name: String = out
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while name.ends_with(['.', ' ']) {
        name.pop();
    }
    let name = name.trim_start().to_string();
    if name.is_empty() || name.chars().all(|c| c == '.' || c == '_') {
        return Err("the file name is empty".into());
    }
    let mut name: String = name.chars().take(NAME_MAX).collect();
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        name.insert(0, '_');
    }
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn names_are_rendered_and_made_safe() {
        let now = Local.with_ymd_and_hms(2026, 11, 11, 14, 30, 5).unwrap();
        assert_eq!(
            render_file_name(DEFAULT_NAME_PATTERN, "shop", "Prod", now).unwrap(),
            "shop_20261111-143005"
        );
        assert_eq!(
            render_file_name("{connection} {date}", "x", "a/b:c", now).unwrap(),
            "a_b_c 2026-11-11"
        );
        assert_eq!(
            render_file_name("{database}", "../../etc/passwd", "c", now).unwrap(),
            ".._.._etc_passwd"
        );
        assert_eq!(
            render_file_name("{database}", "con", "c", now).unwrap(),
            "_con"
        );
        assert_eq!(render_file_name("x. ", "", "", now).unwrap(), "x");
        assert!(render_file_name("{database}", "", "c", now).is_err());
        assert!(render_file_name("{host}", "d", "c", now).is_err());
        assert!(render_file_name("{database", "d", "c", now).is_err());
        assert!(render_file_name("..", "d", "c", now).is_err());
    }
}
