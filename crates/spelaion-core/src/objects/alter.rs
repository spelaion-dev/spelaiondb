//! Structure editing with forms (ADR-047): the columns of a table changed in a form (type,
//! nullability, default, comment) and the constraints dropped with them become one script,
//! written here from the catalog read again each time, shown in full, and applied together
//! only as it was shown. SQLite, which cannot change a column in place, rebuilds the table
//! (`rebuild.rs`).

use std::collections::HashMap;

use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, TableInfo, TransactionState, Value};

use super::ddl::{self, ColumnRewrite, Flavor};
use super::rebuild;
use super::{ObjectError, ObjectTarget, qualified_name};
use crate::edit::run_one;
use crate::generate::{scalar_metadata, short_rows};
use crate::guard::{Risk, script_risks};
use crate::session::Session;

/// A column as a structure form shows it: each field is SQL as the engine writes it (the
/// default an expression, `'text'` with its quotes), so what is typed back is SQL too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    /// A generated (computed) column: its type and default are not changed here.
    pub generated: bool,
}

/// The columns of a table for its structure form, and what this engine can do with them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStructure {
    pub columns: Vec<EditableColumn>,
    /// Columns have comments (not SQLite).
    pub comments: bool,
    /// The changes are applied in one transaction (not MySQL or MariaDB, whose DDL commits).
    pub transactional: bool,
    /// Changing a column creates the table again (SQLite).
    pub rebuilds: bool,
}

/// A column's new state, as the form holds it. Fields equal to the catalog's are not changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnEdit {
    /// The column's current name.
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
}

/// A constraint of the table to drop with the other changes, by its name in the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DroppedConstraint {
    /// A primary key, unique or check constraint.
    Constraint {
        name: String,
    },
    ForeignKey {
        name: String,
    },
}

/// What a structure form asks for on one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableAlteration {
    pub schema: String,
    pub table: String,
    pub columns: Vec<ColumnEdit>,
    pub drop: Vec<DroppedConstraint>,
}

/// One statement of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Run(String),
    /// A count that must be zero, or the plan stops (SQLite's foreign key check after a
    /// rebuild).
    ExpectZero {
        sql: String,
        problem: String,
    },
}

impl Step {
    fn sql(&self) -> &str {
        match self {
            Self::Run(sql) | Self::ExpectZero { sql, .. } => sql,
        }
    }
}

/// The statements of a change, as they will run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterationPlan {
    /// Before the transaction (SQLite's `PRAGMA foreign_keys = OFF`).
    pub(crate) before: Vec<String>,
    pub(crate) steps: Vec<Step>,
    /// After it, also when it failed (`PRAGMA foreign_keys = ON`).
    pub(crate) after: Vec<String>,
    /// The engine's `BEGIN`, when the steps run in one transaction.
    pub(crate) begin: Option<&'static str>,
    /// A first line saying what the script does, when that is not obvious.
    pub(crate) note: Option<&'static str>,
    pub(crate) rebuild: bool,
}

/// A statement of the script that needs confirmation (`guard.rs`), by its position (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlanRisk {
    pub statement: usize,
    pub risk: Risk,
}

impl AlterationPlan {
    /// Every statement in the order it runs.
    pub fn statements(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.before.iter().map(String::as_str).collect();
        if let Some(begin) = self.begin {
            out.push(begin);
        }
        out.extend(self.steps.iter().map(Step::sql));
        if self.begin.is_some() {
            out.push("COMMIT");
        }
        out.extend(self.after.iter().map(String::as_str));
        out
    }

    /// The script to show, or to open in the editor: what runs, each statement ending in `;`.
    pub fn script(&self) -> String {
        let mut script = String::new();
        if let Some(note) = self.note {
            script.push_str("-- ");
            script.push_str(note);
            script.push('\n');
        }
        for statement in self.statements() {
            script.push_str(statement);
            script.push_str(";\n");
        }
        script
    }

    pub fn statement_count(&self) -> usize {
        self.statements().len()
    }

    /// Whether the changes run in one transaction.
    pub fn transactional(&self) -> bool {
        self.begin.is_some()
    }

    /// Whether the table is created again (SQLite).
    pub fn rebuilds(&self) -> bool {
        self.rebuild
    }

    /// The statements that need confirmation under the connection's safeguards.
    pub fn risks(&self, dialect: &dyn Dialect, safeguards: &Safeguards) -> Vec<PlanRisk> {
        self.statements()
            .into_iter()
            .enumerate()
            .filter(|(_, sql)| !matches!(*sql, "BEGIN" | "BEGIN TRANSACTION" | "COMMIT"))
            .filter_map(|(i, sql)| {
                script_risks(dialect, sql, safeguards)
                    .first()
                    .map(|risky| PlanRisk {
                        statement: i + 1,
                        risk: risky.risk,
                    })
            })
            .collect()
    }
}

/// An SQL string literal with its quotes doubled.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A T-SQL Unicode string literal.
fn nstring(text: &str) -> String {
    format!("N'{}'", text.replace('\'', "''"))
}

fn flavor(engine: &str) -> Flavor {
    if engine == "mysql" {
        Flavor::Mysql
    } else {
        Flavor::Sqlite
    }
}

/// What a form typed: trimmed, and empty as nothing.
fn typed(text: &Option<String>) -> Option<String> {
    text.as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
}

/// Types compared as SQL: case and runs of blanks do not count.
fn same_type(a: &str, b: &str) -> bool {
    let normal = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    normal(a) == normal(b)
}

fn failed(message: impl Into<String>) -> ObjectError {
    ObjectError::Failed {
        index: 0,
        total: 0,
        message: message.into(),
    }
}

/// What the catalog says of a table, read again for every preview and every apply.
pub(crate) struct Current {
    pub info: TableInfo,
    /// The text the engine keeps for the table (SQLite, MySQL and MariaDB).
    pub ddl: Option<String>,
    pub columns: Vec<EditableColumn>,
}

async fn read_current(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<Current, ObjectError> {
    let dialect = session.dialect();
    let engine = dialect.name();
    let info = session
        .connection()
        .describe_table(schema, table)
        .await
        .map_err(|e| failed(e.to_string()))?;
    let ddl = match engine {
        "sqlite" => Some(rebuild::table_sql(session, schema, table).await?),
        "mysql" => Some(
            session
                .connection()
                .relation_definition(schema, table)
                .await
                .map_err(|e| failed(e.to_string()))?,
        ),
        _ => None,
    };
    let columns = match &ddl {
        Some(sql) => columns_from_ddl(sql, flavor(engine), &info)?,
        None => info
            .columns
            .iter()
            .map(|c| EditableColumn {
                name: c.name.clone(),
                data_type: c.native_type.clone(),
                nullable: c.nullable,
                default: c.default.clone(),
                comment: c.comment.clone().filter(|c| !c.is_empty()),
                generated: false,
            })
            .collect(),
    };
    Ok(Current { info, ddl, columns })
}

/// The columns as the table's own text writes them: the type and default exactly as stored,
/// nullability and comments from the catalog.
fn columns_from_ddl(
    sql: &str,
    flavor: Flavor,
    info: &TableInfo,
) -> Result<Vec<EditableColumn>, ObjectError> {
    let unreadable = |e: ddl::DdlError| ObjectError::Unsupported(e.to_string());
    let create = ddl::parse_create_table(sql, flavor).map_err(unreadable)?;
    let mut columns = Vec::new();
    for range in &create.elements {
        let element = &sql[range.clone()];
        if ddl::is_table_constraint(element, flavor) {
            continue;
        }
        let def = ddl::parse_column(element, flavor).map_err(unreadable)?;
        let catalog = info.columns.iter().find(|c| c.name == def.name);
        columns.push(EditableColumn {
            data_type: element[def.type_range.clone()].to_string(),
            nullable: catalog.map_or(def.clause(ddl::ClauseKind::NotNull).is_none(), |c| {
                c.nullable
            }),
            default: def.default(element).map(String::from),
            comment: catalog
                .and_then(|c| c.comment.clone())
                .filter(|c| !c.is_empty()),
            generated: def.generated(),
            name: def.name,
        });
    }
    Ok(columns)
}

/// A table's columns for its structure form.
pub async fn table_structure(
    session: &Session,
    schema: &str,
    table: &str,
) -> Result<TableStructure, ObjectError> {
    let engine = session.dialect().name();
    let current = read_current(session, schema, table).await?;
    Ok(TableStructure {
        columns: current.columns,
        comments: engine != "sqlite",
        transactional: engine != "mysql",
        rebuilds: engine == "sqlite",
    })
}

/// One column's change against the catalog.
pub(crate) struct ColumnDiff<'a> {
    pub current: &'a EditableColumn,
    pub data_type: Option<String>,
    pub nullable: Option<bool>,
    pub default: Option<Option<String>>,
    pub comment: Option<Option<String>>,
}

impl ColumnDiff<'_> {
    fn is_empty(&self) -> bool {
        self.data_type.is_none()
            && self.nullable.is_none()
            && self.default.is_none()
            && self.comment.is_none()
    }
}

fn diff<'a>(
    current: &'a [EditableColumn],
    edits: &[ColumnEdit],
    engine: &str,
) -> Result<Vec<ColumnDiff<'a>>, ObjectError> {
    let flavor = flavor(engine);
    let mut out = Vec::new();
    for edit in edits {
        let column = current
            .iter()
            .find(|c| c.name == edit.name)
            .ok_or_else(|| ObjectError::Gone(edit.name.clone()))?;
        let data_type = edit.data_type.trim();
        let data_type = (!same_type(data_type, &column.data_type)).then(|| data_type.to_string());
        if let Some(ty) = &data_type
            && (ty.is_empty() && engine != "sqlite"
                || !ty.is_empty() && !ddl::is_single_expression(ty, flavor))
        {
            return Err(ObjectError::Unsupported(format!(
                "the type of {} must be one type, such as varchar(80)",
                edit.name
            )));
        }
        let default = typed(&edit.default);
        let default = (default != typed(&column.default)).then_some(default);
        if let Some(Some(value)) = &default
            && !ddl::is_single_expression(value, flavor)
        {
            return Err(ObjectError::Unsupported(format!(
                "the default of {} must be one SQL expression, such as 0 or 'text'",
                edit.name
            )));
        }
        let comment = typed(&edit.comment);
        let comment = (comment != typed(&column.comment)).then_some(comment);
        if comment.is_some() && engine == "sqlite" {
            return Err(ObjectError::Unsupported(
                "SQLite columns have no comments".into(),
            ));
        }
        if column.generated && (data_type.is_some() || default.is_some()) {
            return Err(ObjectError::Unsupported(format!(
                "{} is a generated column: its type and expression are changed in its SQL",
                edit.name
            )));
        }
        let change = ColumnDiff {
            current: column,
            data_type,
            nullable: (edit.nullable != column.nullable).then_some(edit.nullable),
            default,
            comment,
        };
        if !change.is_empty() {
            out.push(change);
        }
    }
    Ok(out)
}

/// The script for a structure form, written from the catalog as it is now.
pub async fn alteration_plan(
    session: &Session,
    alteration: &TableAlteration,
) -> Result<AlterationPlan, ObjectError> {
    let dialect = session.dialect();
    let dialect = dialect.as_ref();
    let engine = dialect.name();
    let current = read_current(session, &alteration.schema, &alteration.table).await?;
    let changes = diff(&current.columns, &alteration.columns, engine)?;
    if changes.is_empty() && alteration.drop.is_empty() {
        return Err(ObjectError::Unsupported("nothing has changed".into()));
    }
    let (schema, table) = (alteration.schema.as_str(), alteration.table.as_str());
    let target = qualified_name(dialect, schema, &[table.to_string()]);
    let mut steps = Vec::new();
    match engine {
        "sqlite" => {
            let rewrites = changes
                .iter()
                .map(|c| (c.current.name.clone(), rewrite(c, false)))
                .collect::<HashMap<_, _>>();
            return rebuild::plan(
                session,
                schema,
                table,
                &current,
                &rewrites,
                &alteration.drop,
            )
            .await;
        }
        "duckdb" if !alteration.drop.is_empty() => {
            return Err(ObjectError::Unsupported(
                "DuckDB cannot drop a constraint from an existing table".into(),
            ));
        }
        _ => {}
    }
    for dropped in &alteration.drop {
        let target = dropped_target(&current.info, schema, table, dropped)?;
        let statements = super::change_statements(
            dialect,
            &super::ObjectChange::Drop {
                target,
                cascade: false,
            },
        )?;
        steps.extend(statements.into_iter().map(Step::Run));
    }
    match engine {
        "postgres" => postgres(dialect, &target, schema, table, &changes, &mut steps),
        "mysql" => mysql(&current, &target, &changes, &mut steps)?,
        "sqlserver" => sqlserver(session, &target, schema, table, &changes, &mut steps).await?,
        _ => duckdb(dialect, &target, schema, table, &changes, &mut steps),
    }
    let begin = match engine {
        "mysql" => None,
        _ if steps.len() < 2 => None,
        "sqlserver" => Some("BEGIN TRANSACTION"),
        _ => Some("BEGIN"),
    };
    Ok(AlterationPlan {
        before: Vec::new(),
        steps,
        after: Vec::new(),
        begin,
        note: None,
        rebuild: false,
    })
}

/// The explorer's target for a constraint the form drops, checked against the catalog.
fn dropped_target(
    info: &TableInfo,
    schema: &str,
    table: &str,
    dropped: &DroppedConstraint,
) -> Result<ObjectTarget, ObjectError> {
    match dropped {
        DroppedConstraint::Constraint { name } => {
            let constraint = info
                .constraints
                .iter()
                .find(|c| c.name.as_deref() == Some(name.as_str()))
                .ok_or_else(|| ObjectError::Gone(name.clone()))?;
            Ok(ObjectTarget::Constraint {
                schema: schema.into(),
                table: table.into(),
                name: name.clone(),
                kind: constraint.kind,
            })
        }
        DroppedConstraint::ForeignKey { name } => {
            if !info.foreign_keys.iter().any(|fk| &fk.name == name) {
                return Err(ObjectError::Gone(name.clone()));
            }
            Ok(ObjectTarget::ForeignKey {
                schema: schema.into(),
                table: table.into(),
                name: name.clone(),
            })
        }
    }
}

/// Whether a type holds text, and so takes a character set or a collation.
fn textual(ty: &str) -> bool {
    let first = ty
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        first.as_str(),
        "char"
            | "varchar"
            | "tinytext"
            | "text"
            | "mediumtext"
            | "longtext"
            | "enum"
            | "set"
            | "national"
            | "nchar"
            | "nvarchar"
            | "ntext"
            | "character"
    )
}

/// A column change as a rewrite of its definition (SQLite, MySQL and MariaDB).
fn rewrite(change: &ColumnDiff<'_>, mysql: bool) -> ColumnRewrite {
    ColumnRewrite {
        data_type: change.data_type.clone(),
        nullable: change.nullable,
        default: change.default.clone(),
        comment: change.comment.clone(),
        drop_charset: mysql && change.data_type.as_deref().is_some_and(|ty| !textual(ty)),
    }
}

fn postgres(
    dialect: &dyn Dialect,
    target: &str,
    schema: &str,
    table: &str,
    changes: &[ColumnDiff<'_>],
    steps: &mut Vec<Step>,
) {
    let mut actions = Vec::new();
    for change in changes {
        let column = dialect.quote_identifier(&change.current.name);
        if let Some(ty) = &change.data_type {
            // USING converts what has no implicit cast (text to integer…).
            actions.push(format!(
                "ALTER COLUMN {column} TYPE {ty} USING {column}::{ty}"
            ));
        }
        match change.nullable {
            Some(false) => actions.push(format!("ALTER COLUMN {column} SET NOT NULL")),
            Some(true) => actions.push(format!("ALTER COLUMN {column} DROP NOT NULL")),
            None => {}
        }
        match &change.default {
            Some(Some(value)) => actions.push(format!("ALTER COLUMN {column} SET DEFAULT {value}")),
            Some(None) => actions.push(format!("ALTER COLUMN {column} DROP DEFAULT")),
            None => {}
        }
    }
    if !actions.is_empty() {
        steps.push(Step::Run(format!(
            "ALTER TABLE {target}\n  {}",
            actions.join(",\n  ")
        )));
    }
    comments_on(dialect, schema, table, changes, steps);
}

/// `COMMENT ON COLUMN` (PostgreSQL, DuckDB).
fn comments_on(
    dialect: &dyn Dialect,
    schema: &str,
    table: &str,
    changes: &[ColumnDiff<'_>],
    steps: &mut Vec<Step>,
) {
    for change in changes {
        if let Some(comment) = &change.comment {
            let column = qualified_name(
                dialect,
                schema,
                &[table.to_string(), change.current.name.clone()],
            );
            let value = comment.as_deref().map_or_else(|| "NULL".into(), literal);
            steps.push(Step::Run(format!("COMMENT ON COLUMN {column} IS {value}")));
        }
    }
}

fn duckdb(
    dialect: &dyn Dialect,
    target: &str,
    schema: &str,
    table: &str,
    changes: &[ColumnDiff<'_>],
    steps: &mut Vec<Step>,
) {
    // DuckDB takes one action per ALTER TABLE.
    for change in changes {
        let column = dialect.quote_identifier(&change.current.name);
        let alter = |action: String| {
            Step::Run(format!(
                "ALTER TABLE {target} ALTER COLUMN {column} {action}"
            ))
        };
        if let Some(ty) = &change.data_type {
            steps.push(alter(format!("SET DATA TYPE {ty}")));
        }
        match change.nullable {
            Some(false) => steps.push(alter("SET NOT NULL".into())),
            Some(true) => steps.push(alter("DROP NOT NULL".into())),
            None => {}
        }
        match &change.default {
            Some(Some(value)) => steps.push(alter(format!("SET DEFAULT {value}"))),
            Some(None) => steps.push(alter("DROP DEFAULT".into())),
            None => {}
        }
    }
    comments_on(dialect, schema, table, changes, steps);
}

fn mysql(
    current: &Current,
    target: &str,
    changes: &[ColumnDiff<'_>],
    steps: &mut Vec<Step>,
) -> Result<(), ObjectError> {
    let sql = current.ddl.as_deref().unwrap_or_default();
    let unreadable = |e: ddl::DdlError| ObjectError::Unsupported(e.to_string());
    let create = ddl::parse_create_table(sql, Flavor::Mysql).map_err(unreadable)?;
    let mut modify = Vec::new();
    for change in changes {
        let element = create
            .elements
            .iter()
            .map(|range| &sql[range.clone()])
            .filter(|element| !ddl::is_table_constraint(element, Flavor::Mysql))
            .find_map(|element| {
                let def = ddl::parse_column(element, Flavor::Mysql).ok()?;
                (def.name == change.current.name).then_some((element, def))
            })
            .ok_or_else(|| ObjectError::Gone(change.current.name.clone()))?;
        let (element, def) = element;
        // MODIFY restates the whole column, so every attribute it had is written again.
        modify.push(format!(
            "MODIFY COLUMN {}",
            ddl::rewrite_column(element, &def, &rewrite(change, true))
        ));
    }
    if !modify.is_empty() {
        steps.push(Step::Run(format!(
            "ALTER TABLE {target}\n  {}",
            modify.join(",\n  ")
        )));
    }
    Ok(())
}

async fn sqlserver(
    session: &Session,
    target: &str,
    schema: &str,
    table: &str,
    changes: &[ColumnDiff<'_>],
    steps: &mut Vec<Step>,
) -> Result<(), ObjectError> {
    let dialect = session.dialect();
    let dialect = dialect.as_ref();
    for change in changes {
        let name = &change.current.name;
        let column = dialect.quote_identifier(name);
        let retyped = change.data_type.is_some() || change.nullable.is_some();
        // A column's default is a constraint with a name; a type change must drop it first.
        let constraint = if change.default.is_some() || retyped && change.current.default.is_some()
        {
            default_constraint(session, target, name).await?
        } else {
            None
        };
        if let Some(constraint) = &constraint {
            steps.push(Step::Run(format!(
                "ALTER TABLE {target} DROP CONSTRAINT {}",
                dialect.quote_identifier(constraint)
            )));
        }
        if retyped {
            let ty = change
                .data_type
                .clone()
                .unwrap_or_else(|| change.current.data_type.clone());
            // ALTER COLUMN restates the nullability, or the column becomes nullable, and the
            // collation, or it becomes the database's.
            let nullable = change.nullable.unwrap_or(change.current.nullable);
            let collate = if textual(&ty) {
                collation(session, target, name)
                    .await?
                    .map(|c| format!(" COLLATE {c}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            steps.push(Step::Run(format!(
                "ALTER TABLE {target} ALTER COLUMN {column} {ty}{collate} {}",
                if nullable { "NULL" } else { "NOT NULL" }
            )));
        }
        let default = match &change.default {
            Some(value) => value.clone(),
            None if constraint.is_some() => change.current.default.clone(),
            None => None,
        };
        if let Some(value) = default
            && (change.default.is_some() || constraint.is_some())
        {
            let constraint = constraint
                .clone()
                .unwrap_or_else(|| format!("DF_{table}_{name}"));
            steps.push(Step::Run(format!(
                "ALTER TABLE {target} ADD CONSTRAINT {} DEFAULT {value} FOR {column}",
                dialect.quote_identifier(&constraint)
            )));
        }
        if let Some(comment) = &change.comment {
            let procedure = match (comment, &change.current.comment) {
                (Some(_), Some(_)) => "sp_updateextendedproperty",
                (Some(_), None) => "sp_addextendedproperty",
                (None, _) => "sp_dropextendedproperty",
            };
            let value = comment
                .as_deref()
                .map(|c| format!(", @value = {}", nstring(c)))
                .unwrap_or_default();
            steps.push(Step::Run(format!(
                "EXEC {procedure} @name = N'MS_Description'{value}, @level0type = N'SCHEMA', @level0name = {}, @level1type = N'TABLE', @level1name = {}, @level2type = N'COLUMN', @level2name = {}",
                nstring(schema),
                nstring(table),
                nstring(name)
            )));
        }
    }
    Ok(())
}

/// A SQL Server column's collation, `None` for a type without one.
async fn collation(
    session: &Session,
    target: &str,
    column: &str,
) -> Result<Option<String>, ObjectError> {
    let sql = format!(
        "SELECT collation_name FROM sys.columns WHERE object_id = OBJECT_ID({}) AND name = {}",
        nstring(target),
        nstring(column)
    );
    match scalar_metadata(session, &sql).await.map_err(failed)? {
        // A collation's name is a word: anything else would not be one.
        Some(Value::Text(name)) if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            Ok(Some(name))
        }
        _ => Ok(None),
    }
}

/// The name of a SQL Server column's default constraint.
async fn default_constraint(
    session: &Session,
    target: &str,
    column: &str,
) -> Result<Option<String>, ObjectError> {
    let sql = format!(
        "SELECT dc.name FROM sys.default_constraints dc \
         JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({}) AND c.name = {}",
        nstring(target),
        nstring(column)
    );
    match scalar_metadata(session, &sql).await.map_err(failed)? {
        Some(Value::Text(name)) => Ok(Some(name)),
        _ => Ok(None),
    }
}

/// Runs a structure form's changes, never on a read-only connection, and only as the script
/// the user was shown: the core writes it again from the catalog, and a table that changed in
/// between is refused. Statements that need confirmation (a rebuild's `DROP TABLE`, any change
/// on Production) run only confirmed. Returns how many statements ran.
pub async fn apply_alteration(
    session: &Session,
    alteration: &TableAlteration,
    shown: &str,
    read_only: bool,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<usize, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let plan = alteration_plan(session, alteration).await?;
    if plan.script() != shown {
        return Err(ObjectError::Changed);
    }
    if !confirmed
        && !plan
            .risks(session.dialect().as_ref(), safeguards)
            .is_empty()
    {
        return Err(ObjectError::NeedsConfirmation);
    }
    run_plan(session, &plan).await
}

/// Runs a plan's statements in order; in a transaction, the first failure rolls it back.
pub(crate) async fn run_plan(
    session: &Session,
    plan: &AlterationPlan,
) -> Result<usize, ObjectError> {
    // Never nested in, nor committing, a transaction the user left open on this session.
    if let Ok(TransactionState::Open | TransactionState::Failed) =
        session.connection().transaction_state().await
    {
        return Err(ObjectError::TransactionOpen);
    }
    let total = plan.statement_count();
    let mut index = 0;
    let fail = |index: usize, message: String| ObjectError::Failed {
        index,
        total,
        message,
    };
    let after = |error: ObjectError| async move {
        for statement in &plan.after {
            let _ = run_one(session, statement).await;
        }
        error
    };
    for statement in &plan.before {
        index += 1;
        if let Err(message) = run_one(session, statement).await {
            return Err(after(fail(index, message)).await);
        }
    }
    if let Some(begin) = plan.begin {
        index += 1;
        if let Err(message) = run_one(session, begin).await {
            return Err(after(fail(index, message)).await);
        }
    }
    for step in &plan.steps {
        index += 1;
        let outcome = match step {
            Step::Run(sql) => run_one(session, sql).await.map(|_| ()),
            Step::ExpectZero { sql, problem } => match scalar_metadata(session, sql).await {
                Ok(Some(Value::Int(0))) => Ok(()),
                Ok(_) => Err(problem.clone()),
                Err(message) => Err(message),
            },
        };
        if let Err(message) = outcome {
            if plan.begin.is_some() {
                // Best effort: if even the rollback fails, the session closes the transaction.
                let _ = run_one(session, "ROLLBACK").await;
            }
            return Err(after(fail(index, message)).await);
        }
    }
    if plan.begin.is_some() {
        index += 1;
        if let Err(message) = run_one(session, "COMMIT").await {
            let _ = run_one(session, "ROLLBACK").await;
            return Err(after(fail(index, message)).await);
        }
    }
    for statement in &plan.after {
        index += 1;
        run_one(session, statement)
            .await
            .map_err(|message| fail(index, message))?;
    }
    Ok(index)
}

/// Rows of a short catalog read, for `rebuild.rs`.
pub(crate) async fn catalog_rows(
    session: &Session,
    sql: &str,
) -> Result<Vec<Vec<Value>>, ObjectError> {
    short_rows(session, sql, true)
        .await
        .map(|(_, rows)| rows)
        .map_err(failed)
}

pub(crate) fn sql_literal(text: &str) -> String {
    literal(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> EditableColumn {
        EditableColumn {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: default.map(String::from),
            comment: None,
            generated: false,
        }
    }

    fn edit(name: &str, ty: &str, nullable: bool, default: Option<&str>) -> ColumnEdit {
        ColumnEdit {
            name: name.into(),
            data_type: ty.into(),
            nullable,
            default: default.map(String::from),
            comment: None,
        }
    }

    #[test]
    fn only_what_differs_from_the_catalog_changes() {
        let current = [column(
            "a",
            "character varying(80)",
            true,
            Some("'x'::text"),
        )];
        // The same type written otherwise, an empty default box for no default.
        let same = diff(
            &current,
            &[edit("a", "CHARACTER  VARYING(80)", true, Some("'x'::text"))],
            "postgres",
        )
        .unwrap();
        assert!(same.is_empty());
        let changes = diff(&current, &[edit("a", "text", false, Some(" "))], "postgres").unwrap();
        assert_eq!(changes[0].data_type.as_deref(), Some("text"));
        assert_eq!(changes[0].nullable, Some(false));
        assert_eq!(changes[0].default, Some(None));
        assert!(matches!(
            diff(&current, &[edit("gone", "int", true, None)], "postgres"),
            Err(ObjectError::Gone(_))
        ));
    }

    #[test]
    fn a_type_or_default_is_one_piece_of_sql() {
        let current = [column("a", "int", true, None)];
        for (ty, default) in [
            ("int; DROP TABLE t", None),
            ("int", Some("0; DELETE FROM t")),
            ("varchar(8", None),
            ("", None),
        ] {
            assert!(
                matches!(
                    diff(&current, &[edit("a", ty, true, default)], "postgres"),
                    Err(ObjectError::Unsupported(_))
                ),
                "{ty} {default:?}"
            );
        }
        let mut commented = edit("a", "int", true, None);
        commented.comment = Some("note".into());
        assert!(matches!(
            diff(&current, &[commented], "sqlite"),
            Err(ObjectError::Unsupported(_))
        ));
    }

    #[test]
    fn scripts_wrap_several_statements_in_a_transaction() {
        let plan = AlterationPlan {
            before: vec!["PRAGMA foreign_keys = OFF".into()],
            steps: vec![
                Step::Run("CREATE TABLE b (x)".into()),
                Step::ExpectZero {
                    sql: "SELECT 0".into(),
                    problem: "broken".into(),
                },
            ],
            after: vec!["PRAGMA foreign_keys = ON".into()],
            begin: Some("BEGIN"),
            note: Some("rebuilt"),
            rebuild: true,
        };
        assert_eq!(
            plan.script(),
            "-- rebuilt\nPRAGMA foreign_keys = OFF;\nBEGIN;\nCREATE TABLE b (x);\nSELECT 0;\nCOMMIT;\nPRAGMA foreign_keys = ON;\n"
        );
        assert_eq!(plan.statement_count(), 6);
    }
}
