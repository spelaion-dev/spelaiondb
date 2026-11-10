//! A diagram's model as text other tools read (ADR-046): Mermaid's `erDiagram` and DBML.
//!
//! Both are written from the whole model (every column, type and comment, whatever the display
//! options) and must parse with their own tools: `apps/desktop/scripts/er-text.test.mjs` runs
//! Mermaid's and DBML's parsers on the files `tests/er_diagram.rs` writes from an awkward
//! schema (spaces, quotes, keywords, non-Latin names, composite and self-referencing keys).
//! Where a format cannot hold a character, it is replaced, never dropped silently:
//! - Mermaid attribute names and types are single words (`[A-Za-z_]`, digits, `-`, `()`, `[]`
//!   and letters of any script, not starting with a digit, and not `PK`, `FK` or `UK`); other
//!   characters become `_`, and an attribute whose name changed keeps the original in its
//!   comment. Quoted names and comments cannot hold `"`, which becomes `'`.
//! - DBML quoted names cannot hold `"` (it becomes `'`); notes are single-quoted strings where
//!   only `\'` is an escape (a note ending in a backslash gets a space after it). A table with
//!   no columns cannot be written: a `//` line says so, and keys to or from it are left out.
//!   Two keys with the same columns are written once.

use std::collections::HashSet;
use std::fmt::Write as _;

use super::layout::display_name;
use super::{Cardinality, ErModel, ErTable};

/// `text` on one line, with `"` (which neither format can escape in a quoted name) as `'`.
fn quoted_text(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' => '\'',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// A Mermaid attribute word (a column's name or type).
fn mermaid_word(text: &str) -> String {
    let mut word: String = text
        .trim()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '(' | ')' | '[' | ']') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if !word
        .chars()
        .next()
        .is_some_and(|c| c.is_alphabetic() || c == '_')
    {
        word.insert(0, '_');
    }
    if ["PK", "FK", "UK"]
        .iter()
        .any(|key| word.eq_ignore_ascii_case(key))
    {
        word.push('_');
    }
    word
}

fn mermaid_entity(model: &ErModel, table: &ErTable) -> String {
    let name = quoted_text(&display_name(model, table));
    format!("\"{}\"", if name.is_empty() { "_" } else { &name })
}

/// The model as a Mermaid `erDiagram`.
pub fn mermaid(model: &ErModel) -> String {
    let mut out = String::from("erDiagram\n");
    for table in &model.tables {
        let entity = mermaid_entity(model, table);
        if let Some(comment) = &table.comment {
            let _ = writeln!(
                out,
                "    %% {}: {}",
                quoted_text(&display_name(model, table)),
                quoted_text(comment)
            );
        }
        let _ = writeln!(out, "    {entity} {{");
        for column in &table.columns {
            let name = mermaid_word(&column.name);
            let mut line = format!("        {} {name}", mermaid_word(&column.native_type));
            let keys: Vec<&str> = [
                (table.primary_key.contains(&column.name), "PK"),
                (table.foreign_key_columns.contains(&column.name), "FK"),
                (
                    !table.primary_key.contains(&column.name)
                        && table.unique.iter().any(|key| key.contains(&column.name)),
                    "UK",
                ),
            ]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, key)| *key)
            .collect();
            if !keys.is_empty() {
                let _ = write!(line, " {}", keys.join(", "));
            }
            let original = (name != column.name).then(|| quoted_text(&column.name));
            let comment = match (original, column.comment.as_deref().map(quoted_text)) {
                (Some(original), Some(comment)) => Some(format!("{original}: {comment}")),
                (Some(text), None) | (None, Some(text)) => Some(text),
                (None, None) => None,
            };
            if let Some(comment) = comment.filter(|c| !c.is_empty()) {
                let _ = write!(line, " \"{comment}\"");
            }
            out.push_str(&line);
            out.push('\n');
        }
        out.push_str("    }\n");
    }
    for relation in &model.relations {
        let parent = mermaid_entity(model, &model.tables[relation.to]);
        let child = mermaid_entity(model, &model.tables[relation.from]);
        let parent_end = match relation.to_cardinality {
            Cardinality::One => "||",
            Cardinality::ZeroOrOne | Cardinality::ZeroOrMany => "|o",
        };
        let child_end = match relation.from_cardinality {
            Cardinality::ZeroOrOne | Cardinality::One => "o|",
            Cardinality::ZeroOrMany => "o{",
        };
        let line = if relation.identifying { "--" } else { ".." };
        let _ = writeln!(
            out,
            "    {parent} {parent_end}{line}{child_end} {child} : \"{}\"",
            quoted_text(&relation.name)
        );
    }
    out
}

/// A DBML quoted identifier.
fn dbml_name(name: &str) -> String {
    format!("\"{}\"", quoted_text(name))
}

/// A DBML single-quoted string, as DBML's parser reads one: `\'` is a quote, any other
/// backslash is itself and a line break stays a line break. A string cannot end in a
/// backslash (it would escape the closing quote), so a space follows one there.
fn dbml_note(text: &str) -> String {
    let mut out = String::from("'");
    for c in text.chars() {
        match c {
            '\'' => out.push_str("\\'"),
            '\r' => {}
            '\n' => out.push('\n'),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    if out.ends_with('\\') {
        out.push(' ');
    }
    out.push('\'');
    out
}

fn dbml_table(model: &ErModel, table: &ErTable) -> String {
    if model.single_schema() || table.schema.is_empty() {
        dbml_name(&table.name)
    } else {
        format!("{}.{}", dbml_name(&table.schema), dbml_name(&table.name))
    }
}

fn dbml_columns(columns: &[String]) -> String {
    match columns {
        [one] => dbml_name(one),
        many => format!(
            "({})",
            many.iter()
                .map(|c| dbml_name(c))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The model as DBML.
pub fn dbml(model: &ErModel) -> String {
    let mut out = String::new();
    for table in &model.tables {
        let name = dbml_table(model, table);
        if table.columns.is_empty() {
            let _ = writeln!(out, "// {name} has no columns\n");
            continue;
        }
        match &table.comment {
            Some(comment) => {
                let _ = writeln!(out, "Table {name} [note: {}] {{", dbml_note(comment));
            }
            None => {
                let _ = writeln!(out, "Table {name} {{");
            }
        }
        let single_pk = table.primary_key.len() == 1;
        for column in &table.columns {
            let native: String = column.native_type.chars().filter(|&c| c != '"').collect();
            let native = quoted_text(&native);
            let mut settings = Vec::new();
            if single_pk && table.primary_key[0] == column.name {
                settings.push("pk".to_string());
            }
            if !column.nullable {
                settings.push("not null".to_string());
            }
            if table
                .unique
                .iter()
                .any(|key| key.len() == 1 && key[0] == column.name)
            {
                settings.push("unique".to_string());
            }
            if let Some(comment) = &column.comment {
                settings.push(format!("note: {}", dbml_note(comment)));
            }
            let _ = write!(
                out,
                "  {} {}",
                dbml_name(&column.name),
                dbml_name(if native.is_empty() {
                    "unknown"
                } else {
                    &native
                })
            );
            if !settings.is_empty() {
                let _ = write!(out, " [{}]", settings.join(", "));
            }
            out.push('\n');
        }
        let composite: Vec<(&Vec<String>, &str)> = std::iter::once((&table.primary_key, "pk"))
            .filter(|(key, _)| key.len() > 1)
            .chain(
                table
                    .unique
                    .iter()
                    .filter(|key| key.len() > 1)
                    .map(|key| (key, "unique")),
            )
            .collect();
        if !composite.is_empty() {
            out.push_str("  indexes {\n");
            for (key, kind) in composite {
                let columns = key
                    .iter()
                    .map(|c| dbml_name(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = writeln!(out, "    ({columns}) [{kind}]");
            }
            out.push_str("  }\n");
        }
        out.push_str("}\n\n");
    }
    let mut written = HashSet::new();
    let mut names = HashSet::new();
    for relation in &model.relations {
        let (from, to) = (&model.tables[relation.from], &model.tables[relation.to]);
        let ends_exist = |table: &ErTable, columns: &[String]| {
            !columns.is_empty() && columns.iter().all(|c| table.column(c).is_some())
        };
        if relation.from_columns.len() != relation.to_columns.len()
            || !ends_exist(from, &relation.from_columns)
            || !ends_exist(to, &relation.to_columns)
        {
            continue;
        }
        let ends = (
            relation.from,
            relation.from_columns.clone(),
            relation.to,
            relation.to_columns.clone(),
        );
        if !written.insert(ends) {
            continue;
        }
        // DBML refuses two references with one name; SQLite names keys per table.
        let base = quoted_text(&relation.name);
        let mut name = base.clone();
        let mut n = 2;
        while !names.insert(name.clone()) {
            name = format!("{base} ({n})");
            n += 1;
        }
        let kind = match relation.from_cardinality {
            Cardinality::ZeroOrOne | Cardinality::One => "-",
            Cardinality::ZeroOrMany => ">",
        };
        let _ = writeln!(
            out,
            "Ref {}: {}.{} {kind} {}.{}",
            dbml_name(&name),
            dbml_table(model, from),
            dbml_columns(&relation.from_columns),
            dbml_table(model, to),
            dbml_columns(&relation.to_columns),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::tests::table;
    use super::*;

    #[test]
    fn mermaid_words_are_single_words() {
        assert_eq!(
            mermaid_word("character varying(80)"),
            "character_varying(80)"
        );
        assert_eq!(mermaid_word("numeric(12,2)"), "numeric(12_2)");
        assert_eq!(mermaid_word("1st"), "_1st");
        assert_eq!(mermaid_word("pk"), "pk_");
        assert_eq!(mermaid_word(""), "_");
        assert_eq!(mermaid_word("año"), "año");
    }

    #[test]
    fn both_texts_carry_tables_keys_and_comments() {
        let mut orders = table(
            "s",
            "orders",
            &[("id", "int", false), ("customer id", "int", true)],
            &["id"],
            &[("orders_customer", &["customer id"], "customers", &["id"])],
        );
        orders.columns[1].comment = Some("who \"ordered\"\nit".into());
        orders.object.comment = Some("it's here".into());
        let customers = table("s", "customers", &[("id", "int", false)], &["id"], &[]);
        let model = ErModel::from_tables(vec![orders, customers], None);
        let m = mermaid(&model);
        assert!(m.starts_with("erDiagram\n"));
        assert!(m.contains("        int customer_id FK \"customer id: who 'ordered' it\"\n"));
        assert!(m.contains("    \"customers\" |o..o{ \"orders\" : \"orders_customer\"\n"));
        assert!(m.contains("    %% orders: it's here\n"));
        let d = dbml(&model);
        assert!(d.contains("Table \"orders\" [note: 'it\\'s here'] {\n"));
        assert!(d.contains("  \"customer id\" \"int\" [note: 'who \"ordered\"\nit']\n"));
        assert!(d.contains("  \"id\" \"int\" [pk, not null]\n"));
        assert!(d.contains(
            "Ref \"orders_customer\": \"orders\".\"customer id\" > \"customers\".\"id\"\n"
        ));
    }
}
