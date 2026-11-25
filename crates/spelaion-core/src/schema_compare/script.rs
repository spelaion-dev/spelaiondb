//! The script that makes the target like the source (ADR-070, point 6), written from both
//! snapshots by the writers that already write each statement: ADR-047's alteration (SQLite's
//! rebuild included) and `create.rs`, ADR-026's drops and table elements, "Edit source"'s
//! replacing form, and the engine's own kept `CREATE TABLE` text where it has one. It is only
//! ever opened in an editor tab: nothing here runs anything (point 7).
//!
//! The order is the one the statements need: views and routines that go, each common table's
//! alteration (its changed columns and the keys, constraints and foreign keys it drops),
//! indexes that go, tables that go, new tables, columns that go and come, constraints and
//! indexes that come, foreign keys that come, and the views and routines of the source last.

use std::collections::HashSet;

use spelaion_driver_api::{
    Dialect, ForeignKeyInfo, ObjectKind, SchemaObject, SchemaObjectKind, terminated,
};

use super::{
    CompareError, CompareObjectKind, ComparedSchemas, ConstraintKindCompared, ConstraintSnapshot,
    IndexSnapshot, ObjectStatus, RoutineKind, RoutineSnapshot, SchemaSnapshot, TableSnapshot,
    ViewSnapshot,
};
use crate::objects::alter::{same_type, typed};
use crate::objects::ddl::{self, Flavor};
use crate::objects::source::replace_form;
use crate::objects::{
    ColumnEdit, DroppedConstraint, NewColumn, NewTable, NewTableElement, ObjectChange,
    ObjectTarget, TableAlteration, alteration_plan, change_statements, create_table_plan,
    new_table_element_sql, qualified_name,
};
use crate::session::Session;

/// What the user chose for the script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MigrationOptions {
    /// Also drop what is only in the target (tables, columns, views, routines, indexes,
    /// constraints and foreign keys); otherwise it is left in place and listed.
    pub drops: bool,
}

/// The script, and what its header says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationScript {
    /// The whole script, header included, as it opens in the editor.
    pub sql: String,
    /// Statements that cannot be undone: drops and SQLite's rebuilds, as `kind name`.
    pub irreversible: Vec<String>,
    /// What is only in the target and the script leaves in place (drops not asked for).
    pub left_in_place: Vec<String>,
    /// What the script cannot do on this engine, with the reason.
    pub not_scripted: Vec<NotScripted>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotScripted {
    pub object: String,
    pub reason: String,
}

/// The script's statements, section by section, with what its header says.
struct Writer<'a> {
    dialect: &'a dyn Dialect,
    flavor: Flavor,
    engine: &'static str,
    mariadb: bool,
    version: String,
    source_schema: &'a str,
    target_schema: &'a str,
    sections: Vec<(String, Vec<String>)>,
    out: MigrationScript,
}

fn table_of<'a>(snapshot: &'a SchemaSnapshot, name: &str) -> Option<&'a TableSnapshot> {
    snapshot.tables.iter().find(|t| t.name == name)
}

impl Writer<'_> {
    fn section(&mut self, title: &str) {
        self.sections.push((title.to_string(), Vec::new()));
    }

    /// One piece of the script, each statement ending in `;` (a piece that writes its own
    /// delimiters, MySQL's routines, is kept as it is).
    fn push(&mut self, sql: impl Into<String>) {
        let sql: String = sql.into();
        let sql = sql.trim();
        if sql.is_empty() {
            return;
        }
        let piece = if sql.contains("DELIMITER") {
            sql.to_string()
        } else {
            terminated(sql)
        };
        if let Some((_, pieces)) = self.sections.last_mut() {
            pieces.push(piece);
        }
    }

    fn not_scripted(&mut self, object: impl Into<String>, reason: impl ToString) {
        self.out.not_scripted.push(NotScripted {
            object: object.into(),
            reason: reason.to_string(),
        });
    }

    fn quote(&self, name: &str) -> String {
        self.dialect.quote_identifier(name)
    }

    fn target_name(&self, name: &str) -> String {
        qualified_name(self.dialect, self.target_schema, &[name.to_string()])
    }

    fn drop(&mut self, target: ObjectTarget, label: String) {
        match change_statements(
            self.dialect,
            &ObjectChange::Drop {
                target,
                cascade: false,
            },
        ) {
            Ok(statements) => {
                for statement in statements {
                    self.push(statement);
                }
                self.out.irreversible.push(label);
            }
            Err(e) => self.not_scripted(label, e),
        }
    }

    /// A source text moved to the target: its own name, and the source schema where the tokens
    /// show it qualifies a name (ADR-070, point 4).
    fn moved_definition(&self, definition: &str, name: &str) -> String {
        let qualified = self.target_name(name);
        let renamed = ddl::rename_defined(definition, self.flavor, &qualified)
            .unwrap_or_else(|_| definition.to_string());
        if self.source_schema == self.target_schema {
            return renamed;
        }
        ddl::requalified(
            &renamed,
            self.flavor,
            self.source_schema,
            &self.quote(self.target_schema),
        )
        .unwrap_or(renamed)
    }

    /// A source index's `CREATE INDEX` put on the target's table: SQLite names the index's
    /// schema and leaves the table bare, the others name the table's schema.
    fn index_statement(&mut self, table: &str, index: &IndexSnapshot) {
        let (name, on) = if self.engine == "sqlite" {
            (self.target_name(&index.name), self.quote(table))
        } else {
            (self.quote(&index.name), self.target_name(table))
        };
        match ddl::rename_created(&index.definition, self.flavor, &name, Some(&on)) {
            Ok(sql) => self.push(sql),
            Err(e) => self.not_scripted(format!("index {}", index.name), e),
        }
    }

    fn constraint_element(c: &ConstraintSnapshot) -> NewTableElement {
        let expression = match c.kind {
            ConstraintKindCompared::Unique => String::new(),
            ConstraintKindCompared::Check => check_expression(&c.definition),
        };
        NewTableElement::Constraint {
            name: c.name.clone().unwrap_or_default(),
            kind: match c.kind {
                ConstraintKindCompared::Unique => "unique",
                ConstraintKindCompared::Check => "check",
            }
            .to_string(),
            columns: c.columns.clone(),
            expression,
        }
    }

    fn element(&mut self, table: &str, element: &NewTableElement, label: String) {
        match new_table_element_sql(
            self.dialect,
            &self.version,
            self.target_schema,
            table,
            element,
        ) {
            Ok(sql) => self.push(sql),
            Err(e) => self.not_scripted(label, e),
        }
    }

    /// A key that pointed into the source schema points into the target's.
    fn foreign_key_element(&self, key: &ForeignKeyInfo) -> NewTableElement {
        let referenced_schema =
            if key.referenced_schema.is_empty() || key.referenced_schema == self.source_schema {
                self.target_schema.to_string()
            } else {
                key.referenced_schema.clone()
            };
        NewTableElement::ForeignKey {
            name: key.name.clone(),
            columns: key.columns.clone(),
            referenced_schema,
            referenced_table: key.referenced_table.clone(),
            referenced_columns: key.referenced_columns.clone(),
        }
    }
}

/// `CHECK (expr)` as the catalog writes it, without its keyword and outer parentheses.
fn check_expression(definition: &str) -> String {
    let text = definition.trim();
    let text = text
        .strip_prefix("CHECK")
        .or_else(|| text.strip_prefix("check"))
        .unwrap_or(text)
        .trim();
    match text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        Some(inner) => inner.trim().to_string(),
        None => text.to_string(),
    }
}

/// The tables in an order where each comes after the tables its foreign keys point to (in the
/// same snapshot); a cycle keeps the tables left in their order.
fn by_dependency<'a>(tables: &[&'a TableSnapshot]) -> Vec<&'a TableSnapshot> {
    let names: HashSet<&str> = tables.iter().map(|t| t.name.as_str()).collect();
    let mut placed: Vec<&'a TableSnapshot> = Vec::new();
    let mut left: Vec<&'a TableSnapshot> = tables.to_vec();
    while !left.is_empty() {
        let ready = left.iter().position(|t| {
            t.foreign_keys.iter().all(|k| {
                k.referenced_table == t.name
                    || !names.contains(k.referenced_table.as_str())
                    || placed.iter().any(|p| p.name == k.referenced_table)
            })
        });
        let next = left.remove(ready.unwrap_or(0));
        placed.push(next);
    }
    placed
}

/// The views in an order where each comes after the views its text names.
fn views_by_dependency<'a>(views: &[&'a ViewSnapshot], flavor: Flavor) -> Vec<&'a ViewSnapshot> {
    let mut placed: Vec<&'a ViewSnapshot> = Vec::new();
    let mut left: Vec<&'a ViewSnapshot> = views.to_vec();
    while !left.is_empty() {
        let ready = left.iter().position(|v| {
            left.iter()
                .all(|o| o.name == v.name || !ddl::mentions(&v.definition, &o.name, flavor))
        });
        let next = left.remove(ready.unwrap_or(0));
        placed.push(next);
    }
    placed
}

fn routine_kind(kind: RoutineKind) -> SchemaObjectKind {
    match kind {
        RoutineKind::Function => SchemaObjectKind::Function,
        RoutineKind::Procedure => SchemaObjectKind::Procedure,
    }
}

fn routine_label(routine: &RoutineSnapshot) -> String {
    let kind = match routine.kind {
        RoutineKind::Function => "function",
        RoutineKind::Procedure => "procedure",
    };
    match &routine.detail {
        Some(detail) => format!("{kind} {}({detail})", routine.name),
        None => format!("{kind} {}", routine.name),
    }
}

fn same_routine(a: &RoutineSnapshot, b: &RoutineSnapshot) -> bool {
    a.kind == b.kind && a.name == b.name && a.detail == b.detail
}

/// The script that makes `compared.target` like `compared.source`, written for `target`, the
/// target's session: ADR-047's alteration reads the target's catalog again, as a form does.
pub async fn migration_script(
    target: &Session,
    compared: &ComparedSchemas,
    options: &MigrationOptions,
) -> Result<MigrationScript, CompareError> {
    let dialect = target.dialect();
    let engine = dialect.name();
    let version = target
        .connection()
        .server_version()
        .await
        .unwrap_or_default();
    let (source, target_snapshot) = (&compared.source, &compared.target);
    let mut w = Writer {
        dialect: dialect.as_ref(),
        flavor: crate::objects::alter::flavor(engine),
        engine,
        mariadb: version.to_ascii_lowercase().contains("mariadb"),
        version,
        source_schema: &source.schema,
        target_schema: &target_snapshot.schema,
        sections: Vec::new(),
        out: MigrationScript {
            sql: String::new(),
            irreversible: Vec::new(),
            left_in_place: Vec::new(),
            not_scripted: Vec::new(),
        },
    };

    // What changes, by name.
    let common: Vec<(&TableSnapshot, &TableSnapshot)> = source
        .tables
        .iter()
        .filter_map(|s| table_of(target_snapshot, &s.name).map(|t| (s, t)))
        .collect();
    let new_tables: Vec<&TableSnapshot> = source
        .tables
        .iter()
        .filter(|s| table_of(target_snapshot, &s.name).is_none())
        .collect();
    let gone_tables: Vec<&TableSnapshot> = target_snapshot
        .tables
        .iter()
        .filter(|t| table_of(source, &t.name).is_none())
        .collect();
    let view_of = |snapshot: &SchemaSnapshot, name: &str| {
        snapshot.views.iter().find(|v| v.name == name).cloned()
    };
    // What the report says differs: only those views and routines are written again.
    let differs = |kind: CompareObjectKind, name: &str, detail: Option<&String>| {
        compared.report.objects.iter().any(|o| {
            o.kind == kind
                && o.name == name
                && o.detail.as_ref() == detail
                && matches!(
                    o.status,
                    ObjectStatus::OnlyInSource | ObjectStatus::Changed(_)
                )
        })
    };
    let source_views: Vec<&ViewSnapshot> = source
        .views
        .iter()
        .filter(|s| {
            let kind = if s.materialized {
                CompareObjectKind::MaterializedView
            } else {
                CompareObjectKind::View
            };
            differs(kind, &s.name, None)
        })
        .collect();

    // 1. Views and routines that go.
    w.section("Views and routines only in the target");
    for view in &target_snapshot.views {
        let in_source = view_of(source, &view.name);
        let replaced_materialized = view.materialized
            && in_source.is_some()
            && differs(CompareObjectKind::MaterializedView, &view.name, None);
        let kind = if view.materialized {
            ObjectKind::MaterializedView
        } else {
            ObjectKind::View
        };
        let label = format!("view {}", view.name);
        if in_source.is_none() && !options.drops {
            w.out.left_in_place.push(label);
        } else if in_source.is_none() || replaced_materialized {
            w.drop(
                ObjectTarget::Relation {
                    schema: target_snapshot.schema.clone(),
                    name: view.name.clone(),
                    kind,
                },
                label,
            );
        }
    }
    for routine in &target_snapshot.routines {
        if source.routines.iter().any(|s| same_routine(s, routine)) {
            continue;
        }
        let label = routine_label(routine);
        if !options.drops {
            w.out.left_in_place.push(label);
            continue;
        }
        w.drop(
            ObjectTarget::SchemaObject(SchemaObject {
                schema: target_snapshot.schema.clone(),
                name: routine.name.clone(),
                kind: routine_kind(routine.kind),
                detail: routine.detail.clone(),
                comment: None,
            }),
            label,
        );
    }

    // 2. Each common table's alteration: changed columns, the table's comment, and the keys,
    // constraints and foreign keys that go or change.
    w.section("Changed columns, and keys and constraints that go");
    for (s, t) in &common {
        let columns: Vec<ColumnEdit> = s
            .columns
            .iter()
            .filter_map(|sc| {
                let tc = t.columns.iter().find(|c| c.name == sc.name)?;
                let changed = !same_type(&sc.data_type, &tc.data_type)
                    || sc.nullable != tc.nullable
                    || typed(&sc.default) != typed(&tc.default)
                    || typed(&sc.comment) != typed(&tc.comment);
                changed.then(|| ColumnEdit {
                    name: sc.name.clone(),
                    data_type: sc.data_type.clone(),
                    nullable: sc.nullable,
                    default: sc.default.clone(),
                    comment: sc.comment.clone(),
                })
            })
            .collect();
        let mut drop = Vec::new();
        for tc in &t.constraints {
            let kept = s.constraints.iter().any(|sc| same_constraint(sc, tc));
            let in_source = s
                .constraints
                .iter()
                .any(|sc| sc.name.is_some() && sc.name == tc.name);
            if kept || (!in_source && !options.drops) {
                if !kept {
                    w.out
                        .left_in_place
                        .push(format!("constraint {} of {}", label_of(tc), t.name));
                }
                continue;
            }
            match &tc.name {
                Some(name) => drop.push(DroppedConstraint::Constraint { name: name.clone() }),
                None => w.not_scripted(
                    format!("constraint {} of {}", tc.definition, t.name),
                    "the engine keeps no name for it",
                ),
            }
        }
        let key_goes = s.primary_key != t.primary_key && !t.primary_key.is_empty();
        if key_goes && s.primary_key.is_empty() && !options.drops {
            w.out
                .left_in_place
                .push(format!("primary key of {}", t.name));
        } else if key_goes {
            match &t.primary_key_name {
                Some(name) => drop.push(DroppedConstraint::Constraint { name: name.clone() }),
                None => w.not_scripted(
                    format!("primary key of {}", t.name),
                    "the engine keeps no name for it",
                ),
            }
        }
        for tk in &t.foreign_keys {
            let kept = s
                .foreign_keys
                .iter()
                .any(|sk| same_foreign_key(sk, tk, &source.schema, &target_snapshot.schema));
            let in_source = s
                .foreign_keys
                .iter()
                .any(|sk| !sk.name.is_empty() && sk.name == tk.name);
            if kept || (!in_source && !options.drops) {
                if !kept {
                    w.out
                        .left_in_place
                        .push(format!("foreign key {} of {}", tk.name, t.name));
                }
                continue;
            }
            drop.push(DroppedConstraint::ForeignKey {
                name: tk.name.clone(),
            });
        }
        let comment = (typed(&s.comment) != typed(&t.comment)).then(|| s.comment.clone());
        if columns.is_empty() && drop.is_empty() && comment.is_none() {
            continue;
        }
        let alteration = TableAlteration {
            schema: target_snapshot.schema.clone(),
            table: t.name.clone(),
            columns,
            drop,
            comment,
        };
        match alteration_plan(target, &alteration).await {
            Ok(plan) => {
                if plan.rebuild {
                    w.out
                        .irreversible
                        .push(format!("rebuild of table {}", t.name));
                }
                for statement in plan.statements() {
                    w.push(statement);
                }
            }
            Err(e) => w.not_scripted(format!("table {}", t.name), e),
        }
    }

    // 3. Indexes that go or change.
    w.section("Indexes that go");
    for (s, t) in &common {
        for ti in &t.indexes {
            let source_index = s.indexes.iter().find(|si| si.name == ti.name);
            let changed = source_index.is_some_and(|si| !same_index(si, ti, w.flavor));
            let kept_elsewhere =
                source_index.is_none() && s.indexes.iter().any(|si| same_index(si, ti, w.flavor));
            if kept_elsewhere || (source_index.is_some() && !changed) {
                continue;
            }
            if source_index.is_none() && !options.drops {
                w.out
                    .left_in_place
                    .push(format!("index {} of {}", ti.name, t.name));
                continue;
            }
            w.drop(
                ObjectTarget::Index {
                    schema: target_snapshot.schema.clone(),
                    table: t.name.clone(),
                    name: ti.name.clone(),
                    primary: false,
                },
                format!("index {}", ti.name),
            );
        }
    }

    // 4. Tables that go, those that point to others first.
    w.section("Tables only in the target");
    let mut gone = by_dependency(&gone_tables);
    gone.reverse();
    for table in gone {
        let label = format!("table {}", table.name);
        if !options.drops {
            w.out.left_in_place.push(label);
            continue;
        }
        w.drop(
            ObjectTarget::Relation {
                schema: target_snapshot.schema.clone(),
                name: table.name.clone(),
                kind: ObjectKind::Table,
            },
            label,
        );
    }

    // 5. New tables, each after the tables it points to.
    w.section("Tables only in the source");
    let mut later_keys: Vec<(String, ForeignKeyInfo)> = Vec::new();
    for table in by_dependency(&new_tables) {
        match (&table.kept_definition, engine) {
            // The engine's own text keeps everything the table is: renamed into the target.
            (Some(kept), "sqlite" | "mysql" | "duckdb") => {
                let name = w.target_name(&table.name);
                let moved = ddl::rename_created(kept, w.flavor, &name, None).map(|sql| {
                    if w.source_schema == w.target_schema {
                        sql
                    } else {
                        ddl::requalified(&sql, w.flavor, w.source_schema, &w.quote(w.target_schema))
                            .unwrap_or(sql)
                    }
                });
                match moved {
                    Ok(sql) => w.push(sql),
                    Err(e) => {
                        w.not_scripted(format!("table {}", table.name), e);
                        continue;
                    }
                }
                // MySQL's text has its indexes; SQLite's and DuckDB's do not.
                if engine != "mysql" {
                    for index in &table.indexes {
                        w.index_statement(&table.name, index);
                    }
                }
            }
            _ => {
                let new = NewTable {
                    schema: target_snapshot.schema.clone(),
                    name: table.name.clone(),
                    columns: table
                        .columns
                        .iter()
                        .map(|c| NewColumn {
                            name: c.name.clone(),
                            data_type: c.data_type.clone(),
                            nullable: c.nullable,
                            default: c.default.clone(),
                            comment: c.comment.clone(),
                        })
                        .collect(),
                    primary_key: table.primary_key.clone(),
                    foreign_keys: Vec::new(),
                    indexes: Vec::new(),
                };
                match create_table_plan(w.dialect, &new) {
                    Ok(plan) => {
                        for statement in plan.statements() {
                            w.push(statement);
                        }
                    }
                    Err(e) => {
                        w.not_scripted(format!("table {}", table.name), e);
                        continue;
                    }
                }
                for constraint in &table.constraints {
                    let element = Writer::constraint_element(constraint);
                    w.element(
                        &table.name,
                        &element,
                        format!("constraint {} of {}", label_of(constraint), table.name),
                    );
                }
                for index in &table.indexes {
                    w.index_statement(&table.name, index);
                }
                for key in &table.foreign_keys {
                    later_keys.push((table.name.clone(), key.clone()));
                }
            }
        }
    }

    // 6. Columns that go and come.
    w.section("Columns that go and come");
    for (s, t) in &common {
        for tc in &t.columns {
            if s.columns.iter().any(|c| c.name == tc.name) {
                continue;
            }
            let label = format!("column {}.{}", t.name, tc.name);
            if !options.drops {
                w.out.left_in_place.push(label);
                continue;
            }
            w.drop(
                ObjectTarget::Column {
                    schema: target_snapshot.schema.clone(),
                    table: t.name.clone(),
                    name: tc.name.clone(),
                },
                label,
            );
        }
        for sc in &s.columns {
            if t.columns.iter().any(|c| c.name == sc.name) {
                continue;
            }
            let element = NewTableElement::Column {
                name: sc.name.clone(),
                data_type: sc.data_type.clone(),
                nullable: sc.nullable,
                default: sc.default.clone().unwrap_or_default(),
            };
            w.element(&t.name, &element, format!("column {}.{}", s.name, sc.name));
        }
    }

    // 7. Keys, constraints and indexes that come or changed.
    w.section("Keys, constraints and indexes that come");
    for (s, t) in &common {
        if s.primary_key != t.primary_key && !s.primary_key.is_empty() {
            let element = NewTableElement::Constraint {
                name: s
                    .primary_key_name
                    .clone()
                    .unwrap_or_else(|| format!("{}_pkey", s.name)),
                kind: "primaryKey".into(),
                columns: s.primary_key.clone(),
                expression: String::new(),
            };
            w.element(&t.name, &element, format!("primary key of {}", t.name));
        }
        for sc in &s.constraints {
            if t.constraints.iter().any(|tc| same_constraint(sc, tc)) {
                continue;
            }
            let element = Writer::constraint_element(sc);
            w.element(
                &t.name,
                &element,
                format!("constraint {} of {}", label_of(sc), t.name),
            );
        }
        for si in &s.indexes {
            if t.indexes.iter().any(|ti| same_index(si, ti, w.flavor)) {
                continue;
            }
            w.index_statement(&t.name, si);
        }
        for sk in &s.foreign_keys {
            let kept = t
                .foreign_keys
                .iter()
                .any(|tk| same_foreign_key(sk, tk, &source.schema, &target_snapshot.schema));
            if !kept {
                later_keys.push((t.name.clone(), sk.clone()));
            }
        }
    }

    // 8. Foreign keys, once every table they point to exists.
    w.section("Foreign keys that come");
    for (table, key) in later_keys {
        let element = w.foreign_key_element(&key);
        w.element(
            &table,
            &element,
            format!("foreign key {} of {}", key.name, table),
        );
    }

    // 9. The source's views and routines, each view after the views its text names.
    w.section("Views and routines of the source");
    for view in views_by_dependency(&source_views, w.flavor) {
        let moved = w.moved_definition(&view.definition, &view.name);
        let qualified = w.target_name(&view.name);
        if view.materialized {
            w.push(moved);
            continue;
        }
        match replace_form(engine, w.mariadb, &moved, None, &qualified) {
            Ok(sql) => w.push(sql),
            Err(e) => w.not_scripted(format!("view {}", view.name), e),
        }
    }
    for routine in &source.routines {
        if !differs(
            routine.kind.compared(),
            &routine.name,
            routine.detail.as_ref(),
        ) {
            continue;
        }
        let moved = w.moved_definition(&routine.definition, &routine.name);
        let qualified = w.target_name(&routine.name);
        match replace_form(
            engine,
            w.mariadb,
            &moved,
            Some(routine_kind(routine.kind)),
            &qualified,
        ) {
            Ok(sql) => w.push(sql),
            Err(e) => w.not_scripted(routine_label(routine), e),
        }
    }

    w.out.sql = render(&w, compared);
    Ok(w.out)
}

fn label_of(c: &ConstraintSnapshot) -> String {
    c.name.clone().unwrap_or_else(|| c.definition.clone())
}

fn blanks(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn same_constraint(a: &ConstraintSnapshot, b: &ConstraintSnapshot) -> bool {
    a.kind == b.kind && a.columns == b.columns && blanks(&a.definition) == blanks(&b.definition)
}

fn same_index(a: &IndexSnapshot, b: &IndexSnapshot, flavor: Flavor) -> bool {
    let shape = |i: &IndexSnapshot| {
        let shaped = ddl::rename_created(&i.definition, flavor, "\u{1}", Some("\u{2}"))
            .unwrap_or_else(|_| i.definition.clone());
        blanks(shaped.trim_end_matches(';'))
    };
    a.unique == b.unique && shape(a) == shape(b)
}

fn same_foreign_key(
    s: &ForeignKeyInfo,
    t: &ForeignKeyInfo,
    source_schema: &str,
    target_schema: &str,
) -> bool {
    let referenced = |k: &ForeignKeyInfo, own: &str| {
        if k.referenced_schema.is_empty() || k.referenced_schema == own {
            String::new()
        } else {
            k.referenced_schema.clone()
        }
    };
    s.columns == t.columns
        && s.referenced_table == t.referenced_table
        && s.referenced_columns == t.referenced_columns
        && referenced(s, source_schema) == referenced(t, target_schema)
}

/// The script as it opens: a header saying what it was written from and what it cannot undo,
/// then each section that has statements.
fn render(w: &Writer<'_>, compared: &ComparedSchemas) -> String {
    let mut sql = String::new();
    sql.push_str(&format!(
        "-- Makes schema {} like schema {} (schema compare, ADR-070).\n",
        compared.target.schema, compared.source.schema
    ));
    sql.push_str("-- Review it before running it: nothing here has run.\n");
    if !w.out.irreversible.is_empty() {
        sql.push_str("-- Cannot be undone:\n");
        for item in &w.out.irreversible {
            sql.push_str(&format!("--   {item}\n"));
        }
    }
    if !w.out.left_in_place.is_empty() {
        sql.push_str("-- Only in the target, left in place:\n");
        for item in &w.out.left_in_place {
            sql.push_str(&format!("--   {item}\n"));
        }
    }
    if !w.out.not_scripted.is_empty() {
        sql.push_str("-- Not scripted here:\n");
        for item in &w.out.not_scripted {
            sql.push_str(&format!("--   {}: {}\n", item.object, item.reason));
        }
    }
    sql.push_str(
        "-- Not compared: foreign key ON DELETE / ON UPDATE, identity and collations, column \
         order, sequences, triggers and types.\n",
    );
    for (title, pieces) in &w.sections {
        if pieces.is_empty() {
            continue;
        }
        sql.push_str(&format!("\n-- {title}\n"));
        // SQL Server keeps a batch's whole text as its view's or routine's definition: the
        // section's comment is a batch of its own.
        if w.engine == "sqlserver" {
            sql.push_str("GO\n");
        }
        for piece in pieces {
            sql.push_str(piece);
            sql.push('\n');
            // T-SQL wants `CREATE VIEW`, `CREATE FUNCTION`… first in their batch: every piece is
            // a batch of its own (ADR-017), which a transaction spans.
            if w.engine == "sqlserver" {
                sql.push_str("GO\n");
            }
        }
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_check_loses_only_its_keyword_and_outer_parentheses() {
        assert_eq!(check_expression("CHECK ((price > 0))"), "(price > 0)");
        assert_eq!(check_expression("CHECK (price > 0)"), "price > 0");
        assert_eq!(check_expression("check ([id]>(0))"), "[id]>(0)");
    }

    #[test]
    fn tables_come_after_the_tables_they_point_to() {
        let table = |name: &str, points_to: &[&str]| TableSnapshot {
            name: name.into(),
            comment: None,
            columns: Vec::new(),
            primary_key: Vec::new(),
            primary_key_name: None,
            constraints: Vec::new(),
            indexes: Vec::new(),
            foreign_keys: points_to
                .iter()
                .map(|t| ForeignKeyInfo {
                    name: format!("{name}_{t}"),
                    columns: vec!["x".into()],
                    referenced_schema: String::new(),
                    referenced_table: (*t).into(),
                    referenced_columns: vec!["id".into()],
                })
                .collect(),
            kept_definition: None,
        };
        let (a, b, c) = (table("a", &["b"]), table("b", &["c", "b"]), table("c", &[]));
        let order: Vec<&str> = by_dependency(&[&a, &b, &c])
            .iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(order, ["c", "b", "a"]);
    }
}
