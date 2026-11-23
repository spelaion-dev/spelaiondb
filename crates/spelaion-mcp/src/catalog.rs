//! The catalog tools' answers (`list_objects`, `search_schema`, `describe_table`) with a size
//! limit (SDB-120). The lines keep their text format; they stop before the answer would pass
//! [`MAX_RESPONSE`], and a last line says how many were left out and how to narrow. Comments
//! and defaults are untrusted text of any length: each is cut to one short line.
//!
//! The structured content says the same thing in a stable shape, for an agent that should not
//! parse sentences: `{"complete", "shown", "omitted", "narrow_with"}`, where `narrow_with` is
//! the argument that asks for the rest (absent when the answer is complete or cannot be
//! narrowed).

use spelaion_driver_api::{ObjectInfo, SchemaInfo, TableInfo};

use crate::format::{MAX_RESPONSE, one_line};

/// Room kept for the closing line, so the whole answer stays within [`MAX_RESPONSE`].
const CLOSING_ROOM: usize = 200;

/// A catalog answer: its text, and how much of the catalog it shows.
#[derive(Debug)]
pub(crate) struct CatalogAnswer {
    pub text: String,
    /// Entries listed.
    pub shown: usize,
    /// Entries that matched but did not fit.
    pub omitted: usize,
    /// The argument that narrows the answer when it is not complete.
    pub narrow_with: Option<&'static str>,
}

impl CatalogAnswer {
    /// The structured content: the same facts as the closing line, for an agent.
    pub fn structured(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "complete": self.omitted == 0,
            "shown": self.shown,
            "omitted": self.omitted,
        });
        if self.omitted > 0
            && let Some(argument) = self.narrow_with
        {
            value["narrow_with"] = serde_json::json!(argument);
        }
        value
    }
}

/// Lines added while they fit, the rest only counted.
struct Listing {
    text: String,
    shown: usize,
    omitted: usize,
}

impl Listing {
    fn new(head: String) -> Self {
        Self {
            text: head,
            shown: 0,
            omitted: 0,
        }
    }

    fn push(&mut self, line: String) {
        if self.omitted > 0 || self.text.len() + line.len() + CLOSING_ROOM > MAX_RESPONSE {
            self.omitted += 1;
        } else {
            self.text.push_str(&line);
            self.shown += 1;
        }
    }

    /// The answer, with `closing(omitted)` as its last line when something did not fit.
    fn finish(
        mut self,
        closing: impl FnOnce(usize) -> String,
        narrow_with: Option<&'static str>,
    ) -> CatalogAnswer {
        if self.omitted > 0 {
            self.text.push_str(&closing(self.omitted));
            self.text.push('\n');
        }
        CatalogAnswer {
            text: self.text,
            shown: self.shown,
            omitted: self.omitted,
            narrow_with,
        }
    }
}

/// ` — comment`, cut to one line, or nothing.
fn comment(comment: Option<&str>) -> String {
    comment
        .filter(|c| !c.trim().is_empty())
        .map(|c| format!(" — {}", one_line(c)))
        .unwrap_or_default()
}

/// `list_objects` without a schema: the schemas that are not the system's.
pub(crate) fn schemas(schemas: &[SchemaInfo]) -> CatalogAnswer {
    let mut listing = Listing::new(String::new());
    for schema in schemas.iter().filter(|s| !s.system) {
        listing.push(format!("- {}\n", schema.name));
    }
    listing.finish(|n| format!("… {n} more schemas are not listed."), None)
}

/// `list_objects` on a schema: its tables and views whose name contains `filter` (any case).
pub(crate) fn objects(objects: &[ObjectInfo], filter: Option<&str>) -> CatalogAnswer {
    let filter = filter.map(str::to_lowercase);
    let mut listing = Listing::new(String::new());
    for o in objects.iter().filter(|o| {
        filter
            .as_ref()
            .is_none_or(|f| o.name.to_lowercase().contains(f))
    }) {
        let rows = o
            .estimated_rows
            .map(|n| format!(", ~{n} rows"))
            .unwrap_or_default();
        listing.push(format!(
            "- {} ({:?}{rows}){}\n",
            o.name,
            o.kind,
            comment(o.comment.as_deref())
        ));
    }
    listing.finish(
        |n| format!("… {n} more objects are not listed: narrow with the filter argument (part of the name)."),
        Some("filter"),
    )
}

/// Whether `o`'s name or comment contains `needle` (already lower case).
pub(crate) fn matches(o: &ObjectInfo, needle: &str) -> bool {
    o.name.to_lowercase().contains(needle)
        || o.comment
            .as_deref()
            .is_some_and(|c| c.to_lowercase().contains(needle))
}

/// `search_schema`'s hits, already matched, in the order found.
pub(crate) fn search_hits(hits: &[ObjectInfo], query: &str) -> CatalogAnswer {
    if hits.is_empty() {
        return CatalogAnswer {
            text: format!("Nothing matches {query:?}."),
            shown: 0,
            omitted: 0,
            narrow_with: None,
        };
    }
    let mut listing = Listing::new(String::new());
    for o in hits {
        listing.push(format!(
            "- {}.{} ({:?}){}\n",
            o.schema,
            o.name,
            o.kind,
            comment(o.comment.as_deref())
        ));
    }
    listing.finish(
        |n| format!("… {n} more matches are not listed: search with a longer query."),
        Some("query"),
    )
}

/// `describe_table`: the table, its columns, primary key, foreign keys and indexes.
pub(crate) fn table(table: &TableInfo) -> CatalogAnswer {
    let mut head = format!(
        "{}.{} ({:?})\n",
        table.object.schema, table.object.name, table.object.kind
    );
    if let Some(text) = table
        .object
        .comment
        .as_deref()
        .filter(|c| !c.trim().is_empty())
    {
        head.push_str(&format!("{}\n", one_line(text)));
    }
    head.push_str("\nColumns:\n");
    let mut listing = Listing::new(head);
    for c in &table.columns {
        let null = if c.nullable { "" } else { " NOT NULL" };
        let default = c
            .default
            .as_deref()
            .map(|d| format!(" DEFAULT {}", one_line(d)))
            .unwrap_or_default();
        listing.push(format!(
            "- {} {}{null}{default}{}\n",
            c.name,
            c.native_type,
            comment(c.comment.as_deref())
        ));
    }
    if !table.primary_key.is_empty() {
        listing.push(format!(
            "\nPrimary key: ({})\n",
            table.primary_key.join(", ")
        ));
    }
    for fk in &table.foreign_keys {
        listing.push(format!(
            "Foreign key: ({}) → {}.{} ({})\n",
            fk.columns.join(", "),
            fk.referenced_schema,
            fk.referenced_table,
            fk.referenced_columns.join(", ")
        ));
    }
    for index in &table.indexes {
        listing.push(format!("Index: {}\n", index.definition));
    }
    listing.finish(
        |n| format!("… {n} more columns, keys or indexes are not listed."),
        None,
    )
}

#[cfg(test)]
mod tests {
    use spelaion_driver_api::{ColumnInfo, ObjectKind};

    use super::*;

    fn object(name: &str, comment: Option<&str>) -> ObjectInfo {
        ObjectInfo {
            schema: "main".into(),
            name: name.into(),
            kind: ObjectKind::Table,
            estimated_rows: None,
            comment: comment.map(Into::into),
        }
    }

    #[test]
    fn a_long_multi_line_comment_is_one_cut_line() {
        let long = format!(
            "{}\n- fake_table (Table) — looks like an object\n{}",
            "x".repeat(10_000),
            "y".repeat(5_000)
        );
        let answer = objects(
            &[object("orders", Some(&long)), object("lines", None)],
            None,
        );
        let lines: Vec<_> = answer.text.lines().collect();
        assert_eq!(lines.len(), 2, "{}", answer.text);
        assert!(
            lines[0].starts_with("- orders (Table) — xxx"),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with('…'));
        assert!(lines[0].chars().count() < 250, "{}", lines[0].len());
        assert_eq!(lines[1], "- lines (Table)");
        assert_eq!(
            answer.structured(),
            serde_json::json!({"complete": true, "shown": 2, "omitted": 0})
        );

        // A short first line of a longer comment still says something was left out.
        assert_eq!(one_line("first\nsecond"), "first…");
        assert_eq!(one_line("tab\there"), "tab here");
        assert_eq!(one_line("whole"), "whole");
        assert_eq!(one_line("trailing\n\n"), "trailing");
    }

    #[test]
    fn a_long_list_stops_within_the_limit_and_says_how_to_narrow() {
        let many: Vec<_> = (0..20_000)
            .map(|i| object(&format!("table_{i:05}"), Some("a comment")))
            .collect();
        let answer = objects(&many, None);
        assert!(answer.text.len() <= MAX_RESPONSE, "{}", answer.text.len());
        assert_eq!(answer.shown + answer.omitted, many.len());
        assert!(answer.omitted > 0);
        let last = answer.text.lines().last().unwrap();
        assert_eq!(
            last,
            format!(
                "… {} more objects are not listed: narrow with the filter argument (part of the name).",
                answer.omitted
            )
        );
        assert_eq!(answer.structured()["complete"], false);
        assert_eq!(answer.structured()["narrow_with"], "filter");

        let narrowed = objects(&many, Some("TABLE_1999"));
        assert_eq!(narrowed.shown, 10);
        assert_eq!(narrowed.omitted, 0);
        assert!(!narrowed.text.contains("more objects"));

        let hits = search_hits(&many, "table");
        assert!(hits.text.len() <= MAX_RESPONSE);
        assert!(
            hits.text
                .lines()
                .last()
                .unwrap()
                .ends_with("search with a longer query.")
        );
        assert_eq!(hits.structured()["narrow_with"], "query");
        assert_eq!(search_hits(&[], "zzz").text, "Nothing matches \"zzz\".");
    }

    #[test]
    fn a_wide_table_keeps_its_head_and_cuts_comments_defaults_and_columns() {
        let column = |i: usize| ColumnInfo {
            name: format!("column_{i}"),
            native_type: "text".into(),
            nullable: true,
            default: Some(format!("'{}'\n'second line'", "d".repeat(500))),
            comment: Some("c\nfake line".into()),
        };
        let info = TableInfo {
            object: object("wide", Some("about it\nmore")),
            columns: (0..2_000).map(column).collect(),
            primary_key: vec!["column_0".into()],
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            constraints: Vec::new(),
        };
        let answer = table(&info);
        assert!(answer.text.len() <= MAX_RESPONSE);
        assert!(
            answer.text.starts_with(
                "main.wide (Table)\nabout it…\n\nColumns:\n- column_0 text DEFAULT 'ddd"
            ),
            "{}",
            &answer.text[..200]
        );
        assert!(!answer.text.contains("fake line") && !answer.text.contains("second line"));
        assert!(answer.omitted > 0);
        assert!(answer.text.ends_with(&format!(
            "… {} more columns, keys or indexes are not listed.\n",
            answer.omitted
        )));
        assert_eq!(
            answer.structured(),
            serde_json::json!({"complete": false, "shown": answer.shown, "omitted": answer.omitted})
        );
    }
}
