//! Phase 0, experiments 1 and 3.
//!
//! - Experiment 1: a canvas grid with 1 M synthetic rows must scroll smoothly on WebView2
//!   and on WebKitGTK.
//! - Experiment 3: 1 M PostgreSQL rows -> Arrow batches in the backend -> the UI requests
//!   windows of rows as binary Arrow IPC. Memory and time to the first row are measured.
//!
//! The result lives in the backend (ADR-010); the frontend only keeps what is visible.
//! Throwaway code: it validates risks, it is not the product design.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use arrow::array::{
    ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray, StringBuilder,
    TimestampMillisecondArray,
};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef, TimeUnit};
use arrow::ipc::writer::StreamWriter;
use futures_util::TryStreamExt;
use serde::Serialize;
use tauri::ipc::{Channel, Response};
use tauri::{AppHandle, State};
use tokio_postgres::{NoTls, SimpleQueryMessage};

const SYNTHETIC_BATCH: usize = 65_536;
const PG_BATCH: usize = 10_000;

/// A result materialized in the backend as Arrow batches.
struct ResultSet {
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    /// Index of the first row of each batch.
    starts: Vec<usize>,
    rows: usize,
}

impl ResultSet {
    fn new(schema: SchemaRef) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            starts: Vec::new(),
            rows: 0,
        }
    }

    fn push(&mut self, batch: RecordBatch) {
        self.starts.push(self.rows);
        self.rows += batch.num_rows();
        self.batches.push(batch);
    }

    /// Serializes rows [start, start + len) as an Arrow IPC stream.
    fn window_ipc(&self, start: usize, len: usize) -> anyhow::Result<Vec<u8>> {
        let end = (start + len).min(self.rows);
        let mut out = Vec::new();
        let mut writer = StreamWriter::try_new(&mut out, &self.schema)?;
        if start < end {
            let first = self.starts.partition_point(|&s| s <= start) - 1;
            for (batch, &batch_start) in self.batches[first..].iter().zip(&self.starts[first..]) {
                if batch_start >= end {
                    break;
                }
                let from = start.saturating_sub(batch_start);
                let to = (end - batch_start).min(batch.num_rows());
                writer.write(&batch.slice(from, to - from))?;
            }
        }
        writer.finish()?;
        drop(writer);
        Ok(out)
    }
}

#[derive(Default)]
struct Store {
    next_id: AtomicU32,
    results: Mutex<HashMap<u32, Arc<RwLock<ResultSet>>>>,
}

impl Store {
    fn insert(&self, rs: ResultSet) -> (u32, Arc<RwLock<ResultSet>>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rs = Arc::new(RwLock::new(rs));
        let mut results = self.results.lock().expect("store lock poisoned");
        // A single live result at a time: that way the measured memory is the last one's.
        results.clear();
        results.insert(id, rs.clone());
        (id, rs)
    }

    fn get(&self, id: u32) -> Result<Arc<RwLock<ResultSet>>, String> {
        self.results
            .lock()
            .expect("store lock poisoned")
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("result {id} does not exist"))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenedResult {
    id: u32,
    columns: Vec<String>,
    rows: usize,
    elapsed_ms: f64,
}

#[derive(Clone, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
enum LoadEvent {
    Started {
        id: u32,
        columns: Vec<String>,
    },
    Progress {
        rows: usize,
        elapsed_ms: f64,
        first_batch_ms: f64,
    },
    Done {
        rows: usize,
        elapsed_ms: f64,
        first_batch_ms: f64,
    },
    Failed {
        message: String,
    },
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}

#[tauri::command]
async fn open_synthetic(rows: usize, store: State<'_, Arc<Store>>) -> Result<OpenedResult, String> {
    let started = Instant::now();
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("customer", DataType::Utf8, false),
        Field::new("amount", DataType::Float64, false),
        Field::new(
            "created",
            DataType::Timestamp(TimeUnit::Millisecond, Some("UTC".into())),
            false,
        ),
        Field::new("active", DataType::Boolean, false),
        Field::new("notes", DataType::Utf8, true),
    ]));
    let columns = schema.fields().iter().map(|f| f.name().clone()).collect();
    let mut rs = ResultSet::new(schema.clone());

    const LOREM: &str = "lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua";
    let base_ms = 1_767_225_600_000i64; // 2026-01-01T00:00:00Z
    let mut from = 0usize;
    while from < rows {
        let to = (from + SYNTHETIC_BATCH).min(rows);
        let ids: Vec<i64> = (from as i64..to as i64).collect();
        let cols: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(ids.clone())),
            Arc::new(StringArray::from_iter_values(
                ids.iter().map(|i| format!("customer-{i:07}")),
            )),
            Arc::new(Float64Array::from_iter_values(
                ids.iter().map(|i| (i * 37 % 10_000_000) as f64 / 100.0),
            )),
            Arc::new(
                TimestampMillisecondArray::from_iter_values(ids.iter().map(|i| base_ms + i * 1000))
                    .with_timezone("UTC"),
            ),
            Arc::new(BooleanArray::from_iter(
                ids.iter().map(|i| Some(i % 3 == 0)),
            )),
            Arc::new(StringArray::from_iter(ids.iter().map(|i| {
                (i % 10 != 0).then(|| &LOREM[..40 + (*i as usize * 7) % (LOREM.len() - 40)])
            }))),
        ];
        rs.push(RecordBatch::try_new(schema.clone(), cols).map_err(|e| e.to_string())?);
        from = to;
    }

    let rows = rs.rows;
    let (id, _) = store.insert(rs);
    Ok(OpenedResult {
        id,
        columns,
        rows,
        elapsed_ms: ms(started),
    })
}

/// Launches the query and loads the result in the background. Every column arrives as
/// text (simple protocol): it is the heaviest case in memory, conservative for measuring.
#[tauri::command]
async fn open_postgres(
    conn: String,
    sql: String,
    on_event: Channel<LoadEvent>,
    store: State<'_, Arc<Store>>,
) -> Result<(), String> {
    let started = Instant::now();
    let (client, connection) = tokio_postgres::connect(&conn, NoTls)
        .await
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn(async move {
        let _ = connection.await;
    });
    let stream = client
        .simple_query_raw(&sql)
        .await
        .map_err(|e| e.to_string())?;
    let store = store.inner().clone();

    tauri::async_runtime::spawn(async move {
        let result = load_pg(stream, started, &on_event, store).await;
        if let Err(e) = result {
            let _ = on_event.send(LoadEvent::Failed {
                message: e.to_string(),
            });
        }
        drop(client);
    });
    Ok(())
}

async fn load_pg(
    stream: tokio_postgres::SimpleQueryStream,
    started: Instant,
    on_event: &Channel<LoadEvent>,
    store: Arc<Store>,
) -> anyhow::Result<()> {
    let mut stream = std::pin::pin!(stream);
    let mut target: Option<(Arc<RwLock<ResultSet>>, SchemaRef)> = None;
    let mut builders: Vec<StringBuilder> = Vec::new();
    let mut pending = 0usize;
    let mut first_batch_ms = 0.0;

    let flush = |builders: &mut Vec<StringBuilder>,
                 target: &(Arc<RwLock<ResultSet>>, SchemaRef)|
     -> anyhow::Result<usize> {
        let cols: Vec<ArrayRef> = builders
            .iter_mut()
            .map(|b| Arc::new(b.finish()) as ArrayRef)
            .collect();
        let batch = RecordBatch::try_new(target.1.clone(), cols)?;
        let mut rs = target.0.write().expect("result lock poisoned");
        rs.push(batch);
        Ok(rs.rows)
    };

    while let Some(msg) = stream.try_next().await? {
        match msg {
            SimpleQueryMessage::RowDescription(columns) => {
                let names: Vec<String> = columns.iter().map(|c| c.name().to_string()).collect();
                let schema = Arc::new(Schema::new(
                    names
                        .iter()
                        .map(|n| Field::new(n, DataType::Utf8, true))
                        .collect::<Vec<_>>(),
                ));
                builders = names.iter().map(|_| StringBuilder::new()).collect();
                let (id, rs) = store.insert(ResultSet::new(schema.clone()));
                on_event.send(LoadEvent::Started { id, columns: names })?;
                target = Some((rs, schema));
            }
            SimpleQueryMessage::Row(row) => {
                for (i, b) in builders.iter_mut().enumerate() {
                    b.append_option(row.get(i));
                }
                pending += 1;
                if pending == PG_BATCH {
                    let t = target.as_ref().expect("RowDescription before rows");
                    let rows = flush(&mut builders, t)?;
                    if first_batch_ms == 0.0 {
                        first_batch_ms = ms(started);
                    }
                    pending = 0;
                    on_event.send(LoadEvent::Progress {
                        rows,
                        elapsed_ms: ms(started),
                        first_batch_ms,
                    })?;
                }
            }
            SimpleQueryMessage::CommandComplete(_) => {}
            _ => {}
        }
    }

    let t = target
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("the query returned no rows"))?;
    let rows = if pending > 0 {
        flush(&mut builders, t)?
    } else {
        t.0.read().expect("lock poisoned").rows
    };
    if first_batch_ms == 0.0 {
        first_batch_ms = ms(started);
    }
    on_event.send(LoadEvent::Done {
        rows,
        elapsed_ms: ms(started),
        first_batch_ms,
    })?;
    Ok(())
}

#[tauri::command]
fn fetch_window(
    id: u32,
    start: usize,
    len: usize,
    store: State<'_, Arc<Store>>,
) -> Result<Response, String> {
    let rs = store.get(id)?;
    let rs = rs.read().map_err(|_| "result lock poisoned".to_string())?;
    rs.window_ipc(start, len)
        .map(Response::new)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStats {
    /// Resident memory of the process and all its children (including the WebView processes).
    tree_mb: f64,
    own_mb: f64,
    processes: usize,
}

#[tauri::command]
fn memory_stats() -> Result<MemoryStats, String> {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let mut sys = System::new().map_err(|e| e.to_string())?;
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let me = Pid::from_u32(std::process::id());

    let mut tree = vec![me];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(
            sys.processes()
                .iter()
                .filter(|(_, p)| p.parent() == Some(parent))
                .map(|(pid, _)| *pid),
        );
        i += 1;
    }
    let mb = |pid: &Pid| sys.process(*pid).map(|p| p.memory()).unwrap_or(0) as f64 / 1_048_576.0;
    Ok(MemoryStats {
        tree_mb: tree.iter().map(mb).sum(),
        own_mb: mb(&me),
        processes: tree.len(),
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SystemInfo {
    os: &'static str,
    arch: &'static str,
    webview: String,
    autorun: bool,
    rows: usize,
    pg: Option<String>,
    pg_sql: String,
}

#[tauri::command]
fn system_info() -> SystemInfo {
    SystemInfo {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        webview: tauri::webview_version().unwrap_or_else(|e| format!("unknown ({e})")),
        autorun: std::env::var("SPIKE_AUTORUN").is_ok_and(|v| v == "1"),
        rows: std::env::var("SPIKE_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000),
        pg: std::env::var("SPIKE_PG").ok(),
        pg_sql: std::env::var("SPIKE_PG_SQL").unwrap_or_else(|_| "select * from big".into()),
    }
}

/// Saves the report in SPIKE_RESULTS_DIR (defaults to ./spike-results).
#[tauri::command]
fn save_report(report: serde_json::Value) -> Result<String, String> {
    let dir = std::env::var("SPIKE_RESULTS_DIR").unwrap_or_else(|_| "spike-results".into());
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = std::path::Path::new(&dir).join(format!("{}-{stamp}.json", std::env::consts::OS));
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    Ok(path.display().to_string())
}

#[tauri::command]
fn exit_app(app: AppHandle) {
    app.exit(0);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Arc::new(Store::default()))
        .invoke_handler(tauri::generate_handler![
            open_synthetic,
            open_postgres,
            fetch_window,
            memory_stats,
            system_info,
            save_report,
            exit_app
        ])
        .run(tauri::generate_context!())
        .expect("error while starting the app");
}
