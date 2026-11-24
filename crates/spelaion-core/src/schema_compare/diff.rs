//! Comparing two snapshots (ADR-070, points 3 to 5): objects matched by kind and name, every
//! property decided here once, with the structure forms' rules (`same_type`, `typed`).

use std::collections::HashSet;

use spelaion_config::mcp_clients::{DiffLine, line_diff};
use spelaion_driver_api::{Dialect, ForeignKeyInfo};

use super::{
    CompareObjectKind, ComparedPart, ConstraintKindCompared, ConstraintSnapshot, IndexSnapshot,
    NotCompared, ObjectChanges, ObjectComparison, ObjectStatus, PartComparison, PartStatus,
    Property, PropertyChange, RoutineSnapshot, SchemaComparison, SchemaSnapshot, TableSnapshot,
    ViewSnapshot,
};
use crate::objects::EditableColumn;
use crate::objects::alter::{flavor, same_type, typed};
use crate::objects::ddl::{self, Flavor};

/// Unchanged lines shown around each change of a source's line diff.
const DIFF_CONTEXT: usize = 3;

/// What stands for an object's own name when two definitions are compared: never in SQL.
const OWN_NAME: &str = "\u{1}";
/// What stands for an index's own table.
const OWN_TABLE: &str = "\u{2}";
/// What stands for a source's own schema where it qualifies a name.
const OWN_SCHEMA: &str = "\u{3}";

/// Compares `source` with `target`, both read on `dialect`'s engine.
pub fn compare(
    dialect: &dyn Dialect,
    source: &SchemaSnapshot,
    target: &SchemaSnapshot,
) -> SchemaComparison {
    let rules = Rules {
        flavor: flavor(dialect.name()),
        source_schema: &source.schema,
        target_schema: &target.schema,
    };
    let mut objects = Vec::new();
    let mut same = 0;

    let mut record = |kind: CompareObjectKind,
                      name: &str,
                      detail: Option<&String>,
                      status: Option<ObjectStatus>| {
        match status {
            Some(status) => objects.push(ObjectComparison {
                kind,
                name: name.to_string(),
                detail: detail.cloned(),
                status,
            }),
            None => same += 1,
        }
    };

    for (s, t) in pairs(&source.tables, &target.tables, |t| t.name.clone()) {
        let name = s.or(t).map(|t| t.name.as_str()).unwrap_or_default();
        let status = match (s, t) {
            (Some(_), None) => Some(ObjectStatus::OnlyInSource),
            (None, Some(_)) => Some(ObjectStatus::OnlyInTarget),
            (Some(s), Some(t)) => rules.table(s, t).map(ObjectStatus::Changed),
            (None, None) => None,
        };
        record(CompareObjectKind::Table, name, None, status);
    }
    for (s, t) in pairs(&source.views, &target.views, |v| v.name.clone()) {
        let view = s.or(t).expect("a pair has a side");
        let kind = if view.materialized {
            CompareObjectKind::MaterializedView
        } else {
            CompareObjectKind::View
        };
        let status = match (s, t) {
            (Some(_), None) => Some(ObjectStatus::OnlyInSource),
            (None, Some(_)) => Some(ObjectStatus::OnlyInTarget),
            (Some(s), Some(t)) => rules.view(s, t).map(ObjectStatus::Changed),
            (None, None) => None,
        };
        record(kind, &view.name, None, status);
    }
    for (s, t) in pairs(&source.routines, &target.routines, |r| {
        format!(
            "{:?}\u{0}{}\u{0}{}",
            r.kind,
            r.name,
            r.detail.as_deref().unwrap_or("")
        )
    }) {
        let routine = s.or(t).expect("a pair has a side");
        let status = match (s, t) {
            (Some(_), None) => Some(ObjectStatus::OnlyInSource),
            (None, Some(_)) => Some(ObjectStatus::OnlyInTarget),
            (Some(s), Some(t)) => rules.routine(s, t).map(ObjectStatus::Changed),
            (None, None) => None,
        };
        record(
            routine.kind.compared(),
            &routine.name,
            routine.detail.as_ref(),
            status,
        );
    }
    objects.sort_by(|a, b| (a.kind, &a.name, &a.detail).cmp(&(b.kind, &b.name, &b.detail)));
    SchemaComparison {
        source_schema: source.schema.clone(),
        target_schema: target.schema.clone(),
        objects,
        same,
        not_compared: vec![
            NotCompared::ForeignKeyActions,
            NotCompared::ColumnAttributes,
            NotCompared::ColumnOrder,
            NotCompared::OtherObjects,
        ],
    }
}

/// Every item of either side once, paired with the other side's item of the same key, in the
/// source's order and then the target's.
fn pairs<'a, T>(
    source: &'a [T],
    target: &'a [T],
    key: impl Fn(&T) -> String,
) -> Vec<(Option<&'a T>, Option<&'a T>)> {
    let mut out = Vec::new();
    let mut matched = HashSet::new();
    for s in source {
        let k = key(s);
        let t = target.iter().position(|t| key(t) == k);
        if let Some(i) = t {
            matched.insert(i);
        }
        out.push((Some(s), t.map(|i| &target[i])));
    }
    for (i, t) in target.iter().enumerate() {
        if !matched.contains(&i) {
            out.push((None, Some(t)));
        }
    }
    out
}

/// Parts paired by name first, then, among those left, by what they are (`key`): an engine
/// that names constraints itself (SQL Server's `UQ__t__…`) names the same constraint
/// differently on two servers.
fn parts_paired<'a, T>(
    source: &'a [T],
    target: &'a [T],
    name: impl Fn(&T) -> Option<String>,
    key: impl Fn(&T) -> String,
) -> Vec<(Option<&'a T>, Option<&'a T>)> {
    let mut used_target = vec![false; target.len()];
    let mut out: Vec<(Option<&'a T>, Option<&'a T>)> = Vec::new();
    let mut left: Vec<&'a T> = Vec::new();
    for s in source {
        let by_name = name(s).and_then(|n| {
            target
                .iter()
                .enumerate()
                .position(|(i, t)| !used_target[i] && name(t).as_deref() == Some(n.as_str()))
        });
        match by_name {
            Some(i) => {
                used_target[i] = true;
                out.push((Some(s), Some(&target[i])));
            }
            None => left.push(s),
        }
    }
    for s in left {
        let k = key(s);
        let by_key = target
            .iter()
            .enumerate()
            .position(|(i, t)| !used_target[i] && key(t) == k);
        match by_key {
            Some(i) => {
                used_target[i] = true;
                out.push((Some(s), Some(&target[i])));
            }
            None => out.push((Some(s), None)),
        }
    }
    for (i, t) in target.iter().enumerate() {
        if !used_target[i] {
            out.push((None, Some(t)));
        }
    }
    out
}

/// Runs of blanks as one space, and case kept: what two engines of the same kind write the
/// same way.
fn blanks(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A source text with line endings and trailing blanks ignored.
fn lines(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn change(property: Property, source: Option<String>, target: Option<String>) -> PropertyChange {
    PropertyChange {
        property,
        source,
        target,
    }
}

fn yes_no(value: bool) -> Option<String> {
    Some(if value { "yes" } else { "no" }.to_string())
}

struct Rules<'a> {
    flavor: Flavor,
    source_schema: &'a str,
    target_schema: &'a str,
}

impl Rules<'_> {
    fn table(&self, s: &TableSnapshot, t: &TableSnapshot) -> Option<ObjectChanges> {
        let mut parts = Vec::new();
        if typed(&s.comment) != typed(&t.comment) {
            parts.push(PartComparison {
                part: ComparedPart::Object,
                status: PartStatus::Changed(vec![change(
                    Property::Comment,
                    typed(&s.comment),
                    typed(&t.comment),
                )]),
            });
        }
        for (sc, tc) in pairs(&s.columns, &t.columns, |c| c.name.clone()) {
            let column = sc.or(tc).expect("a pair has a side");
            let status = match (sc, tc) {
                (Some(c), None) => Some(PartStatus::OnlyInSource(column_text(c))),
                (None, Some(c)) => Some(PartStatus::OnlyInTarget(column_text(c))),
                (Some(sc), Some(tc)) => {
                    let changes = column_changes(sc, tc);
                    (!changes.is_empty()).then_some(PartStatus::Changed(changes))
                }
                (None, None) => None,
            };
            if let Some(status) = status {
                parts.push(PartComparison {
                    part: ComparedPart::Column(column.name.clone()),
                    status,
                });
            }
        }
        if s.primary_key != t.primary_key {
            let key = |columns: &[String]| format!("PRIMARY KEY ({})", columns.join(", "));
            let status = match (s.primary_key.is_empty(), t.primary_key.is_empty()) {
                (false, true) => PartStatus::OnlyInSource(key(&s.primary_key)),
                (true, false) => PartStatus::OnlyInTarget(key(&t.primary_key)),
                (false, false) | (true, true) => PartStatus::Changed(vec![change(
                    Property::Columns,
                    Some(s.primary_key.join(", ")),
                    Some(t.primary_key.join(", ")),
                )]),
            };
            parts.push(PartComparison {
                part: ComparedPart::PrimaryKey,
                status,
            });
        }
        self.constraints(s, t, &mut parts);
        self.indexes(s, t, &mut parts);
        self.foreign_keys(s, t, &mut parts);
        (!parts.is_empty()).then_some(ObjectChanges {
            parts,
            source_diff: Vec::new(),
            names_source_schema: false,
        })
    }

    fn constraints(&self, s: &TableSnapshot, t: &TableSnapshot, parts: &mut Vec<PartComparison>) {
        let key = |c: &ConstraintSnapshot| {
            format!(
                "{:?}\u{0}{}\u{0}{}",
                c.kind,
                c.columns.join(","),
                blanks(&c.definition)
            )
        };
        let label = |c: &ConstraintSnapshot| c.name.clone().unwrap_or_else(|| c.definition.clone());
        for (sc, tc) in parts_paired(&s.constraints, &t.constraints, |c| c.name.clone(), key) {
            let status = match (sc, tc) {
                (Some(c), None) => Some((label(c), PartStatus::OnlyInSource(c.definition.clone()))),
                (None, Some(c)) => Some((label(c), PartStatus::OnlyInTarget(c.definition.clone()))),
                (Some(sc), Some(tc)) => {
                    let mut changes = Vec::new();
                    if sc.kind != tc.kind {
                        changes.push(change(
                            Property::Unique,
                            yes_no(sc.kind == ConstraintKindCompared::Unique),
                            yes_no(tc.kind == ConstraintKindCompared::Unique),
                        ));
                    }
                    if sc.columns != tc.columns {
                        changes.push(change(
                            Property::Columns,
                            Some(sc.columns.join(", ")),
                            Some(tc.columns.join(", ")),
                        ));
                    }
                    if blanks(&sc.definition) != blanks(&tc.definition) {
                        changes.push(change(
                            Property::Definition,
                            Some(sc.definition.clone()),
                            Some(tc.definition.clone()),
                        ));
                    }
                    (!changes.is_empty()).then(|| (label(sc), PartStatus::Changed(changes)))
                }
                (None, None) => None,
            };
            if let Some((name, status)) = status {
                parts.push(PartComparison {
                    part: ComparedPart::Constraint(name),
                    status,
                });
            }
        }
    }

    /// An index's definition with its own name and table left out.
    fn index_shape(&self, index: &IndexSnapshot) -> String {
        let shaped = ddl::rename_created(&index.definition, self.flavor, OWN_NAME, Some(OWN_TABLE))
            .unwrap_or_else(|_| index.definition.clone());
        blanks(shaped.trim_end_matches(';'))
    }

    fn indexes(&self, s: &TableSnapshot, t: &TableSnapshot, parts: &mut Vec<PartComparison>) {
        let key = |i: &IndexSnapshot| format!("{}\u{0}{}", i.unique, self.index_shape(i));
        for (si, ti) in parts_paired(&s.indexes, &t.indexes, |i| Some(i.name.clone()), key) {
            let status = match (si, ti) {
                (Some(i), None) => Some((
                    i.name.clone(),
                    PartStatus::OnlyInSource(i.definition.clone()),
                )),
                (None, Some(i)) => Some((
                    i.name.clone(),
                    PartStatus::OnlyInTarget(i.definition.clone()),
                )),
                (Some(si), Some(ti)) => {
                    let mut changes = Vec::new();
                    if si.unique != ti.unique {
                        changes.push(change(
                            Property::Unique,
                            yes_no(si.unique),
                            yes_no(ti.unique),
                        ));
                    }
                    if self.index_shape(si) != self.index_shape(ti) {
                        changes.push(change(
                            Property::Definition,
                            Some(si.definition.clone()),
                            Some(ti.definition.clone()),
                        ));
                    }
                    (!changes.is_empty()).then(|| (si.name.clone(), PartStatus::Changed(changes)))
                }
                (None, None) => None,
            };
            if let Some((name, status)) = status {
                parts.push(PartComparison {
                    part: ComparedPart::Index(name),
                    status,
                });
            }
        }
    }

    /// The referenced table, with the schema left out when it is the key's own schema: a key
    /// that points into its own schema points into the target's once moved there.
    fn referenced(&self, key: &ForeignKeyInfo, own_schema: &str) -> String {
        if key.referenced_schema.is_empty() || key.referenced_schema == own_schema {
            key.referenced_table.clone()
        } else {
            format!("{}.{}", key.referenced_schema, key.referenced_table)
        }
    }

    fn foreign_keys(&self, s: &TableSnapshot, t: &TableSnapshot, parts: &mut Vec<PartComparison>) {
        let text = |k: &ForeignKeyInfo, own: &str| {
            format!(
                "FOREIGN KEY ({}) REFERENCES {} ({})",
                k.columns.join(", "),
                self.referenced(k, own),
                k.referenced_columns.join(", ")
            )
        };
        let source_key = |k: &ForeignKeyInfo| text(k, self.source_schema);
        let target_key = |k: &ForeignKeyInfo| text(k, self.target_schema);
        // Paired by name, then by what they are, each side read against its own schema.
        let mut used = vec![false; t.foreign_keys.len()];
        let mut paired: Vec<(Option<&ForeignKeyInfo>, Option<&ForeignKeyInfo>)> = Vec::new();
        let mut left = Vec::new();
        for sk in &s.foreign_keys {
            match t
                .foreign_keys
                .iter()
                .enumerate()
                .position(|(i, tk)| !used[i] && !sk.name.is_empty() && tk.name == sk.name)
            {
                Some(i) => {
                    used[i] = true;
                    paired.push((Some(sk), Some(&t.foreign_keys[i])));
                }
                None => left.push(sk),
            }
        }
        for sk in left {
            let k = source_key(sk);
            match t
                .foreign_keys
                .iter()
                .enumerate()
                .position(|(i, tk)| !used[i] && target_key(tk) == k)
            {
                Some(i) => {
                    used[i] = true;
                    paired.push((Some(sk), Some(&t.foreign_keys[i])));
                }
                None => paired.push((Some(sk), None)),
            }
        }
        for (i, tk) in t.foreign_keys.iter().enumerate() {
            if !used[i] {
                paired.push((None, Some(tk)));
            }
        }
        let label = |k: &ForeignKeyInfo, own: &str| {
            if k.name.is_empty() {
                text(k, own)
            } else {
                k.name.clone()
            }
        };
        for pair in paired {
            let status = match pair {
                (Some(k), None) => Some((
                    label(k, self.source_schema),
                    PartStatus::OnlyInSource(source_key(k)),
                )),
                (None, Some(k)) => Some((
                    label(k, self.target_schema),
                    PartStatus::OnlyInTarget(target_key(k)),
                )),
                (Some(sk), Some(tk)) => {
                    let mut changes = Vec::new();
                    if sk.columns != tk.columns {
                        changes.push(change(
                            Property::Columns,
                            Some(sk.columns.join(", ")),
                            Some(tk.columns.join(", ")),
                        ));
                    }
                    let (sr, tr) = (
                        self.referenced(sk, self.source_schema),
                        self.referenced(tk, self.target_schema),
                    );
                    if sr != tr {
                        changes.push(change(Property::ReferencedTable, Some(sr), Some(tr)));
                    }
                    if sk.referenced_columns != tk.referenced_columns {
                        changes.push(change(
                            Property::ReferencedColumns,
                            Some(sk.referenced_columns.join(", ")),
                            Some(tk.referenced_columns.join(", ")),
                        ));
                    }
                    (!changes.is_empty())
                        .then(|| (label(sk, self.source_schema), PartStatus::Changed(changes)))
                }
                (None, None) => None,
            };
            if let Some((name, status)) = status {
                parts.push(PartComparison {
                    part: ComparedPart::ForeignKey(name),
                    status,
                });
            }
        }
    }

    /// A view's or routine's source with its own name, and every name its own schema
    /// qualifies, left out for comparing: the same view in two schemas names its own schema's
    /// tables differently (PostgreSQL, MySQL and DuckDB write the schema in).
    fn source_shape(&self, definition: &str, own_schema: &str) -> String {
        let renamed = ddl::rename_defined(definition, self.flavor, OWN_NAME)
            .unwrap_or_else(|_| definition.to_string());
        lines(&ddl::requalified(&renamed, self.flavor, own_schema, OWN_SCHEMA).unwrap_or(renamed))
    }

    fn source_changes(&self, source: &str, target: &str) -> Option<ObjectChanges> {
        if self.source_shape(source, self.source_schema)
            == self.source_shape(target, self.target_schema)
        {
            return None;
        }
        let source_diff: Vec<DiffLine> = line_diff(&lines(target), &lines(source), DIFF_CONTEXT);
        Some(ObjectChanges {
            parts: vec![PartComparison {
                part: ComparedPart::Object,
                status: PartStatus::Changed(vec![change(
                    Property::Source,
                    Some(source.to_string()),
                    Some(target.to_string()),
                )]),
            }],
            source_diff,
            names_source_schema: self.names_source_schema(source),
        })
    }

    /// The source's text names the source schema, which the target's is not, other than as a
    /// qualifier (those are moved to the target's): in a string (dynamic SQL), or bare.
    fn names_source_schema(&self, text: &str) -> bool {
        self.source_schema != self.target_schema && {
            let requalified = ddl::requalified(text, self.flavor, self.source_schema, OWN_SCHEMA)
                .unwrap_or_else(|_| text.to_string());
            ddl::mentions(&requalified, self.source_schema, self.flavor)
                || ddl::strings_qualify_with(&requalified, self.flavor, self.source_schema)
        }
    }

    fn view(&self, s: &ViewSnapshot, t: &ViewSnapshot) -> Option<ObjectChanges> {
        self.source_changes(&s.definition, &t.definition)
    }

    fn routine(&self, s: &RoutineSnapshot, t: &RoutineSnapshot) -> Option<ObjectChanges> {
        self.source_changes(&s.definition, &t.definition)
    }
}

/// A column as one line of a report.
fn column_text(column: &EditableColumn) -> String {
    let mut text = format!("{} {}", column.name, column.data_type);
    if !column.nullable {
        text.push_str(" NOT NULL");
    }
    if let Some(default) = typed(&column.default) {
        text.push_str(&format!(" DEFAULT {default}"));
    }
    text
}

fn column_changes(s: &EditableColumn, t: &EditableColumn) -> Vec<PropertyChange> {
    let mut changes = Vec::new();
    if !same_type(&s.data_type, &t.data_type) {
        changes.push(change(
            Property::Type,
            Some(s.data_type.clone()),
            Some(t.data_type.clone()),
        ));
    }
    if s.nullable != t.nullable {
        changes.push(change(
            Property::Nullable,
            yes_no(s.nullable),
            yes_no(t.nullable),
        ));
    }
    if typed(&s.default) != typed(&t.default) {
        changes.push(change(
            Property::Default,
            typed(&s.default),
            typed(&t.default),
        ));
    }
    if typed(&s.comment) != typed(&t.comment) {
        changes.push(change(
            Property::Comment,
            typed(&s.comment),
            typed(&t.comment),
        ));
    }
    if s.generated != t.generated {
        changes.push(change(
            Property::Generated,
            yes_no(s.generated),
            yes_no(t.generated),
        ));
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_compare::{RoutineKind, RoutineSnapshot};
    use spelaion_dialect::PostgresDialect as Pg;

    fn column(
        name: &str,
        data_type: &str,
        nullable: bool,
        default: Option<&str>,
    ) -> EditableColumn {
        EditableColumn {
            name: name.into(),
            data_type: data_type.into(),
            nullable,
            default: default.map(String::from),
            comment: None,
            generated: false,
        }
    }

    fn table(name: &str, columns: Vec<EditableColumn>) -> TableSnapshot {
        TableSnapshot {
            name: name.into(),
            comment: None,
            columns,
            primary_key: vec!["id".into()],
            primary_key_name: Some(format!("{name}_pkey")),
            constraints: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            kept_definition: None,
        }
    }

    fn snapshot(schema: &str, tables: Vec<TableSnapshot>) -> SchemaSnapshot {
        SchemaSnapshot {
            schema: schema.into(),
            tables,
            views: Vec::new(),
            routines: Vec::new(),
        }
    }

    fn statuses(comparison: &SchemaComparison) -> Vec<(CompareObjectKind, String, &'static str)> {
        comparison
            .objects
            .iter()
            .map(|o| {
                let status = match o.status {
                    ObjectStatus::OnlyInSource => "source",
                    ObjectStatus::OnlyInTarget => "target",
                    ObjectStatus::Changed(_) => "changed",
                };
                (o.kind, o.name.clone(), status)
            })
            .collect()
    }

    #[test]
    fn equal_schemas_compare_empty_whatever_their_names_and_blanks() {
        let id = column("id", "integer", false, None);
        let mut source = snapshot("a", vec![table("t", vec![id.clone()])]);
        let mut target = snapshot(
            "b",
            vec![table("t", vec![column("id", "INTEGER ", false, None)])],
        );
        source.tables[0].indexes.push(IndexSnapshot {
            name: "t_x".into(),
            unique: false,
            definition: "CREATE INDEX t_x ON a.t USING btree (id)".into(),
        });
        target.tables[0].indexes.push(IndexSnapshot {
            name: "t_x".into(),
            unique: false,
            definition: "CREATE INDEX t_x ON b.t USING  btree (id);".into(),
        });
        source.views.push(ViewSnapshot {
            name: "v".into(),
            materialized: false,
            definition: "CREATE VIEW a.v AS\r\n SELECT 1;  ".into(),
        });
        target.views.push(ViewSnapshot {
            name: "v".into(),
            materialized: false,
            definition: "CREATE VIEW b.v AS\n SELECT 1;".into(),
        });
        let comparison = compare(&Pg, &source, &target);
        assert!(comparison.is_empty(), "{:?}", comparison.objects);
        assert_eq!(comparison.same, 2);
        assert!(
            comparison
                .not_compared
                .contains(&NotCompared::ForeignKeyActions)
        );
    }

    #[test]
    fn objects_only_on_one_side_and_changed_columns_are_reported() {
        let source = snapshot(
            "s",
            vec![
                table(
                    "items",
                    vec![
                        column("id", "integer", false, None),
                        column("price", "numeric(10,2)", true, Some("0")),
                        column("note", "text", true, None),
                    ],
                ),
                table("only_source", vec![column("id", "integer", false, None)]),
            ],
        );
        let target = snapshot(
            "t",
            vec![
                table(
                    "items",
                    vec![
                        column("id", "integer", false, None),
                        column("price", "numeric(12,2)", false, None),
                        column("legacy", "text", true, None),
                    ],
                ),
                table("only_target", vec![column("id", "integer", false, None)]),
            ],
        );
        let comparison = compare(&Pg, &source, &target);
        assert_eq!(
            statuses(&comparison),
            vec![
                (CompareObjectKind::Table, "items".into(), "changed"),
                (CompareObjectKind::Table, "only_source".into(), "source"),
                (CompareObjectKind::Table, "only_target".into(), "target"),
            ]
        );
        let ObjectStatus::Changed(changes) = &comparison.objects[0].status else {
            panic!("items changed");
        };
        let part = |name: &str| {
            changes
                .parts
                .iter()
                .find(|p| p.part == ComparedPart::Column(name.into()))
                .map(|p| &p.status)
        };
        let Some(PartStatus::Changed(price)) = part("price") else {
            panic!("price changed: {changes:?}");
        };
        let properties: Vec<Property> = price.iter().map(|c| c.property).collect();
        assert_eq!(
            properties,
            [Property::Type, Property::Nullable, Property::Default]
        );
        assert!(matches!(part("note"), Some(PartStatus::OnlyInSource(_))));
        assert!(matches!(part("legacy"), Some(PartStatus::OnlyInTarget(_))));
    }

    #[test]
    fn engine_named_constraints_pair_by_what_they_are() {
        let mut source = snapshot(
            "dbo",
            vec![table("t", vec![column("id", "int", false, None)])],
        );
        let mut target = source.clone();
        source.tables[0].constraints.push(ConstraintSnapshot {
            name: Some("UQ__t__3213E83F".into()),
            kind: ConstraintKindCompared::Unique,
            columns: vec!["id".into()],
            definition: "UNIQUE (id)".into(),
        });
        target.tables[0].constraints.push(ConstraintSnapshot {
            name: Some("UQ__t__AB12CD34".into()),
            kind: ConstraintKindCompared::Unique,
            columns: vec!["id".into()],
            definition: "UNIQUE (id)".into(),
        });
        target.tables[0].constraints.push(ConstraintSnapshot {
            name: Some("ck_positive".into()),
            kind: ConstraintKindCompared::Check,
            columns: vec![],
            definition: "CHECK ([id] > 0)".into(),
        });
        let comparison = compare(&Pg, &source, &target);
        let ObjectStatus::Changed(changes) = &comparison.objects[0].status else {
            panic!("t changed");
        };
        assert_eq!(changes.parts.len(), 1, "{changes:?}");
        assert_eq!(
            changes.parts[0].part,
            ComparedPart::Constraint("ck_positive".into())
        );
        assert!(matches!(
            changes.parts[0].status,
            PartStatus::OnlyInTarget(_)
        ));
    }

    #[test]
    fn a_key_into_its_own_schema_matches_across_schemas() {
        let key = |schema: &str| ForeignKeyInfo {
            name: "fk".into(),
            columns: vec!["item".into()],
            referenced_schema: schema.into(),
            referenced_table: "items".into(),
            referenced_columns: vec!["id".into()],
        };
        let mut source = snapshot(
            "a",
            vec![table("orders", vec![column("id", "int", false, None)])],
        );
        let mut target = snapshot("b", source.tables.clone());
        source.tables[0].foreign_keys.push(key("a"));
        target.tables[0].foreign_keys.push(key("b"));
        assert!(compare(&Pg, &source, &target).is_empty());
        target.tables[0].foreign_keys[0].referenced_schema = "elsewhere".into();
        let comparison = compare(&Pg, &source, &target);
        let ObjectStatus::Changed(changes) = &comparison.objects[0].status else {
            panic!("orders changed");
        };
        let PartStatus::Changed(properties) = &changes.parts[0].status else {
            panic!("the key changed");
        };
        assert_eq!(properties[0].property, Property::ReferencedTable);
        assert_eq!(properties[0].target.as_deref(), Some("elsewhere.items"));
    }

    #[test]
    fn a_changed_source_carries_its_line_diff_and_says_when_it_names_the_source_schema() {
        let routine = |schema: &str, body: &str| RoutineSnapshot {
            name: "f".into(),
            kind: RoutineKind::Function,
            detail: Some("integer".into()),
            definition: format!(
                "CREATE OR REPLACE FUNCTION {schema}.f(x integer)\n RETURNS integer\nAS $$ {body} $$;"
            ),
        };
        let mut source = snapshot("a", vec![]);
        let mut target = snapshot("b", vec![]);
        // Dynamic SQL names the source schema in a string: it is not moved, and it is said.
        source.routines.push(routine(
            "a",
            "SELECT x + (SELECT count(*) FROM a.t); EXECUTE 'DELETE FROM a.t'",
        ));
        target.routines.push(routine("b", "SELECT x"));
        let comparison = compare(&Pg, &source, &target);
        let object = &comparison.objects[0];
        assert_eq!(object.kind, CompareObjectKind::Function);
        assert_eq!(object.detail.as_deref(), Some("integer"));
        let ObjectStatus::Changed(changes) = &object.status else {
            panic!("f changed");
        };
        assert!(changes.names_source_schema);
        assert!(
            changes
                .source_diff
                .iter()
                .any(|l| matches!(l, DiffLine::Added(_)))
        );
        assert!(
            changes
                .source_diff
                .iter()
                .any(|l| matches!(l, DiffLine::Removed(_)))
        );
        // Same body: neither its own name nor its own schema's qualifiers count.
        source.routines[0] = routine("a", "SELECT x + (SELECT count(*) FROM a.t)");
        target.routines[0] = routine("b", "SELECT x + (SELECT count(*) FROM b.t)");
        assert!(compare(&Pg, &source, &target).is_empty());
        // Another schema's qualifier does count, and a qualifier alone does not name the
        // source schema.
        target.routines[0] = routine("b", "SELECT x + (SELECT count(*) FROM c.t)");
        let comparison = compare(&Pg, &source, &target);
        let ObjectStatus::Changed(changes) = &comparison.objects[0].status else {
            panic!("f changed");
        };
        assert!(!changes.names_source_schema);
    }

    #[test]
    fn a_target_view_over_the_source_schema_is_not_the_same_as_the_source_view() {
        let view = |schema: &str, over: &str| ViewSnapshot {
            name: "v".into(),
            materialized: false,
            definition: format!("CREATE VIEW {schema}.v AS SELECT a FROM {over}.t;"),
        };
        let mut source = snapshot("src", vec![]);
        let mut target = snapshot("dst", vec![]);
        source.views.push(view("src", "src"));
        // Each side's own schema counts as the same: src.t in src is dst.t in dst.
        target.views.push(view("dst", "dst"));
        assert!(compare(&Pg, &source, &target).is_empty());
        // The target's view reads the source schema's table: not the source's view moved.
        target.views[0] = view("dst", "src");
        let comparison = compare(&Pg, &source, &target);
        assert!(
            matches!(
                comparison.objects.as_slice(),
                [ObjectComparison {
                    status: ObjectStatus::Changed(_),
                    ..
                }]
            ),
            "{comparison:?}"
        );
    }

    #[test]
    fn overloads_are_compared_one_by_one() {
        let overload = |detail: &str| RoutineSnapshot {
            name: "f".into(),
            kind: RoutineKind::Function,
            detail: Some(detail.into()),
            definition: format!("CREATE FUNCTION s.f({detail}) RETURNS int AS $$ SELECT 1 $$;"),
        };
        let mut source = snapshot("s", vec![]);
        let mut target = snapshot("s", vec![]);
        source.routines = vec![overload("integer"), overload("text")];
        target.routines = vec![overload("integer"), overload("bigint")];
        let comparison = compare(&Pg, &source, &target);
        assert_eq!(comparison.same, 1);
        let details: Vec<_> = comparison
            .objects
            .iter()
            .map(|o| {
                (
                    o.detail.clone().unwrap_or_default(),
                    matches!(o.status, ObjectStatus::OnlyInSource),
                )
            })
            .collect();
        assert_eq!(
            details,
            [("bigint".to_string(), false), ("text".to_string(), true)]
        );
    }
}
