//! The export dialog's defaults (SDB-66): what the last export chose, offered by the next
//! one. Saved as personal settings (`spelaion-store`'s `ExportPreferences`) only once they
//! pass [`ExportDefaults::check`]; read back leniently, so a value this version cannot use is
//! its default and the dialog always opens.

use spelaion_store::ExportPreferences;

use crate::export::{
    CsvOptions, JsonOptions, KeywordCase, MAX_ROWS_PER_INSERT, NameCase, SqlOptions, TextEncoding,
};
use crate::export_stream::FileNamePattern;

/// A format a file can be exported in (an IN list is only copied).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFileFormat {
    #[default]
    Csv,
    Tsv,
    Json,
    SqlInsert,
    Markdown,
    Html,
    Ndjson,
    Parquet,
    ArrowIpc,
    Xlsx,
}

impl ExportFileFormat {
    fn as_str(self) -> &'static str {
        match self {
            ExportFileFormat::Csv => "csv",
            ExportFileFormat::Tsv => "tsv",
            ExportFileFormat::Json => "json",
            ExportFileFormat::SqlInsert => "sqlInsert",
            ExportFileFormat::Markdown => "markdown",
            ExportFileFormat::Html => "html",
            ExportFileFormat::Ndjson => "ndjson",
            ExportFileFormat::Parquet => "parquet",
            ExportFileFormat::ArrowIpc => "arrowIpc",
            ExportFileFormat::Xlsx => "xlsx",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "csv" => ExportFileFormat::Csv,
            "tsv" => ExportFileFormat::Tsv,
            "json" => ExportFileFormat::Json,
            "sqlInsert" => ExportFileFormat::SqlInsert,
            "markdown" => ExportFileFormat::Markdown,
            "html" => ExportFileFormat::Html,
            "ndjson" => ExportFileFormat::Ndjson,
            "parquet" => ExportFileFormat::Parquet,
            "arrowIpc" => ExportFileFormat::ArrowIpc,
            "xlsx" => ExportFileFormat::Xlsx,
            _ => return None,
        })
    }
}

/// The longest NULL text kept as a default, in bytes.
pub const MAX_EXPORT_NULL_TEXT: usize = spelaion_store::EXPORT_NULL_TEXT_MAX;

/// What the export dialog offers when it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDefaults {
    pub format: ExportFileFormat,
    /// Column names as the first line (CSV and TSV).
    pub header: bool,
    /// CSV's delimiter, the NULL text and formula escaping.
    pub csv: CsvOptions,
    pub encoding: TextEncoding,
    pub gzip: bool,
    pub names: FileNamePattern,
    /// The SQL options as chosen; an upsert is still only written for a table's export.
    pub sql: SqlOptions,
    pub json: JsonOptions,
}

impl Default for ExportDefaults {
    /// A new user's: CSV with a comma and a header, UTF-8 with a byte order mark (so
    /// spreadsheets read accents) and formula escaping on.
    fn default() -> Self {
        Self {
            format: ExportFileFormat::Csv,
            header: true,
            csv: CsvOptions {
                escape_formulas: true,
                ..CsvOptions::default()
            },
            encoding: TextEncoding::Utf8Bom,
            gzip: false,
            names: FileNamePattern::default(),
            sql: SqlOptions::default(),
            json: JsonOptions::default(),
        }
    }
}

impl ExportDefaults {
    /// Options an export would accept: a delimiter that can be read back, a NULL text of one
    /// short line, a file name pattern and timestamp format the core can render, and a number
    /// of rows per `INSERT` within bounds.
    pub fn check(&self) -> Result<(), String> {
        self.csv.check()?;
        if self.csv.null_text.len() > MAX_EXPORT_NULL_TEXT
            || self.csv.null_text.chars().any(char::is_control)
        {
            return Err(format!(
                "the NULL text is one line of at most {MAX_EXPORT_NULL_TEXT} bytes"
            ));
        }
        self.names.check()?;
        self.sql.check()
    }

    /// As the store keeps them.
    pub(crate) fn to_saved(&self) -> ExportPreferences {
        ExportPreferences {
            format: self.format.as_str().into(),
            delimiter: self.csv.delimiter.to_string(),
            header: self.header,
            null_text: self.csv.null_text.clone(),
            encoding: match self.encoding {
                TextEncoding::Utf8 => "utf8",
                TextEncoding::Utf8Bom => "utf8Bom",
                TextEncoding::Utf16Le => "utf16Le",
                TextEncoding::Windows1252 => "windows1252",
            }
            .into(),
            escape_formulas: self.csv.escape_formulas,
            gzip: self.gzip,
            name_pattern: self.names.pattern.clone(),
            timestamp_format: self.names.timestamp_format.clone(),
            // Checked: at most MAX_ROWS_PER_INSERT, which fits.
            rows_per_insert: u32::try_from(self.sql.rows_per_insert).unwrap_or(1),
            omit_schema: self.sql.omit_schema,
            keyword_case: match self.sql.keyword_case {
                KeywordCase::Upper => "upper",
                KeywordCase::Lower => "lower",
            }
            .into(),
            name_case: match self.sql.name_case {
                NameCase::AsIs => "asIs",
                NameCase::Lower => "lower",
                NameCase::Upper => "upper",
            }
            .into(),
            upsert: self.sql.upsert,
            iso_dates: self.json.iso_dates,
            json_as_json: self.json.json_as_json,
        }
    }

    /// The defaults as saved, each part that this version cannot use at its default: an
    /// unknown format or encoding, a delimiter that cannot be read back, a NULL text that is
    /// too long, a file name pattern that does not render, rows per `INSERT` out of bounds.
    pub(crate) fn from_saved(saved: &ExportPreferences) -> Self {
        let defaults = Self::default();
        let one = |text: &str| {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Some(c),
                _ => None,
            }
        };
        let delimiter = one(&saved.delimiter)
            .filter(|&delimiter| {
                CsvOptions {
                    delimiter,
                    ..CsvOptions::default()
                }
                .check()
                .is_ok()
            })
            .unwrap_or(defaults.csv.delimiter);
        let null_text = if saved.null_text.len() <= MAX_EXPORT_NULL_TEXT
            && !saved.null_text.chars().any(char::is_control)
        {
            saved.null_text.clone()
        } else {
            defaults.csv.null_text.clone()
        };
        let names = FileNamePattern {
            pattern: saved.name_pattern.clone(),
            timestamp_format: saved.timestamp_format.clone(),
        };
        let names = if names.check().is_ok() {
            names
        } else {
            defaults.names.clone()
        };
        let rows_per_insert = usize::try_from(saved.rows_per_insert)
            .ok()
            .filter(|rows| (1..=MAX_ROWS_PER_INSERT).contains(rows))
            .unwrap_or(defaults.sql.rows_per_insert);
        let read = Self {
            format: ExportFileFormat::parse(&saved.format).unwrap_or(defaults.format),
            header: saved.header,
            csv: CsvOptions {
                delimiter,
                null_text,
                escape_formulas: saved.escape_formulas,
            },
            encoding: match saved.encoding.as_str() {
                "utf8" => TextEncoding::Utf8,
                "utf8Bom" => TextEncoding::Utf8Bom,
                "utf16Le" => TextEncoding::Utf16Le,
                "windows1252" => TextEncoding::Windows1252,
                _ => defaults.encoding,
            },
            gzip: saved.gzip,
            names,
            sql: SqlOptions {
                rows_per_insert,
                omit_schema: saved.omit_schema,
                keyword_case: match saved.keyword_case.as_str() {
                    "lower" => KeywordCase::Lower,
                    _ => KeywordCase::Upper,
                },
                name_case: match saved.name_case.as_str() {
                    "lower" => NameCase::Lower,
                    "upper" => NameCase::Upper,
                    _ => NameCase::AsIs,
                },
                upsert: saved.upsert,
            },
            json: JsonOptions {
                iso_dates: saved.iso_dates,
                json_as_json: saved.json_as_json,
            },
        };
        if read.check().is_ok() { read } else { defaults }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chosen() -> ExportDefaults {
        ExportDefaults {
            format: ExportFileFormat::SqlInsert,
            header: false,
            csv: CsvOptions {
                delimiter: ';',
                null_text: "NULL".into(),
                escape_formulas: false,
            },
            encoding: TextEncoding::Windows1252,
            gzip: true,
            names: FileNamePattern {
                pattern: "{schema}_{table}_{timestamp}".into(),
                timestamp_format: "%Y%m%d".into(),
            },
            sql: SqlOptions {
                rows_per_insert: 250,
                omit_schema: true,
                keyword_case: KeywordCase::Lower,
                name_case: NameCase::Upper,
                upsert: true,
            },
            json: JsonOptions {
                iso_dates: true,
                json_as_json: false,
            },
        }
    }

    #[test]
    fn a_new_user_escapes_formulas() {
        let defaults = ExportDefaults::default();
        assert!(defaults.csv.escape_formulas);
        assert_eq!(defaults.encoding, TextEncoding::Utf8Bom);
        assert_eq!(defaults.format, ExportFileFormat::Csv);
        assert!(defaults.check().is_ok());
        // The store's defaults read as the same.
        assert_eq!(
            ExportDefaults::from_saved(&ExportPreferences::default()),
            defaults
        );
        assert_eq!(defaults.to_saved(), ExportPreferences::default());
    }

    #[test]
    fn the_store_keeps_the_same_bounds() {
        assert_eq!(
            spelaion_store::EXPORT_MAX_ROWS_PER_INSERT as usize,
            MAX_ROWS_PER_INSERT
        );
        assert_eq!(
            spelaion_store::EXPORT_PATTERN_MAX,
            crate::export_stream::MAX_NAME_PATTERN
        );
    }

    #[test]
    fn every_value_goes_through_the_store_and_back() {
        let chosen = chosen();
        assert!(chosen.check().is_ok());
        let saved = chosen.to_saved();
        assert!(saved.validate().is_ok());
        assert_eq!(ExportDefaults::from_saved(&saved), chosen);
        for format in spelaion_store::EXPORT_FORMATS {
            let parsed = ExportFileFormat::parse(format).unwrap();
            assert_eq!(parsed.as_str(), format);
        }
        for encoding in [
            TextEncoding::Utf8,
            TextEncoding::Utf8Bom,
            TextEncoding::Utf16Le,
            TextEncoding::Windows1252,
        ] {
            let defaults = ExportDefaults {
                encoding,
                ..chosen.clone()
            };
            let saved = defaults.to_saved();
            assert!(spelaion_store::EXPORT_ENCODINGS.contains(&saved.encoding.as_str()));
            assert_eq!(ExportDefaults::from_saved(&saved), defaults);
        }
        for (keyword_case, name_case) in [
            (KeywordCase::Upper, NameCase::AsIs),
            (KeywordCase::Lower, NameCase::Lower),
            (KeywordCase::Upper, NameCase::Upper),
        ] {
            let defaults = ExportDefaults {
                sql: SqlOptions {
                    keyword_case,
                    name_case,
                    ..chosen.sql.clone()
                },
                ..chosen.clone()
            };
            assert_eq!(ExportDefaults::from_saved(&defaults.to_saved()), defaults);
        }
    }

    #[test]
    fn options_an_export_would_refuse_are_not_defaults() {
        let chosen = chosen();
        for refused in [
            ExportDefaults {
                csv: CsvOptions {
                    delimiter: '"',
                    ..chosen.csv.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                csv: CsvOptions {
                    delimiter: 'a',
                    ..chosen.csv.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                csv: CsvOptions {
                    null_text: "x".repeat(MAX_EXPORT_NULL_TEXT + 1),
                    ..chosen.csv.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                csv: CsvOptions {
                    null_text: "a\nb".into(),
                    ..chosen.csv.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                names: FileNamePattern {
                    pattern: "{owner}".into(),
                    ..chosen.names.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                names: FileNamePattern {
                    timestamp_format: "%Q".into(),
                    ..chosen.names.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                sql: SqlOptions {
                    rows_per_insert: 0,
                    ..chosen.sql.clone()
                },
                ..chosen.clone()
            },
            ExportDefaults {
                sql: SqlOptions {
                    rows_per_insert: MAX_ROWS_PER_INSERT + 1,
                    ..chosen.sql.clone()
                },
                ..chosen.clone()
            },
        ] {
            assert!(refused.check().is_err(), "{refused:?}");
        }
    }

    #[test]
    fn what_was_saved_is_read_leniently() {
        let chosen = chosen();
        let saved = chosen.to_saved();
        // Each part this version cannot use is its default; the rest is kept.
        let odd = ExportPreferences {
            format: "avro".into(),
            delimiter: "\"".into(),
            encoding: "ebcdic".into(),
            keyword_case: "title".into(),
            name_case: "snake".into(),
            rows_per_insert: 0,
            ..saved.clone()
        };
        let defaults = ExportDefaults::default();
        assert_eq!(
            ExportDefaults::from_saved(&odd),
            ExportDefaults {
                format: defaults.format,
                csv: CsvOptions {
                    delimiter: defaults.csv.delimiter,
                    ..chosen.csv.clone()
                },
                encoding: defaults.encoding,
                sql: SqlOptions {
                    rows_per_insert: defaults.sql.rows_per_insert,
                    keyword_case: KeywordCase::Upper,
                    name_case: NameCase::AsIs,
                    ..chosen.sql.clone()
                },
                ..chosen.clone()
            }
        );
        // A pattern that no longer renders is the default pattern, with its timestamp format.
        let pattern = ExportPreferences {
            name_pattern: "{owner}_{table}".into(),
            ..saved.clone()
        };
        assert_eq!(
            ExportDefaults::from_saved(&pattern).names,
            FileNamePattern::default()
        );
        // A NULL text with a line break, or too long, is empty again.
        for null_text in ["a\nb".to_string(), "x".repeat(MAX_EXPORT_NULL_TEXT + 1)] {
            let odd = ExportPreferences {
                null_text,
                ..saved.clone()
            };
            assert_eq!(ExportDefaults::from_saved(&odd).csv.null_text, "");
        }
    }
}
