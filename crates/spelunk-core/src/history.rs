//! Desktop query history. A single actor owns local SQLite; synchronous disk access
//! never runs on the async executor. The recorder wraps the normal session event stream.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use spelunk_store::{HistoryEntry, NewHistoryEntry, Store, StoreError};
use tokio::sync::{mpsc, oneshot};

use crate::{CoreEvent, ResultId, Session};

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("{0}")]
    Store(#[from] StoreError),
    #[error("query history is unavailable")]
    Closed,
    #[error("could not start query history: {0}")]
    Start(#[source] std::io::Error),
    #[error("unsupported language; expected system, en or es")]
    UnsupportedLanguage,
}

type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

enum Request {
    GetLanguage {
        reply: Reply<String>,
    },
    SetLanguage {
        language: String,
        reply: Reply<()>,
    },
    List {
        workspace: String,
        search: String,
        limit: usize,
        offset: usize,
        reply: Reply<Vec<HistoryEntry>>,
    },
    Enabled {
        workspace: String,
        connection: String,
        reply: Reply<bool>,
    },
    SetEnabled {
        workspace: String,
        connection: String,
        enabled: bool,
        reply: Reply<()>,
    },
    Record {
        entry: NewHistoryEntry,
        reply: Reply<()>,
    },
}

/// Personal history shared by editor sessions. Dropping all handles shuts down its actor.
#[derive(Clone)]
pub struct QueryHistory {
    requests: mpsc::Sender<Request>,
}

impl QueryHistory {
    /// Opens and migrates the local store synchronously, for application startup.
    pub fn open(path: PathBuf) -> Result<Self, HistoryError> {
        let store = Store::open(&path)?;
        let (requests, mut receiver) = mpsc::channel(32);
        std::thread::Builder::new()
            .name("spelunk-history".into())
            .spawn(move || {
                while let Some(request) = receiver.blocking_recv() {
                    match request {
                        Request::GetLanguage { reply } => {
                            let _ = reply.send(store.get_language());
                        }
                        Request::SetLanguage { language, reply } => {
                            let _ = reply.send(store.set_language(&language));
                        }
                        Request::List {
                            workspace,
                            search,
                            limit,
                            offset,
                            reply,
                        } => {
                            let _ = reply.send(store.history(&workspace, &search, limit, offset));
                        }
                        Request::Enabled {
                            workspace,
                            connection,
                            reply,
                        } => {
                            let _ = reply.send(store.history_enabled(&workspace, &connection));
                        }
                        Request::SetEnabled {
                            workspace,
                            connection,
                            enabled,
                            reply,
                        } => {
                            let _ = reply.send(store.set_history_enabled(
                                &workspace,
                                &connection,
                                enabled,
                            ));
                        }
                        Request::Record { entry, reply } => {
                            let _ = reply.send(store.record_history(&entry));
                        }
                    }
                }
            })
            .map_err(HistoryError::Start)?;
        Ok(Self { requests })
    }

    async fn request<T>(&self, build: impl FnOnce(Reply<T>) -> Request) -> Result<T, HistoryError> {
        let (reply, response) = oneshot::channel();
        self.requests
            .send(build(reply))
            .await
            .map_err(|_| HistoryError::Closed)?;
        response
            .await
            .map_err(|_| HistoryError::Closed)?
            .map_err(HistoryError::Store)
    }

    /// Personal interface language for this user directory, defaulting to `system`.
    pub async fn language(&self) -> Result<String, HistoryError> {
        self.request(|reply| Request::GetLanguage { reply }).await
    }

    /// Validates external input before it reaches the store actor.
    pub async fn set_language(&self, language: String) -> Result<(), HistoryError> {
        if !matches!(language.as_str(), "system" | "en" | "es") {
            return Err(HistoryError::UnsupportedLanguage);
        }
        self.request(|reply| Request::SetLanguage { language, reply })
            .await
    }

    pub async fn list(
        &self,
        workspace: String,
        search: String,
        limit: usize,
        offset: usize,
    ) -> Result<Vec<HistoryEntry>, HistoryError> {
        self.request(|reply| Request::List {
            workspace,
            search,
            limit,
            offset,
            reply,
        })
        .await
    }

    pub async fn enabled(
        &self,
        workspace: String,
        connection: String,
    ) -> Result<bool, HistoryError> {
        self.request(|reply| Request::Enabled {
            workspace,
            connection,
            reply,
        })
        .await
    }

    pub async fn set_enabled(
        &self,
        workspace: String,
        connection: String,
        enabled: bool,
    ) -> Result<(), HistoryError> {
        self.request(|reply| Request::SetEnabled {
            workspace,
            connection,
            enabled,
            reply,
        })
        .await
    }

    async fn record(&self, entry: NewHistoryEntry) -> Result<(), HistoryError> {
        self.request(|reply| Request::Record { entry, reply }).await
    }

    /// Records only statements that actually execute. The caller must drain this stream
    /// even after its UI disappears, so completion and history do not depend on that UI.
    /// A script started with history disabled never saves SQL, even if enabled mid-run.
    pub fn run_script(
        &self,
        session: &Session,
        sql: &str,
        workspace: String,
        connection: String,
    ) -> BoxStream<'static, CoreEvent> {
        let mut events = session.run_script(sql);
        let sql = sql.to_string();
        let history = self.clone();
        Box::pin(async_stream::stream! {
            let enabled = match history.enabled(workspace.clone(), connection.clone()).await {
                Ok(enabled) => enabled,
                Err(error) => {
                    yield warning(error);
                    false
                }
            };
            let mut active: Option<PendingEntry> = None;
            while let Some(event) = events.next().await {
                match &event {
                    CoreEvent::StatementStarted { span, .. } => {
                        if enabled {
                            active = Some(PendingEntry {
                                entry: NewHistoryEntry {
                                    at_ms: millis(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()),
                                    workspace: workspace.clone(),
                                    connection: connection.clone(),
                                    statement: span.text(&sql).to_string(),
                                    duration_ms: 0,
                                    rows: 0,
                                    error: None,
                                    cancelled: false,
                                },
                                started: Instant::now(),
                                results: HashMap::new(),
                            });
                        }
                    }
                    CoreEvent::Rows { result, total_rows, .. } => {
                        if let Some(active) = &mut active {
                            active.results.insert(*result, *total_rows);
                        }
                    }
                    CoreEvent::StatementFinished { elapsed, .. } => {
                        if let Some(active) = active.take()
                            && let Err(error) = history.record(active.finish(*elapsed, None, false)).await
                        {
                            yield warning(error);
                        }
                    }
                    CoreEvent::StatementFailed { error, .. } => {
                        if let Some(active) = active.take() {
                            let elapsed = active.started.elapsed();
                            let message = if error.cancelled {
                                "Query cancelled".to_string()
                            } else {
                                error.message.clone()
                            };
                            if let Err(error) = history.record(active.finish(elapsed, Some(message), error.cancelled)).await {
                                yield warning(error);
                            }
                        }
                    }
                    CoreEvent::ResultStarted { .. }
                    | CoreEvent::Notice { .. }
                    | CoreEvent::ScriptFinished { .. }
                    | CoreEvent::HistoryWarning { .. } => {}
                }
                yield event;
            }
        })
    }
}

fn warning(error: HistoryError) -> CoreEvent {
    CoreEvent::HistoryWarning {
        message: error.to_string(),
    }
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

struct PendingEntry {
    entry: NewHistoryEntry,
    started: Instant,
    results: HashMap<ResultId, usize>,
}

impl PendingEntry {
    fn finish(
        mut self,
        elapsed: Duration,
        error: Option<String>,
        cancelled: bool,
    ) -> NewHistoryEntry {
        self.entry.duration_ms = millis(elapsed);
        self.entry.rows = self.results.values().fold(0i64, |total, &rows| {
            total.saturating_add(i64::try_from(rows).unwrap_or(i64::MAX))
        });
        self.entry.error = error;
        self.entry.cancelled = cancelled;
        self.entry
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use spelunk_driver_api::{ConnectParams, Driver};
    use spelunk_driver_sqlite::SqliteDriver;

    use super::*;
    use crate::ResultStore;

    #[tokio::test]
    async fn language_settings_are_personal_persistent_and_coexist_with_history() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let user_dirs = std::env::temp_dir().join(format!(
            "spelunk-core-language-users-{}-{nonce}",
            std::process::id()
        ));
        let first_path = user_dirs.join("first-user/state.db");
        let second_path = user_dirs.join("second-user/state.db");
        let history = QueryHistory::open(first_path.clone()).unwrap();
        let other_user = QueryHistory::open(second_path).unwrap();
        assert_eq!(history.language().await.unwrap(), "system");
        history.set_language("es".into()).await.unwrap();
        assert_eq!(other_user.language().await.unwrap(), "system");
        assert!(matches!(
            history.set_language("fr".into()).await,
            Err(HistoryError::UnsupportedLanguage)
        ));
        assert_eq!(history.language().await.unwrap(), "es");
        history
            .record(NewHistoryEntry {
                at_ms: 1,
                workspace: "/repo".into(),
                connection: "shop".into(),
                statement: "select 1".into(),
                duration_ms: 2,
                rows: 1,
                error: None,
                cancelled: false,
            })
            .await
            .unwrap();
        history
            .set_enabled("/repo".into(), "shop".into(), false)
            .await
            .unwrap();
        let reopened = QueryHistory::open(first_path).unwrap();
        assert_eq!(reopened.language().await.unwrap(), "es");
        assert_eq!(
            reopened
                .list("/repo".into(), "select".into(), 10, 0)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            !reopened
                .enabled("/repo".into(), "shop".into())
                .await
                .unwrap()
        );
        history.set_language("en".into()).await.unwrap();
        assert_eq!(reopened.language().await.unwrap(), "en");
        history.set_language("system".into()).await.unwrap();
        assert_eq!(reopened.language().await.unwrap(), "system");
    }

    #[tokio::test]
    async fn invalid_language_is_rejected_before_reaching_the_store_actor() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        assert!(matches!(
            history.set_language("es-ES".into()).await,
            Err(HistoryError::UnsupportedLanguage)
        ));
        assert!(matches!(
            history.set_language("es".into()).await,
            Err(HistoryError::Closed)
        ));
    }

    #[tokio::test]
    async fn unavailable_history_warns_without_changing_query_execution() {
        let (requests, receiver) = mpsc::channel(1);
        drop(receiver);
        let history = QueryHistory { requests };
        let driver = SqliteDriver::default();
        let connection = driver
            .connect(&ConnectParams {
                database: ":memory:".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        let session = Session::new(connection, driver.dialect(), Arc::new(ResultStore::new()));
        let events: Vec<_> = history
            .run_script(
                &session,
                "select 'sensitive_value'",
                "/repo".into(),
                "shop".into(),
            )
            .collect()
            .await;
        assert!(
            matches!(events.first(), Some(CoreEvent::HistoryWarning { message })
            if message.contains("history is unavailable") && !message.contains("sensitive_value"))
        );
        assert!(matches!(
            events.last(),
            Some(CoreEvent::ScriptFinished {
                executed: 1,
                failed: false
            })
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, CoreEvent::Rows { total_rows: 1, .. }))
        );
    }
}
