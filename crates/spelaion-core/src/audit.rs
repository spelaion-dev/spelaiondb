//! The MCP activity log as the app shows and exports it (ADR-033): what `spelaiondb mcp`
//! recorded, with secrets masked, as CSV or JSON. The log itself is the store's append-only
//! `audit` table (ADR-008); clearing it is the store's `clear_audit`, which leaves a record.

use std::io::Write;
use std::path::Path;

use spelaion_store::{AuditEntry, Decision};

/// What replaces a secret.
const MASK: &str = "'***'";

/// The words after which a quoted literal is a secret: `PASSWORD 'x'`, `IDENTIFIED BY 'x'`,
/// `SECRET 'x'`…, with an optional `=` between them.
const SECRET_WORDS: [&str; 5] = ["password", "identified by", "secret", "pwd", "token"];

/// `statement` with the secrets it may carry masked: quoted literals after `PASSWORD`,
/// `IDENTIFIED BY`, `SECRET`, `PWD` or `TOKEN`, and the password of URLs (`user:pass@host`).
/// A heuristic: it catches what statements usually carry, not a determined attempt.
pub fn redact_secrets(statement: &str) -> String {
    redact_urls(&redact_literals(statement))
}

fn redact_literals(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    let mut i = 0;
    while i < bytes.len() {
        // Bytes, not `str` slices: `i` may fall inside a multi-byte character.
        let word = SECRET_WORDS.iter().find(|w| {
            lower.as_bytes()[i..].starts_with(w.as_bytes())
                && (i == 0 || !is_word(bytes[i - 1]))
                && !lower
                    .as_bytes()
                    .get(i + w.len())
                    .copied()
                    .is_some_and(is_word)
        });
        let Some(word) = word else {
            i += 1;
            continue;
        };
        // Skip spaces and one `=`, then expect a quote.
        let mut j = i + word.len();
        while j < bytes.len() && (bytes[j] == b' ' || bytes[j] == b'\t' || bytes[j] == b'=') {
            j += 1;
        }
        // A Unicode (`N'…'`, SQL Server) or escape (`E'…'`, PostgreSQL) string's prefix is kept.
        let prefix = bytes.get(j).copied().filter(|b| b"NnEe".contains(b));
        if prefix.is_some() && bytes.get(j + 1) == Some(&b'\'') {
            j += 1;
        }
        let escapes = prefix.is_some_and(|b| b.eq_ignore_ascii_case(&b'e'));
        let Some(&quote) = bytes.get(j).filter(|b| matches!(b, b'\'' | b'"')) else {
            i += word.len();
            continue;
        };
        // The literal ends at the next quote that is not doubled (nor escaped, in `E'…'`).
        let mut k = j + 1;
        while k < bytes.len() {
            if escapes && bytes[k] == b'\\' {
                k += 2;
                continue;
            }
            if bytes[k] == quote {
                if bytes.get(k + 1) == Some(&quote) {
                    k += 2;
                    continue;
                }
                break;
            }
            k += 1;
        }
        out.push_str(&text[copied..j]);
        out.push_str(MASK);
        copied = (k + 1).min(bytes.len());
        i = copied;
    }
    out.push_str(&text[copied..]);
    out
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `scheme://user:password@host` → `scheme://user:***@host`.
fn redact_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '/' | ')'))
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match (authority.rfind('@'), authority.find(':')) {
            (Some(at), Some(colon)) if colon < at => {
                out.push_str(&authority[..=colon]);
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// An entry as the app shows it: secrets masked in its statement and detail.
pub fn redacted(entry: &AuditEntry) -> AuditEntry {
    AuditEntry {
        statement: redact_secrets(&entry.statement),
        detail: entry.detail.as_deref().map(redact_secrets),
        ..entry.clone()
    }
}

/// How an entry ended, in the page's words.
pub fn status(decision: Decision) -> &'static str {
    match decision {
        Decision::Executed => "ok",
        Decision::Failed => "error",
        Decision::Rejected => "refused",
        Decision::Cleared => "cleared",
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn iso_time(at_ms: i64) -> String {
    let secs = at_ms.div_euclid(1000);
    let ms = at_ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let s = secs.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        s / 3600,
        s % 3600 / 60,
        s % 60
    )
}

/// The formats of an export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditFormat {
    Csv,
    Json,
}

/// A CSV field: quoted when it must be, and never read as a formula by a spreadsheet (a
/// statement written by an agent is untrusted).
fn csv_field(text: &str) -> String {
    let guarded = if text.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{text}")
    } else {
        text.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// Writes `entries` (already redacted by the caller or not: they are redacted here) to `out`.
pub fn write_audit(
    entries: &[AuditEntry],
    format: AuditFormat,
    out: &mut impl Write,
) -> std::io::Result<()> {
    match format {
        AuditFormat::Csv => {
            writeln!(
                out,
                "time,session,client,connection,tool,statement,status,rows,duration_ms,detail"
            )?;
            for entry in entries.iter().map(redacted) {
                let fields = [
                    iso_time(entry.at_ms),
                    entry.session.clone().unwrap_or_default(),
                    entry.client.clone(),
                    entry.connection.clone(),
                    entry.tool.clone().unwrap_or_default(),
                    entry.statement.clone(),
                    status(entry.decision).to_string(),
                    entry.rows.map(|n| n.to_string()).unwrap_or_default(),
                    entry.duration_ms.map(|n| n.to_string()).unwrap_or_default(),
                    entry.detail.clone().unwrap_or_default(),
                ];
                let line = fields
                    .iter()
                    .map(|f| csv_field(f))
                    .collect::<Vec<_>>()
                    .join(",");
                writeln!(out, "{line}")?;
            }
        }
        AuditFormat::Json => {
            let items: Vec<serde_json::Value> = entries
                .iter()
                .map(redacted)
                .map(|e| {
                    serde_json::json!({
                        "time": iso_time(e.at_ms),
                        "session": e.session,
                        "client": e.client,
                        "connection": e.connection,
                        "tool": e.tool,
                        "statement": e.statement,
                        "status": status(e.decision),
                        "rows": e.rows,
                        "duration_ms": e.duration_ms,
                        "detail": e.detail,
                    })
                })
                .collect();
            serde_json::to_writer_pretty(&mut *out, &items)?;
            writeln!(out)?;
        }
    }
    Ok(())
}

/// Writes `entries` to the file at `path`, through a file beside it moved over it at the end.
pub fn export_audit(
    entries: &[AuditEntry],
    format: AuditFormat,
    path: &Path,
) -> std::io::Result<()> {
    let mut temp = path.as_os_str().to_owned();
    temp.push(".spelaion-tmp");
    let temp = std::path::PathBuf::from(temp);
    let result = (|| {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&temp)?);
        write_audit(entries, format, &mut file)?;
        file.flush()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_in_statements_are_masked() {
        for (sql, expect) in [
            (
                "alter user bob password 'hunter2'",
                "alter user bob password '***'",
            ),
            (
                "CREATE USER x IDENTIFIED BY \"s3cr3t\"",
                "CREATE USER x IDENTIFIED BY '***'",
            ),
            (
                "create user y with password = 'it''s'",
                "create user y with password = '***'",
            ),
            (
                "select * from dblink('postgres://admin:topsecret@db/prod', 'select 1')",
                "select * from dblink('postgres://admin:***@db/prod', 'select 1')",
            ),
            (
                "create secret s (token 'abc')",
                "create secret s (token '***')",
            ),
            (
                "insert into t (passwords) values ('kept')",
                "insert into t (passwords) values ('kept')",
            ),
            ("select password from users", "select password from users"),
            ("select 'x' -- nothing", "select 'x' -- nothing"),
            ("password '", "password '***'"),
            (
                "CREATE LOGIN [a] WITH PASSWORD = N'Top''secret', CHECK_POLICY = OFF",
                "CREATE LOGIN [a] WITH PASSWORD = N'***', CHECK_POLICY = OFF",
            ),
            (
                r"ALTER ROLE a PASSWORD E'it\'s secret' VALID UNTIL 'infinity'",
                "ALTER ROLE a PASSWORD E'***' VALID UNTIL 'infinity'",
            ),
            ("password = name", "password = name"),
            ("password = n", "password = n"),
            (
                "select 'ñandú' where pwd = 'clave'",
                "select 'ñandú' where pwd = '***'",
            ),
        ] {
            assert_eq!(redact_secrets(sql), expect, "{sql}");
        }
    }

    fn entry(statement: &str, decision: Decision) -> AuditEntry {
        AuditEntry {
            at_ms: 1_762_550_000_123,
            session: Some("s1".into()),
            tool: Some("run_query".into()),
            rows: Some(2),
            duration_ms: Some(5),
            detail: Some("note, with \"quotes\"".into()),
            ..AuditEntry::now("claude-code", "~/shop", statement, decision)
        }
    }

    #[test]
    fn csv_is_quoted_never_a_formula_and_masks_secrets() {
        let mut out = Vec::new();
        write_audit(
            &[
                entry("select 1,\n2", Decision::Executed),
                entry("=HYPERLINK(\"http://x\")", Decision::Rejected),
                entry("alter user a password 'p'", Decision::Failed),
            ],
            AuditFormat::Csv,
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        let mut lines = text.split("\r\n").flat_map(|l| l.split('\n'));
        assert_eq!(
            lines.next(),
            Some("time,session,client,connection,tool,statement,status,rows,duration_ms,detail")
        );
        assert!(
            text.contains("2025-11-07T21:13:20.123Z,s1,claude-code,~/shop,run_query,\"select 1,\n2\",ok,2,5,\"note, with \"\"quotes\"\"\"\n"),
            "{text}"
        );
        assert!(
            text.contains(",\"'=HYPERLINK(\"\"http://x\"\")\",refused,"),
            "{text}"
        );
        assert!(
            text.contains(",alter user a password '***',error,"),
            "{text}"
        );
        assert!(!text.contains("'p'"));
    }

    #[test]
    fn json_export_reads_back() {
        let mut out = Vec::new();
        write_audit(
            &[entry("alter user a password 'p'", Decision::Rejected)],
            AuditFormat::Json,
            &mut out,
        )
        .unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(parsed[0]["statement"], "alter user a password '***'");
        assert_eq!(parsed[0]["status"], "refused");
        assert_eq!(parsed[0]["time"], "2025-11-07T21:13:20.123Z");
        assert_eq!(parsed[0]["tool"], "run_query");
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }
}
