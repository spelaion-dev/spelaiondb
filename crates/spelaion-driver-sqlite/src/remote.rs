//! SQLite on a server (ADR-018): the server's own `sqlite3` program, run in safe mode through
//! a [`RemoteCommand`], one process per connection.
//!
//! Each statement is written followed by `.print <marker>`, with a marker chosen at random,
//! so the end of its output is known. `.mode quote` writes every value as an SQL literal
//! (`1`, `1.5`, `'text'`, `X'00ff'`, `NULL`), which keeps SQLite's storage classes; columns
//! get their types as in the local driver, from the first batch (ADR-006). `.changes on`
//! gives the rows affected, and `.timeout 5000` waits for another writer's lock like the
//! local driver. Only statements from the core's splitter are sent: one that
//! starts with `.` (a command of the `sqlite3` shell) or is not complete is refused, so user
//! text never reaches the shell as a command.

use std::sync::Arc;

use arrow::array::RecordBatch;
use async_trait::async_trait;
use rusqlite::types::Value as SqlValue;
use spelaion_driver_api::{
    CancelHandle, Capabilities, ColumnMeta, Connection, Dialect, DriverError, EventStream,
    ExecEvent, ExecOptions, LikeFilter, NameFilter, ObjectInfo, ReferencingKey, Remote,
    RemoteControl, RemoteProcess, Result, ResultSchema, SchemaInfo, SchemaObject, TableInfo,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

use crate::catalog::{self, Row};
use crate::dialect::SqliteDialect;
use crate::exec::{self, Affinity, Builder, Seen};

/// Oldest `sqlite3` with `-safe`.
const SAFE_MODE_SINCE: &str = "3.37";

/// A word for the remote POSIX shell (`'…'`, with `'` written as `'\''`).
fn shell_word(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The command line that starts `sqlite3` on the server.
pub(crate) fn command_line(program: &str, path: &str, read_only: bool) -> Result<String> {
    // A path starting with `-` would be read as an option.
    if path.is_empty() || path.starts_with('-') {
        return Err(DriverError::Connection(format!(
            "{path:?} is not a usable path for the file on the server"
        )));
    }
    // sqlite3 creates a missing file: a mistyped path must not leave an empty database.
    let path = shell_word(path);
    Ok(format!(
        "[ -f {path} ] || {{ echo 'spelaion: no such file on the server'; exit 1; }}; \
         exec {} -safe -batch -header {}-cmd '.mode quote' -cmd '.changes on' \
         -cmd '.timeout 5000' {path} 2>&1",
        shell_word(program),
        if read_only { "-readonly " } else { "" },
    ))
}

/// Refuses what must not reach the `sqlite3` shell: its own commands, and statements that
/// do not end where they seem to (an open quote or comment would swallow the end marker).
pub(crate) fn check_statement(sql: &str) -> Result<()> {
    if sql.trim_start().starts_with('.') {
        return Err(DriverError::Other(
            "commands of the sqlite3 shell (starting with a dot) are not run on the server".into(),
        ));
    }
    if SqliteDialect
        .split_statements(&format!("{sql}\n;\nselect 1"))
        .len()
        != 2
    {
        return Err(DriverError::Other(
            "the statement is not complete (an open quote or comment)".into(),
        ));
    }
    Ok(())
}

/// One line of output, or a record that spans lines (a text value with line breaks).
#[derive(Debug, PartialEq)]
pub(crate) enum Record {
    /// Values in quote mode: a header (names) or a row.
    Values(Vec<SqlValue>),
    /// `changes: N   total_changes: M`.
    Changes(u64),
    /// Any other line: an error or a message.
    Message(String),
    /// The end marker of the current statement.
    End,
}

/// Lines that are messages, not values: values in quote mode start with a quote, a digit, a
/// sign, `X'` or `NULL`.
fn is_message(line: &str) -> bool {
    const MESSAGES: &[&str] = &[
        "Parse error",
        "Runtime error",
        "Error",
        "line ",
        "sqlite3:",
        "the server refused",
        "Usage",
    ];
    line.is_empty()
        || line.starts_with(' ')
        || MESSAGES.iter().any(|m| line.starts_with(m))
        || !line.starts_with(|c: char| {
            c == '\''
                || c == '-'
                || c == '+'
                || c.is_ascii_digit()
                || c == 'X'
                || c == 'N'
                || c == 'I'
        })
}

/// Splits a record in quote mode into its values.
pub(crate) fn parse_values(record: &str) -> Option<Vec<SqlValue>> {
    let mut values = Vec::new();
    let mut chars = record.chars().peekable();
    loop {
        let value = match chars.peek()? {
            '\'' => {
                chars.next();
                let mut text = String::new();
                loop {
                    match chars.next()? {
                        '\'' if chars.peek() == Some(&'\'') => {
                            chars.next();
                            text.push('\'');
                        }
                        '\'' => break,
                        c => text.push(c),
                    }
                }
                SqlValue::Text(text)
            }
            'X' => {
                chars.next();
                if chars.next()? != '\'' {
                    return None;
                }
                let hex: String = chars.by_ref().take_while(|c| *c != '\'').collect();
                let bytes = (0..hex.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                    .collect::<Option<Vec<u8>>>()?;
                SqlValue::Blob(bytes)
            }
            _ => {
                let mut word = String::new();
                while let Some(c) = chars.peek() {
                    if *c == ',' {
                        break;
                    }
                    word.push(*c);
                    chars.next();
                }
                match word.as_str() {
                    "NULL" => SqlValue::Null,
                    "Inf" => SqlValue::Real(f64::INFINITY),
                    "-Inf" => SqlValue::Real(f64::NEG_INFINITY),
                    _ => match word.parse::<i64>() {
                        Ok(n) => SqlValue::Integer(n),
                        Err(_) => SqlValue::Real(word.parse().ok()?),
                    },
                }
            }
        };
        values.push(value);
        match chars.next() {
            None => return Some(values),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}

/// Reads the output of `sqlite3` record by record.
struct Reader {
    lines: BufReader<std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>>,
    /// Everything read lately, for when the process ends unexpectedly.
    tail: String,
}

impl Reader {
    async fn line(&mut self) -> Result<Option<String>> {
        let mut bytes = Vec::new();
        let read = self
            .lines
            .read_until(b'\n', &mut bytes)
            .await
            .map_err(|e| DriverError::Connection(format!("reading from sqlite3: {e}")))?;
        if read == 0 {
            return Ok(None);
        }
        let line = String::from_utf8_lossy(&bytes)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.tail.push_str(&line);
        self.tail.push('\n');
        if self.tail.len() > 4096 {
            let cut = self.tail.len() - 4096;
            let cut = (cut..self.tail.len())
                .find(|i| self.tail.is_char_boundary(*i))
                .unwrap_or(0);
            self.tail.drain(..cut);
        }
        Ok(Some(line))
    }

    /// The next record, or `None` at the end of the output (the process ended).
    async fn record(&mut self, marker: &str) -> Result<Option<Record>> {
        let Some(mut line) = self.line().await? else {
            return Ok(None);
        };
        if line == marker {
            return Ok(Some(Record::End));
        }
        if let Some(rest) = line.strip_prefix("changes:") {
            let n = rest
                .split_whitespace()
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            return Ok(Some(Record::Changes(n)));
        }
        if is_message(&line) {
            return Ok(Some(Record::Message(line)));
        }
        // A text value may hold line breaks: the record goes on while a quote is open.
        while line.chars().filter(|c| *c == '\'').count() % 2 == 1 {
            let Some(next) = self.line().await? else {
                return Ok(None);
            };
            line.push('\n');
            line.push_str(&next);
        }
        Ok(Some(match parse_values(&line) {
            Some(values) => Record::Values(values),
            None => Record::Message(line),
        }))
    }
}

/// What the process said when it ended without being asked to, in our words when we know it.
fn ended(output: &str) -> DriverError {
    let output = output.trim();
    let message = if output.contains("spelaion: no such file on the server") {
        "the file does not exist on the server".to_string()
    } else if output.contains("unknown option: -safe") {
        format!(
            "the server's sqlite3 is too old: SpelaionDB needs {SAFE_MODE_SINCE} or later, for its safe mode"
        )
    } else if output.contains("not found")
        || output.contains("No such file") && output.contains("sqlite3")
    {
        "sqlite3 is not installed on the server, or not on the SSH user's PATH: install it or set its path in the connection".into()
    } else if output.contains("Program interrupted") {
        "the statement was cancelled, and sqlite3 on the server ends when it is interrupted: \
         this session ended (an open transaction was rolled back); reconnect to go on"
            .into()
    } else if output.contains("in safe mode") {
        format!(
            "{} — SpelaionDB runs sqlite3 in safe mode, which ends it on that; the session ended",
            output.lines().last().unwrap_or(output)
        )
    } else if output.is_empty() {
        "sqlite3 on the server ended".into()
    } else {
        format!("sqlite3 on the server ended: {output}")
    };
    DriverError::Connection(message)
}

/// The text of an error from the shell: `Parse error near line 1: no such table: x`.
fn database_error(lines: &[String]) -> DriverError {
    let first = lines.first().cloned().unwrap_or_default();
    let message = [
        "Parse error near line ",
        "Runtime error near line ",
        "Error: near line ",
    ]
    .iter()
    .find_map(|prefix| first.strip_prefix(prefix))
    .and_then(|rest| rest.split_once(": ").map(|(_, m)| m.to_string()))
    .or_else(|| first.strip_prefix("Error: ").map(str::to_string))
    .or_else(|| first.strip_prefix("Runtime error: ").map(str::to_string))
    .unwrap_or(first);
    if message.contains("interrupted") {
        return DriverError::Cancelled;
    }
    let detail = (lines.len() > 1).then(|| lines[1..].join("\n"));
    DriverError::Database {
        code: None,
        message,
        detail,
        hint: None,
        position: None,
    }
}

enum Command {
    Execute {
        sql: String,
        batch_size: usize,
        events: mpsc::Sender<Result<ExecEvent>>,
    },
    Rows(String, oneshot::Sender<Result<Vec<Row>>>),
}

struct Process {
    stdin: std::pin::Pin<Box<dyn tokio::io::AsyncWrite + Send>>,
    reader: Reader,
    marker: String,
    /// The process ended: nothing more can run.
    dead: Option<DriverError>,
}

impl Process {
    async fn send(&mut self, sql: &str) -> Result<()> {
        let text = format!("{sql}\n;\n.print {}\n", self.marker);
        let written = async {
            self.stdin.write_all(text.as_bytes()).await?;
            self.stdin.flush().await
        };
        written
            .await
            .map_err(|e| DriverError::Connection(format!("writing to sqlite3: {e}")))
    }

    fn closed(&self) -> DriverError {
        match &self.dead {
            Some(DriverError::Connection(m)) => DriverError::Connection(m.clone()),
            _ => DriverError::Connection("the sqlite3 session on the server is closed".into()),
        }
    }

    /// The process ended in the middle of a statement.
    fn died(&mut self) -> DriverError {
        let error = ended(&self.reader.tail);
        if let DriverError::Connection(m) = &error {
            self.dead = Some(DriverError::Connection(m.clone()));
        }
        error
    }

    /// Runs a statement and returns all its rows (catalog queries, the version).
    async fn rows(&mut self, sql: &str) -> Result<Vec<Row>> {
        if self.dead.is_some() {
            return Err(self.closed());
        }
        self.send(sql).await?;
        let mut header = false;
        let mut rows = Vec::new();
        let mut errors = Vec::new();
        loop {
            match self.reader.record(&self.marker.clone()).await? {
                None => return Err(self.died()),
                Some(Record::End) => break,
                Some(Record::Values(values)) if header => rows.push(values),
                Some(Record::Values(_)) => header = true,
                Some(Record::Message(m)) => errors.push(m),
                Some(Record::Changes(_)) => {}
            }
        }
        if errors.is_empty() {
            Ok(rows)
        } else {
            Err(database_error(&errors))
        }
    }

    async fn execute(
        &mut self,
        sql: &str,
        batch_size: usize,
        events: &mpsc::Sender<Result<ExecEvent>>,
    ) {
        let send = |event| async move { events.send(event).await.is_ok() };
        if let Err(e) = check_statement(sql) {
            send(Err(e)).await;
            return;
        }
        if self.dead.is_some() {
            send(Err(self.closed())).await;
            return;
        }
        if let Err(e) = self.send(sql).await {
            send(Err(e)).await;
            return;
        }
        let marker = self.marker.clone();
        let mut names: Option<Vec<String>> = None;
        let mut first: Vec<Row> = Vec::new();
        let mut current: Option<(Arc<ResultSchema>, Vec<Builder>, usize)> = None;
        let mut total: u64 = 0;
        let mut changes = None;
        let mut errors = Vec::new();
        // Nobody listening any more: the rest is read and dropped, to stay in step.
        let mut listening = true;
        loop {
            let record = match self.reader.record(&marker).await {
                Ok(Some(record)) => record,
                Ok(None) => {
                    let error = self.died();
                    // A cancel: sqlite3 interrupted the statement, then ended (batch mode).
                    let cancelled = self.reader.tail.contains("Program interrupted");
                    send(Err(if cancelled {
                        DriverError::Cancelled
                    } else {
                        error
                    }))
                    .await;
                    return;
                }
                Err(e) => {
                    send(Err(e)).await;
                    return;
                }
            };
            match record {
                Record::End => break,
                Record::Changes(n) => changes = Some(n),
                Record::Message(m) => errors.push(m),
                Record::Values(values) if names.is_none() => {
                    names = Some(
                        values
                            .into_iter()
                            .map(|v| match v {
                                SqlValue::Text(s) => s,
                                other => format!("{other:?}"),
                            })
                            .collect(),
                    );
                }
                Record::Values(values) => {
                    total += 1;
                    if !listening || !errors.is_empty() {
                        continue;
                    }
                    match current.as_mut() {
                        // The first batch decides the column types.
                        None => {
                            first.push(values);
                            if first.len() == batch_size {
                                match start(names.as_deref().unwrap_or_default(), &mut first) {
                                    Ok(started) => {
                                        listening =
                                            send(Ok(ExecEvent::Columns(started.0.clone()))).await;
                                        current = Some(started);
                                        if listening
                                            && let Some((schema, builders, pending)) =
                                                current.as_mut()
                                        {
                                            match flush(schema, builders, pending) {
                                                Ok(batch) => {
                                                    listening =
                                                        send(Ok(ExecEvent::Batch(batch))).await
                                                }
                                                Err(e) => errors.push(e.to_string()),
                                            }
                                        }
                                    }
                                    Err(e) => errors.push(e.to_string()),
                                }
                            }
                        }
                        Some((schema, builders, pending)) => {
                            if let Err(e) = append(schema, builders, &values, total) {
                                errors.push(e.to_string());
                                continue;
                            }
                            *pending += 1;
                            if *pending == batch_size {
                                match flush(schema, builders, pending) {
                                    Ok(batch) => {
                                        listening = send(Ok(ExecEvent::Batch(batch))).await
                                    }
                                    Err(e) => errors.push(e.to_string()),
                                }
                            }
                        }
                    }
                }
            }
        }
        if !listening {
            return;
        }
        if !errors.is_empty() {
            send(Err(database_error(&errors))).await;
            return;
        }
        if current.is_none() && !first.is_empty() {
            match start(names.as_deref().unwrap_or_default(), &mut first) {
                Ok(started) => {
                    if !send(Ok(ExecEvent::Columns(started.0.clone()))).await {
                        return;
                    }
                    current = Some(started);
                }
                Err(e) => {
                    send(Err(e)).await;
                    return;
                }
            }
        }
        if let Some((schema, builders, pending)) = current.as_mut()
            && *pending > 0
        {
            match flush(schema, builders, pending) {
                Ok(batch) => {
                    if !send(Ok(ExecEvent::Batch(batch))).await {
                        return;
                    }
                }
                Err(e) => {
                    send(Err(e)).await;
                    return;
                }
            }
        }
        // As in the local driver: rows for queries, `changes()` for INSERT/UPDATE/DELETE.
        // A query with no rows prints no header: its columns are not known.
        let rows_affected = if names.is_some() || exec::is_query(sql) {
            Some(total)
        } else if exec::is_dml(sql) {
            changes
        } else {
            None
        };
        send(Ok(ExecEvent::Done { rows_affected })).await;
    }
}

/// The schema from the header and the first batch, with the first batch already added.
fn start(
    names: &[String],
    first: &mut Vec<Row>,
) -> Result<(Arc<ResultSchema>, Vec<Builder>, usize)> {
    let mut seen = vec![Seen::default(); names.len()];
    for row in first.iter() {
        for (s, v) in seen.iter_mut().zip(row) {
            s.add(v);
        }
    }
    let columns: Vec<ColumnMeta> = names
        .iter()
        .zip(&seen)
        .map(|(name, seen)| ColumnMeta {
            name: name.clone(),
            // No declared types through the shell: as a column without one.
            logical: exec::decide(Affinity::Blob, *seen),
            native_type: seen.storage_class().to_string(),
            native_id: None,
        })
        .collect();
    let schema = Arc::new(ResultSchema::new(columns));
    let mut builders: Vec<Builder> = schema
        .columns
        .iter()
        .map(|c| Builder::new(c.logical))
        .collect();
    let rows = std::mem::take(first);
    let pending = rows.len();
    for (n, row) in rows.iter().enumerate() {
        append(&schema, &mut builders, row, n as u64 + 1)?;
    }
    Ok((schema, builders, pending))
}

fn append(schema: &ResultSchema, builders: &mut [Builder], row: &Row, number: u64) -> Result<()> {
    for (i, builder) in builders.iter_mut().enumerate() {
        let value = row.get(i).unwrap_or(&SqlValue::Null);
        builder.append(exec::as_ref(value)).map_err(|found| DriverError::Decode {
            type_name: schema.columns[i].native_type.clone(),
            message: format!(
                "column \"{}\" mixes types: value {found} at row {number}, in a column read as {}. \
                 SQLite allows it; use CAST(\"{}\" AS TEXT) to see it in full",
                schema.columns[i].name,
                schema.columns[i].logical.as_str(),
                schema.columns[i].name
            ),
        })?;
    }
    Ok(())
}

fn flush(
    schema: &ResultSchema,
    builders: &mut [Builder],
    pending: &mut usize,
) -> Result<RecordBatch> {
    *pending = 0;
    RecordBatch::try_new(
        schema.arrow.clone(),
        builders.iter_mut().map(Builder::finish).collect(),
    )
    .map_err(|e| DriverError::Other(e.to_string()))
}

/// A marker no output will contain by chance.
fn new_marker() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    format!("spelaion-end-{:016x}", hasher.finish())
}

/// Starts `sqlite3` on the server and checks it answers.
pub async fn connect(remote: &Remote, path: &str, read_only: bool) -> Result<Box<dyn Connection>> {
    let program = remote.program.as_deref().unwrap_or("sqlite3");
    let RemoteProcess {
        stdin,
        stdout,
        control,
    } = remote
        .command
        .start(&command_line(program, path, read_only)?)
        .await?;
    let mut process = Process {
        stdin,
        reader: Reader {
            lines: BufReader::new(stdout),
            tail: String::new(),
        },
        marker: new_marker(),
        dead: None,
    };
    // The first answer: the version (and a sign the file opened).
    let version = process.rows("select sqlite_version()").await?;
    let version = match version.first().and_then(|row| row.first()) {
        Some(SqlValue::Text(v)) => v.clone(),
        _ => {
            return Err(DriverError::Connection(
                "sqlite3 on the server did not answer".into(),
            ));
        }
    };
    let (commands, inbox) = mpsc::channel(8);
    tokio::spawn(actor(process, inbox));
    Ok(Box::new(RemoteConnection {
        commands,
        version,
        cancel: Arc::new(RemoteCancel(control)),
        capabilities: Capabilities {
            transactions: true,
            cancel: true,
            explain: true,
            editable_results: false,
            schemas: true,
        },
    }))
}

async fn actor(mut process: Process, mut inbox: mpsc::Receiver<Command>) {
    while let Some(command) = inbox.recv().await {
        match command {
            Command::Execute {
                sql,
                batch_size,
                events,
            } => process.execute(&sql, batch_size, &events).await,
            Command::Rows(sql, reply) => {
                let _ = reply.send(process.rows(&sql).await);
            }
        }
    }
    // Closing its input ends sqlite3.
    let _ = process.stdin.shutdown().await;
}

struct RemoteConnection {
    commands: mpsc::Sender<Command>,
    version: String,
    cancel: Arc<RemoteCancel>,
    capabilities: Capabilities,
}

fn closed() -> DriverError {
    DriverError::Connection("the sqlite3 session on the server is closed".into())
}

impl RemoteConnection {
    async fn rows(&self, sql: String) -> Result<Vec<Row>> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Rows(sql, reply))
            .await
            .map_err(|_| closed())?;
        answer.await.map_err(|_| closed())?
    }
}

#[async_trait]
impl Connection for RemoteConnection {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    async fn execute(&self, sql: &str, opts: &ExecOptions) -> Result<EventStream> {
        let (events, mut rx) = mpsc::channel(4);
        self.commands
            .send(Command::Execute {
                sql: sql.to_string(),
                batch_size: opts.batch_size.max(1),
                events,
            })
            .await
            .map_err(|_| closed())?;
        Ok(Box::pin(futures_util::stream::poll_fn(move |cx| {
            rx.poll_recv(cx)
        })))
    }

    fn cancel_handle(&self) -> Arc<dyn CancelHandle> {
        self.cancel.clone()
    }

    async fn server_version(&self) -> Result<String> {
        Ok(self.version.clone())
    }

    async fn list_schemas(&self) -> Result<Vec<SchemaInfo>> {
        Ok(catalog::schemas(
            &self
                .rows(catalog::schemas_sql(&LikeFilter::default()))
                .await?,
        ))
    }

    async fn list_objects(&self, schema: &str) -> Result<Vec<ObjectInfo>> {
        Ok(catalog::objects(
            schema,
            &self
                .rows(catalog::objects_sql(schema, &LikeFilter::default()))
                .await?,
        ))
    }

    async fn describe_table(&self, schema: &str, name: &str) -> Result<TableInfo> {
        let object = self
            .list_objects(schema)
            .await?
            .into_iter()
            .find(|o| o.name == name)
            .ok_or_else(|| DriverError::Other(format!("{schema}.{name} does not exist")))?;
        let (mut table, to_primary_keys) = catalog::table(
            object,
            &self.rows(catalog::columns_sql(schema, name)).await?,
            &self.rows(catalog::indexes_sql(schema, name)).await?,
            &self.rows(catalog::foreign_keys_sql(schema, name)).await?,
            &self.rows(catalog::unique_columns_sql(schema, name)).await?,
        );
        for i in to_primary_keys {
            let referenced = table.foreign_keys[i].referenced_table.clone();
            table.foreign_keys[i].referenced_columns = catalog::names(
                &self
                    .rows(catalog::primary_key_sql(schema, &referenced))
                    .await?,
            );
        }
        Ok(table)
    }

    async fn list_referencing_keys(&self, schema: &str, name: &str) -> Result<Vec<ReferencingKey>> {
        let rows = self
            .rows(catalog::referencing_keys_sql(schema, name))
            .await?;
        let primary_key = catalog::names(&self.rows(catalog::primary_key_sql(schema, name)).await?);
        Ok(catalog::referencing_keys(schema, &rows, &primary_key))
    }

    async fn list_schema_objects(&self, schema: &str) -> Result<Vec<SchemaObject>> {
        Ok(catalog::schema_objects(
            schema,
            &self
                .rows(catalog::schema_objects_sql(schema, &LikeFilter::default()))
                .await?,
        ))
    }

    // Narrowed in the catalog query, then matched exactly (ADR-025).
    async fn list_schemas_matching(&self, filter: &NameFilter) -> Result<Vec<SchemaInfo>> {
        let rows = self
            .rows(catalog::schemas_sql(&filter.server_side()))
            .await?;
        let mut schemas = catalog::schemas(&rows);
        schemas.retain(|s| filter.matches(&s.name));
        Ok(schemas)
    }

    async fn list_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<ObjectInfo>> {
        let rows = self
            .rows(catalog::objects_sql(schema, &filter.server_side()))
            .await?;
        let mut objects = catalog::objects(schema, &rows);
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn list_schema_objects_matching(
        &self,
        schema: &str,
        filter: &NameFilter,
    ) -> Result<Vec<SchemaObject>> {
        let rows = self
            .rows(catalog::schema_objects_sql(schema, &filter.server_side()))
            .await?;
        let mut objects = catalog::schema_objects(schema, &rows);
        objects.retain(|o| filter.matches(&o.name));
        Ok(objects)
    }

    async fn object_definition(&self, object: &SchemaObject) -> Result<String> {
        let rows = self.rows(catalog::definition_sql(object)?).await?;
        catalog::definition(object, &rows)
    }

    async fn relation_definition(&self, schema: &str, name: &str) -> Result<String> {
        let rows = self
            .rows(catalog::relation_definition_sql(schema, name))
            .await?;
        catalog::relation_definition(schema, name, &rows)
    }
}

/// Cancelling: SIGINT to `sqlite3`, which interrupts the statement and then ends (it only
/// carries on in its interactive mode, which echoes its input). The session ends with it.
struct RemoteCancel(Arc<dyn RemoteControl>);

#[async_trait]
impl CancelHandle for RemoteCancel {
    async fn cancel(&self) -> Result<()> {
        self.0.interrupt().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_in_quote_mode_keep_their_storage_class() {
        assert_eq!(
            parse_values("1,'it''s, ok',X'00ff',NULL,-0.5,1.000000000000000052e+300,'a\nb'"),
            Some(vec![
                SqlValue::Integer(1),
                SqlValue::Text("it's, ok".into()),
                SqlValue::Blob(vec![0x00, 0xff]),
                SqlValue::Null,
                SqlValue::Real(-0.5),
                SqlValue::Real(1e300),
                SqlValue::Text("a\nb".into()),
            ])
        );
        assert_eq!(
            parse_values("'id','name'"),
            Some(vec![
                SqlValue::Text("id".into()),
                SqlValue::Text("name".into())
            ])
        );
        assert_eq!(parse_values("X''"), Some(vec![SqlValue::Blob(vec![])]));
        assert_eq!(parse_values("'unclosed"), None);
    }

    #[test]
    fn messages_are_told_from_values() {
        for line in [
            "Parse error near line 1: no such table: x",
            "   ^--- error here",
            "line 3: cannot run .shell in safe mode",
            "",
        ] {
            assert!(is_message(line), "{line}");
        }
        for line in ["'a',1", "1,2", "-3", "X'00'", "NULL"] {
            assert!(!is_message(line), "{line}");
        }
    }

    #[test]
    fn the_shell_never_receives_commands_or_incomplete_statements() {
        assert!(check_statement(".shell rm -rf /").is_err());
        assert!(check_statement("  .system ls").is_err());
        assert!(check_statement("select 'open").is_err());
        assert!(check_statement("select 1 /* open").is_err());
        assert!(check_statement("select 1 -- a comment; still a comment").is_ok());
        assert!(
            check_statement("create trigger t after insert on a begin\nselect 1;\n.shell x;\nend")
                .is_ok()
        );
        assert!(check_statement("select '.shell'").is_ok());
    }

    #[test]
    fn the_command_line_quotes_what_it_is_given() {
        assert_eq!(
            command_line("sqlite3", "/data/it's.db", true).unwrap(),
            r"[ -f '/data/it'\''s.db' ] || { echo 'spelaion: no such file on the server'; exit 1; }; exec 'sqlite3' -safe -batch -header -readonly -cmd '.mode quote' -cmd '.changes on' -cmd '.timeout 5000' '/data/it'\''s.db' 2>&1"
        );
        assert!(command_line("sqlite3", "-x.db", false).is_err());
        assert!(command_line("sqlite3", "", false).is_err());
    }

    #[test]
    fn errors_read_like_sqlite() {
        let error = database_error(&[
            "Parse error near line 1: no such table: missing".into(),
            "  select * from missing".into(),
        ]);
        assert!(
            matches!(error, DriverError::Database { ref message, ref detail, .. }
            if message == "no such table: missing" && detail.as_deref() == Some("  select * from missing"))
        );
        assert!(matches!(
            database_error(&["Runtime error: interrupted".into()]),
            DriverError::Cancelled
        ));
        assert!(
            matches!(ended("sqlite3: Error: unknown option: -safe\nUse -help for a list of options."),
            DriverError::Connection(m) if m.contains("3.37"))
        );
    }
}
