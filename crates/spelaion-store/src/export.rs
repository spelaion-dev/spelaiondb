//! The export dialog's remembered choices (SDB-66): the format, the CSV options, the file's
//! name and gzip, and the SQL and JSON options of the last export, for every workspace in this
//! user directory. Kept as text: each value is checked against its known values and bounds
//! when it is saved and again when it is read, where one that no longer passes reads as its
//! default. The core checks what they mean together (`spelaion-core`'s `ExportDefaults`).

use crate::{Store, StoreError};

/// The formats a file can be exported in, as the app names them.
pub const EXPORT_FORMATS: [&str; 10] = [
    "csv",
    "tsv",
    "json",
    "sqlInsert",
    "markdown",
    "html",
    "ndjson",
    "parquet",
    "arrowIpc",
    "xlsx",
];

/// The encodings of a text file, as the app names them.
pub const EXPORT_ENCODINGS: [&str; 4] = ["utf8Bom", "utf8", "utf16Le", "windows1252"];

/// How SQL keywords are written.
pub const EXPORT_KEYWORD_CASES: [&str; 2] = ["upper", "lower"];

/// How table and column names are written.
pub const EXPORT_NAME_CASES: [&str; 3] = ["asIs", "lower", "upper"];

/// The longest NULL text kept, in bytes.
pub const EXPORT_NULL_TEXT_MAX: usize = 128;

/// The longest file name pattern or timestamp format kept, in characters (the core's
/// `MAX_NAME_PATTERN`).
pub const EXPORT_PATTERN_MAX: usize = 200;

/// The most rows per `INSERT` (the core's `MAX_ROWS_PER_INSERT`).
pub const EXPORT_MAX_ROWS_PER_INSERT: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPreferences {
    /// One of [`EXPORT_FORMATS`].
    pub format: String,
    /// CSV's field separator: one character.
    pub delimiter: String,
    /// Column names as the first line (CSV and TSV).
    pub header: bool,
    /// What a NULL is written as: one line of at most [`EXPORT_NULL_TEXT_MAX`] bytes.
    pub null_text: String,
    /// One of [`EXPORT_ENCODINGS`].
    pub encoding: String,
    /// On for a new user: a cell a spreadsheet would run as a formula gets a leading `'`.
    pub escape_formulas: bool,
    pub gzip: bool,
    /// The file name pattern: one non-empty line of at most [`EXPORT_PATTERN_MAX`] characters.
    pub name_pattern: String,
    /// The `strftime` pattern of `{timestamp}`: one line of at most [`EXPORT_PATTERN_MAX`]
    /// characters.
    pub timestamp_format: String,
    /// From 1 to [`EXPORT_MAX_ROWS_PER_INSERT`].
    pub rows_per_insert: u32,
    pub omit_schema: bool,
    /// One of [`EXPORT_KEYWORD_CASES`].
    pub keyword_case: String,
    /// One of [`EXPORT_NAME_CASES`].
    pub name_case: String,
    pub upsert: bool,
    pub iso_dates: bool,
    pub json_as_json: bool,
}

impl Default for ExportPreferences {
    /// What the dialog offered before it remembered anything: CSV with a comma and a header,
    /// UTF-8 with a byte order mark, formula escaping on, files named after their table.
    fn default() -> Self {
        Self {
            format: "csv".into(),
            delimiter: ",".into(),
            header: true,
            null_text: String::new(),
            encoding: "utf8Bom".into(),
            escape_formulas: true,
            gzip: false,
            name_pattern: "{table}".into(),
            timestamp_format: "%Y%m%d-%H%M%S".into(),
            rows_per_insert: 1,
            omit_schema: false,
            keyword_case: "upper".into(),
            name_case: "asIs".into(),
            upsert: false,
            iso_dates: false,
            json_as_json: true,
        }
    }
}

fn one_char(text: &str) -> bool {
    text.chars().count() == 1
}

fn null_text_ok(text: &str) -> bool {
    text.len() <= EXPORT_NULL_TEXT_MAX && !text.chars().any(char::is_control)
}

fn pattern_ok(text: &str) -> bool {
    text.chars().count() <= EXPORT_PATTERN_MAX && !text.chars().any(char::is_control)
}

fn name_pattern_ok(text: &str) -> bool {
    pattern_ok(text) && !text.trim().is_empty()
}

fn rows_ok(rows: u32) -> bool {
    (1..=EXPORT_MAX_ROWS_PER_INSERT).contains(&rows)
}

impl ExportPreferences {
    /// Every value known and within its bounds.
    pub fn validate(&self) -> Result<(), StoreError> {
        let valid = EXPORT_FORMATS.contains(&self.format.as_str())
            && one_char(&self.delimiter)
            && null_text_ok(&self.null_text)
            && EXPORT_ENCODINGS.contains(&self.encoding.as_str())
            && name_pattern_ok(&self.name_pattern)
            && pattern_ok(&self.timestamp_format)
            && rows_ok(self.rows_per_insert)
            && EXPORT_KEYWORD_CASES.contains(&self.keyword_case.as_str())
            && EXPORT_NAME_CASES.contains(&self.name_case.as_str());
        if valid {
            Ok(())
        } else {
            Err(StoreError::new("invalid export preferences".into()))
        }
    }
}

impl Store {
    /// The export dialog's last choices, each at its default until it is saved; a saved value
    /// that is not known or not within its bounds reads as its default.
    pub fn get_export_preferences(&self) -> Result<ExportPreferences, StoreError> {
        let defaults = ExportPreferences::default();
        let text = |key: &str, ok: &dyn Fn(&str) -> bool, default: String| {
            Ok::<_, StoreError>(
                self.personal_setting(key)?
                    .filter(|value| ok(value))
                    .unwrap_or(default),
            )
        };
        let known = |list: &'static [&'static str]| move |value: &str| list.contains(&value);
        Ok(ExportPreferences {
            format: text("export.format", &known(&EXPORT_FORMATS), defaults.format)?,
            delimiter: text("export.delimiter", &one_char, defaults.delimiter)?,
            header: self.get_flag("export.header", defaults.header)?,
            null_text: text("export.null_text", &null_text_ok, defaults.null_text)?,
            encoding: text(
                "export.encoding",
                &known(&EXPORT_ENCODINGS),
                defaults.encoding,
            )?,
            escape_formulas: self.get_flag("export.escape_formulas", defaults.escape_formulas)?,
            gzip: self.get_flag("export.gzip", defaults.gzip)?,
            name_pattern: text(
                "export.name_pattern",
                &name_pattern_ok,
                defaults.name_pattern,
            )?,
            timestamp_format: text(
                "export.timestamp_format",
                &pattern_ok,
                defaults.timestamp_format,
            )?,
            rows_per_insert: self
                .personal_setting("export.sql.rows_per_insert")?
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|rows| rows_ok(*rows))
                .unwrap_or(defaults.rows_per_insert),
            omit_schema: self.get_flag("export.sql.omit_schema", defaults.omit_schema)?,
            keyword_case: text(
                "export.sql.keyword_case",
                &known(&EXPORT_KEYWORD_CASES),
                defaults.keyword_case,
            )?,
            name_case: text(
                "export.sql.name_case",
                &known(&EXPORT_NAME_CASES),
                defaults.name_case,
            )?,
            upsert: self.get_flag("export.sql.upsert", defaults.upsert)?,
            iso_dates: self.get_flag("export.json.iso_dates", defaults.iso_dates)?,
            json_as_json: self.get_flag("export.json.json_as_json", defaults.json_as_json)?,
        })
    }

    /// Saves the export dialog's choices, all of them or nothing (the core has checked what
    /// they mean together).
    pub fn set_export_preferences(
        &self,
        preferences: &ExportPreferences,
    ) -> Result<(), StoreError> {
        preferences.validate()?;
        let transaction = self.conn.unchecked_transaction()?;
        self.set_personal_setting("export.format", &preferences.format)?;
        self.set_personal_setting("export.delimiter", &preferences.delimiter)?;
        self.set_flag("export.header", preferences.header)?;
        self.set_personal_setting("export.null_text", &preferences.null_text)?;
        self.set_personal_setting("export.encoding", &preferences.encoding)?;
        self.set_flag("export.escape_formulas", preferences.escape_formulas)?;
        self.set_flag("export.gzip", preferences.gzip)?;
        self.set_personal_setting("export.name_pattern", &preferences.name_pattern)?;
        self.set_personal_setting("export.timestamp_format", &preferences.timestamp_format)?;
        self.set_personal_setting(
            "export.sql.rows_per_insert",
            &preferences.rows_per_insert.to_string(),
        )?;
        self.set_flag("export.sql.omit_schema", preferences.omit_schema)?;
        self.set_personal_setting("export.sql.keyword_case", &preferences.keyword_case)?;
        self.set_personal_setting("export.sql.name_case", &preferences.name_case)?;
        self.set_flag("export.sql.upsert", preferences.upsert)?;
        self.set_flag("export.json.iso_dates", preferences.iso_dates)?;
        self.set_flag("export.json.json_as_json", preferences.json_as_json)?;
        transaction.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "spelaion-export-prefs-{name}-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    /// Every field changed from its default.
    fn changed() -> ExportPreferences {
        ExportPreferences {
            format: "sqlInsert".into(),
            delimiter: ";".into(),
            header: false,
            null_text: "NULL".into(),
            encoding: "windows1252".into(),
            escape_formulas: false,
            gzip: true,
            name_pattern: "{schema}_{table}_{date}".into(),
            timestamp_format: "%Y%m%d".into(),
            rows_per_insert: 500,
            omit_schema: true,
            keyword_case: "lower".into(),
            name_case: "upper".into(),
            upsert: true,
            iso_dates: true,
            json_as_json: false,
        }
    }

    #[test]
    fn export_choices_start_at_their_defaults_and_survive_a_reopen() {
        let path = temp("persist");
        let store = Store::open(&path).unwrap();
        let defaults = store.get_export_preferences().unwrap();
        assert_eq!(defaults, ExportPreferences::default());
        // A new user escapes formulas and gets a byte order mark.
        assert!(defaults.escape_formulas);
        assert_eq!(defaults.encoding, "utf8Bom");
        let changed = changed();
        store.set_export_preferences(&changed).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.get_export_preferences().unwrap(), changed);
        // A tab delimiter and every known value of each list are kept.
        for format in EXPORT_FORMATS {
            for encoding in EXPORT_ENCODINGS {
                let chosen = ExportPreferences {
                    format: format.into(),
                    encoding: encoding.into(),
                    delimiter: "\t".into(),
                    ..changed.clone()
                };
                reopened.set_export_preferences(&chosen).unwrap();
                assert_eq!(reopened.get_export_preferences().unwrap(), chosen);
            }
        }
        drop(reopened);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn export_choices_that_are_unknown_or_unbounded_are_refused() {
        let path = temp("refused");
        let store = Store::open(&path).unwrap();
        let changed = changed();
        store.set_export_preferences(&changed).unwrap();
        let refused = [
            ExportPreferences {
                format: "inList".into(),
                ..changed.clone()
            },
            ExportPreferences {
                format: "CSV".into(),
                ..changed.clone()
            },
            ExportPreferences {
                delimiter: String::new(),
                ..changed.clone()
            },
            ExportPreferences {
                delimiter: ";;".into(),
                ..changed.clone()
            },
            ExportPreferences {
                null_text: "a\nb".into(),
                ..changed.clone()
            },
            ExportPreferences {
                null_text: "x".repeat(EXPORT_NULL_TEXT_MAX + 1),
                ..changed.clone()
            },
            ExportPreferences {
                encoding: "latin1".into(),
                ..changed.clone()
            },
            ExportPreferences {
                name_pattern: "  ".into(),
                ..changed.clone()
            },
            ExportPreferences {
                name_pattern: "x".repeat(EXPORT_PATTERN_MAX + 1),
                ..changed.clone()
            },
            ExportPreferences {
                timestamp_format: "%Y\r%m".into(),
                ..changed.clone()
            },
            ExportPreferences {
                rows_per_insert: 0,
                ..changed.clone()
            },
            ExportPreferences {
                rows_per_insert: EXPORT_MAX_ROWS_PER_INSERT + 1,
                ..changed.clone()
            },
            ExportPreferences {
                keyword_case: "title".into(),
                ..changed.clone()
            },
            ExportPreferences {
                name_case: "camel".into(),
                ..changed.clone()
            },
        ];
        for refused in refused {
            assert!(
                store.set_export_preferences(&refused).is_err(),
                "{refused:?}"
            );
        }
        // Nothing of a refused save was kept.
        assert_eq!(store.get_export_preferences().unwrap(), changed);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_stored_export_choice_that_is_no_longer_valid_reads_as_its_default() {
        let path = temp("lenient");
        let store = Store::open(&path).unwrap();
        let changed = changed();
        store.set_export_preferences(&changed).unwrap();
        // Values written by another version, or by hand, that this one does not know.
        for (key, value) in [
            ("export.format", "avro"),
            ("export.delimiter", ";;"),
            ("export.null_text", "a\u{0}b"),
            ("export.encoding", "ebcdic"),
            ("export.name_pattern", ""),
            ("export.sql.rows_per_insert", "100000"),
            ("export.sql.keyword_case", "UPPER"),
            ("export.sql.name_case", "snake"),
            ("export.escape_formulas", "maybe"),
        ] {
            store.set_personal_setting(key, value).unwrap();
        }
        store
            .set_personal_setting("export.timestamp_format", &"%Y".repeat(101))
            .unwrap();
        let defaults = ExportPreferences::default();
        assert_eq!(
            store.get_export_preferences().unwrap(),
            ExportPreferences {
                format: defaults.format,
                delimiter: defaults.delimiter,
                null_text: defaults.null_text,
                encoding: defaults.encoding,
                name_pattern: defaults.name_pattern,
                timestamp_format: defaults.timestamp_format,
                rows_per_insert: defaults.rows_per_insert,
                keyword_case: defaults.keyword_case,
                name_case: defaults.name_case,
                // A damaged flag reads as the default: escaping stays on.
                escape_formulas: true,
                ..changed
            }
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }
}
