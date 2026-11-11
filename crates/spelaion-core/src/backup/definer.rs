//! "Remove DEFINER" for `mysqldump`, which has no such option (ADR-049): `DEFINER=user@host`
//! clauses are removed from each line of the dump that is not a row of data, so routines, views,
//! triggers and events restore under whichever account loads the file.

use std::borrow::Cow;

/// The line without its `DEFINER=…` clauses (and the space after each). Rows of data
/// (`INSERT INTO …`) are left alone: their values may hold the same text.
pub(crate) fn strip_definer(line: &[u8]) -> Cow<'_, [u8]> {
    const KEY: &[u8] = b"DEFINER=";
    if line.starts_with(b"INSERT INTO") || !contains(line, KEY) {
        return Cow::Borrowed(line);
    }
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line[i..].starts_with(KEY)
            && let Some(end) = account_end(line, i + KEY.len())
        {
            i = end;
            if line.get(i) == Some(&b' ') {
                i += 1;
            }
            continue;
        }
        out.push(line[i]);
        i += 1;
    }
    Cow::Owned(out)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Where `user@host` starting at `start` ends, or `None` if it is not one.
fn account_end(line: &[u8], start: usize) -> Option<usize> {
    let user_end = name_end(line, start)?;
    if line.get(user_end) != Some(&b'@') {
        // `DEFINER=CURRENT_USER` and friends have no host.
        let word: Vec<u8> = line[start..user_end].to_ascii_uppercase();
        return (word.starts_with(b"CURRENT_USER")).then_some(user_end);
    }
    name_end(line, user_end + 1)
}

/// The end of a quoted (`` `…` ``, `'…'`, `"…"`, the quote doubled inside) or bare name.
fn name_end(line: &[u8], start: usize) -> Option<usize> {
    let first = *line.get(start)?;
    if matches!(first, b'`' | b'\'' | b'"') {
        let mut i = start + 1;
        while i < line.len() {
            if line[i] == first {
                if line.get(i + 1) == Some(&first) {
                    i += 2;
                    continue;
                }
                return Some(i + 1);
            }
            i += 1;
        }
        return None;
    }
    let mut i = start;
    while i < line.len() && !matches!(line[i], b'@' | b' ' | b'*' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    (i > start).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        String::from_utf8(strip_definer(s.as_bytes()).into_owned()).unwrap()
    }

    #[test]
    fn definers_go_from_definitions_only() {
        assert_eq!(
            strip("/*!50013 DEFINER=`root`@`%` SQL SECURITY DEFINER */\n"),
            "/*!50013 SQL SECURITY DEFINER */\n"
        );
        assert_eq!(
            strip("CREATE DEFINER=`app``s`@`10.0.0.%` PROCEDURE `p`()\n"),
            "CREATE PROCEDURE `p`()\n"
        );
        assert_eq!(
            strip("/*!50003 CREATE*/ /*!50017 DEFINER=`root`@`localhost`*/ /*!50003 TRIGGER t"),
            "/*!50003 CREATE*/ /*!50017 */ /*!50003 TRIGGER t"
        );
        assert_eq!(strip("CREATE DEFINER='u'@'h' EVENT e"), "CREATE EVENT e");
        assert_eq!(strip("CREATE DEFINER=CURRENT_USER VIEW v"), "CREATE VIEW v");
        let row = "INSERT INTO `t` VALUES ('DEFINER=`a`@`b` ');\n";
        assert_eq!(strip(row), row);
        assert_eq!(strip("SELECT 1;\n"), "SELECT 1;\n");
        // Not an account: left as it is.
        assert_eq!(strip("-- DEFINER=`open"), "-- DEFINER=`open");
    }
}
