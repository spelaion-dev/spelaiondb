//! The lock view next to the process monitor: who holds which lock, who waits, and who blocks
//! whom. Every statement is a read checked by [`crate::observe::read`]; ending a blocking
//! session goes through [`crate::control_process`] and its checks, never through here.

use std::collections::{BTreeMap, BTreeSet};

use crate::Session;
use crate::observe::{read, whole_number};

/// One lock held or requested by a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lock {
    /// The process id of the session (the process monitor's `Process::id`).
    pub session: String,
    /// What is locked, as the server names it: `schema.table`, a transaction, a page…
    pub object: String,
    /// The kind of resource (`relation`, `transactionid`, `RECORD`, `KEY`, `metadata TABLE`…).
    pub lock_type: String,
    /// The lock mode, as the server writes it (`RowExclusiveLock`, `X,REC_NOT_GAP`, `IX`…).
    pub mode: String,
    pub granted: bool,
    /// Seconds since it was granted (its transaction's start) or since the wait began.
    pub seconds: Option<i64>,
    /// What else the server says about it (index, key, page, duration).
    pub detail: Option<String>,
}

/// `waiting` waits for a lock that `blocking` holds (or is ahead of it in the queue).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LockWait {
    pub waiting: String,
    pub blocking: String,
}

/// One row of the blocking tree, in display order: a session that blocks others and is not
/// waiting itself at depth 0, the sessions waiting for it under it, and so on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockingNode {
    pub session: String,
    pub depth: u32,
    /// The session is already above it on this branch: a deadlock the server has not broken
    /// yet. Its branch stops here.
    pub cycle: bool,
}

pub struct LockSnapshot {
    /// The monitoring session, whose own locks are left out.
    pub own_id: String,
    pub locks: Vec<Lock>,
    pub waits: Vec<LockWait>,
    pub tree: Vec<BlockingNode>,
}

/// At most this many rows in the blocking tree: a session waiting for several others shows
/// under each of them, so a tangle could otherwise grow very large.
const TREE_LIMIT: usize = 2_000;

/// Reads the server's locks and waits once.
pub async fn list_locks(session: &Session) -> Result<LockSnapshot, String> {
    let engine = session.dialect().name();
    let (own_sql, locks_sql, waits_sql) = match engine {
        "postgres" => ("SELECT pg_backend_pid()", POSTGRES_LOCKS, POSTGRES_WAITS),
        "sqlserver" => ("SELECT @@SPID", SQLSERVER_LOCKS, SQLSERVER_WAITS),
        "mysql" => {
            let version = read(session, "SELECT VERSION()").await?;
            let mariadb = version
                .rows
                .first()
                .and_then(|row| row.first().cloned().flatten())
                .is_some_and(|v| v.to_ascii_lowercase().contains("mariadb"));
            if mariadb {
                ("SELECT CONNECTION_ID()", MARIADB_LOCKS, MARIADB_WAITS)
            } else {
                ("SELECT CONNECTION_ID()", MYSQL_LOCKS, MYSQL_WAITS)
            }
        }
        _ => return Err("the lock view is not supported for this engine".into()),
    };
    let own = read(session, own_sql).await?;
    let own_id = own
        .rows
        .first()
        .and_then(|row| row.first().cloned().flatten())
        .ok_or("the server did not return this session's id")?;

    let rows = read(session, locks_sql).await?;
    let [
        session_col,
        object,
        lock_type,
        mode,
        status,
        seconds,
        detail,
    ] = [
        rows.column("session")?,
        rows.column("object")?,
        rows.column("lock_type")?,
        rows.column("mode")?,
        rows.column("status")?,
        rows.column("seconds")?,
        rows.column("detail")?,
    ];
    let mut locks = Vec::new();
    for row in &rows.rows {
        let Some(id) = row[session_col].clone() else {
            continue;
        };
        if id == own_id {
            continue;
        }
        locks.push(Lock {
            session: id,
            object: row[object].clone().unwrap_or_default(),
            lock_type: row[lock_type].clone().unwrap_or_default(),
            mode: row[mode].clone().unwrap_or_default(),
            granted: row[status].as_deref() == Some("granted"),
            seconds: whole_number(row[seconds].as_deref()),
            detail: row[detail].clone().filter(|d| !d.trim().is_empty()),
        });
    }
    locks.sort_by(|a, b| {
        session_order(&a.session, &b.session)
            .then(a.granted.cmp(&b.granted))
            .then(a.object.cmp(&b.object))
    });

    let rows = read(session, waits_sql).await?;
    let [waiting, blocking] = [rows.column("waiting")?, rows.column("blocking")?];
    let waits: Vec<LockWait> = rows
        .rows
        .iter()
        .filter_map(|row| {
            Some(LockWait {
                waiting: row[waiting].clone()?,
                blocking: row[blocking].clone()?,
            })
        })
        .filter(|w| w.waiting != w.blocking)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let tree = blocking_tree(&waits);
    Ok(LockSnapshot {
        own_id,
        locks,
        waits,
        tree,
    })
}

/// Numeric ids in numeric order, anything else after them in text order.
fn session_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

/// Who blocks whom, as a tree: the sessions that block others without waiting themselves
/// first (the ones to look at), each followed by the sessions waiting for it. A cycle with no
/// such head (a deadlock) starts from its lowest id and is marked where it closes.
pub fn blocking_tree(waits: &[LockWait]) -> Vec<BlockingNode> {
    let mut waiting_for: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    let mut waiters: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for wait in waits {
        waiting_for
            .entry(&wait.waiting)
            .or_default()
            .push(&wait.blocking);
        waiters
            .entry(&wait.blocking)
            .or_default()
            .push(&wait.waiting);
    }
    for list in waiters.values_mut() {
        list.sort_by(|a, b| session_order(a, b));
        list.dedup();
    }
    let mut heads: Vec<&str> = waiters
        .keys()
        .copied()
        .filter(|s| !waiting_for.contains_key(s))
        .collect();
    heads.sort_by(|a, b| session_order(a, b));

    let mut tree = Vec::new();
    let mut seen = BTreeSet::new();
    let mut path = Vec::new();
    for head in heads {
        walk(head, 0, &waiters, &mut path, &mut seen, &mut tree);
    }
    // What is left only waits in cycles: start each from its lowest id.
    let mut rest: Vec<&str> = waiters
        .keys()
        .copied()
        .filter(|s| !seen.contains(s))
        .collect();
    rest.sort_by(|a, b| session_order(a, b));
    for start in rest {
        if !seen.contains(start) {
            walk(start, 0, &waiters, &mut path, &mut seen, &mut tree);
        }
    }
    tree
}

fn walk<'a>(
    session: &'a str,
    depth: u32,
    waiters: &BTreeMap<&'a str, Vec<&'a str>>,
    path: &mut Vec<&'a str>,
    seen: &mut BTreeSet<&'a str>,
    tree: &mut Vec<BlockingNode>,
) {
    if tree.len() >= TREE_LIMIT {
        return;
    }
    let cycle = path.contains(&session);
    tree.push(BlockingNode {
        session: session.to_string(),
        depth,
        cycle,
    });
    seen.insert(session);
    if cycle {
        return;
    }
    path.push(session);
    for waiter in waiters.get(session).map(Vec::as_slice).unwrap_or_default() {
        walk(waiter, depth + 1, waiters, path, seen, tree);
    }
    path.pop();
}

// Each transaction holds an exclusive lock on its own virtual id: only the waits for one are
// shown (CREATE INDEX CONCURRENTLY waits on them). Relations of other databases have no name
// here (pg_class is per database). `waitstart` is PostgreSQL 14+, so a wait counts from the
// start of the statement that waits.
pub(crate) const POSTGRES_LOCKS: &str = r#"
SELECT l.pid::text AS session,
       CASE WHEN l.relation IS NOT NULL THEN COALESCE(n.nspname || '.' || c.relname, 'relation ' || l.relation::text)
            WHEN l.locktype = 'transactionid' THEN 'transaction ' || l.transactionid::text
            WHEN l.locktype = 'virtualxid' THEN 'virtual transaction ' || l.virtualxid
            WHEN l.locktype = 'advisory' THEN 'advisory ' || COALESCE(l.classid::text, '') || ':' || COALESCE(l.objid::text, '')
            ELSE l.locktype END AS object,
       l.locktype AS lock_type, l.mode AS mode,
       CASE WHEN l.granted THEN 'granted' ELSE 'waiting' END AS status,
       EXTRACT(EPOCH FROM (clock_timestamp() - CASE WHEN l.granted THEN COALESCE(a.xact_start, a.query_start, a.backend_start) ELSE COALESCE(a.query_start, a.state_change) END))::bigint AS seconds,
       CASE WHEN l.locktype IN ('tuple', 'page') THEN 'page ' || l.page::text || COALESCE(', tuple ' || l.tuple::text, '') END AS detail
FROM pg_catalog.pg_locks AS l
LEFT JOIN pg_catalog.pg_stat_activity AS a ON a.pid = l.pid
LEFT JOIN pg_catalog.pg_class AS c ON c.oid = l.relation AND l.database = (SELECT oid FROM pg_catalog.pg_database WHERE datname = current_database())
LEFT JOIN pg_catalog.pg_namespace AS n ON n.oid = c.relnamespace
WHERE l.pid IS NOT NULL AND l.pid <> pg_backend_pid() AND NOT (l.locktype = 'virtualxid' AND l.granted)"#;

pub(crate) const POSTGRES_WAITS: &str = r#"
SELECT a.pid::text AS waiting, b.pid::text AS blocking
FROM pg_catalog.pg_stat_activity AS a
CROSS JOIN LATERAL unnest(pg_catalog.pg_blocking_pids(a.pid)) AS b(pid)
WHERE a.wait_event_type = 'Lock'"#;

// InnoDB's locks (MySQL 8.0+) and the metadata locks of tables, schemas and the server.
// Pairs for metadata locks are a pending lock and every other session's granted lock on the
// same object, as MySQL's own sys.schema_table_lock_waits does: a session queued behind a
// pending exclusive lock shows the holders as its blockers.
pub(crate) const MYSQL_LOCKS: &str = r#"
SELECT CAST(t.PROCESSLIST_ID AS CHAR) AS session,
       CONCAT_WS('.', l.OBJECT_SCHEMA, l.OBJECT_NAME) AS object,
       l.LOCK_TYPE AS lock_type, l.LOCK_MODE AS mode,
       CASE WHEN l.LOCK_STATUS = 'GRANTED' THEN 'granted' ELSE 'waiting' END AS status,
       TIMESTAMPDIFF(SECOND, CASE WHEN l.LOCK_STATUS = 'GRANTED' THEN x.trx_started ELSE x.trx_wait_started END, NOW()) AS seconds,
       CONCAT_WS(', ', CONCAT('index ', l.INDEX_NAME), CONCAT('data ', l.LOCK_DATA)) AS detail
FROM performance_schema.data_locks AS l
JOIN performance_schema.threads AS t ON t.THREAD_ID = l.THREAD_ID
LEFT JOIN information_schema.INNODB_TRX AS x ON x.trx_mysql_thread_id = t.PROCESSLIST_ID
WHERE t.PROCESSLIST_ID IS NOT NULL AND t.PROCESSLIST_ID <> CONNECTION_ID()
UNION ALL
SELECT CAST(t.PROCESSLIST_ID AS CHAR) AS session,
       COALESCE(CONCAT_WS('.', m.OBJECT_SCHEMA, m.OBJECT_NAME), m.OBJECT_TYPE) AS object,
       CONCAT('metadata ', m.OBJECT_TYPE) AS lock_type, m.LOCK_TYPE AS mode,
       CASE WHEN m.LOCK_STATUS = 'GRANTED' THEN 'granted' ELSE 'waiting' END AS status,
       CASE WHEN m.LOCK_STATUS = 'GRANTED' THEN TIMESTAMPDIFF(SECOND, x.trx_started, NOW()) ELSE t.PROCESSLIST_TIME END AS seconds,
       CONCAT('duration ', m.LOCK_DURATION) AS detail
FROM performance_schema.metadata_locks AS m
JOIN performance_schema.threads AS t ON t.THREAD_ID = m.OWNER_THREAD_ID
LEFT JOIN information_schema.INNODB_TRX AS x ON x.trx_mysql_thread_id = t.PROCESSLIST_ID
WHERE t.PROCESSLIST_ID IS NOT NULL AND t.PROCESSLIST_ID <> CONNECTION_ID()
  AND m.LOCK_STATUS IN ('GRANTED', 'PENDING')"#;

pub(crate) const MYSQL_WAITS: &str = r#"
SELECT CAST(r.PROCESSLIST_ID AS CHAR) AS waiting, CAST(b.PROCESSLIST_ID AS CHAR) AS blocking
FROM performance_schema.data_lock_waits AS w
JOIN performance_schema.threads AS r ON r.THREAD_ID = w.REQUESTING_THREAD_ID
JOIN performance_schema.threads AS b ON b.THREAD_ID = w.BLOCKING_THREAD_ID
WHERE r.PROCESSLIST_ID IS NOT NULL AND b.PROCESSLIST_ID IS NOT NULL
UNION
SELECT CAST(r.PROCESSLIST_ID AS CHAR) AS waiting, CAST(b.PROCESSLIST_ID AS CHAR) AS blocking
FROM performance_schema.metadata_locks AS p
JOIN performance_schema.metadata_locks AS g
  ON g.OBJECT_TYPE = p.OBJECT_TYPE AND g.OBJECT_SCHEMA <=> p.OBJECT_SCHEMA AND g.OBJECT_NAME <=> p.OBJECT_NAME
 AND g.LOCK_STATUS = 'GRANTED' AND g.OWNER_THREAD_ID <> p.OWNER_THREAD_ID
JOIN performance_schema.threads AS r ON r.THREAD_ID = p.OWNER_THREAD_ID
JOIN performance_schema.threads AS b ON b.THREAD_ID = g.OWNER_THREAD_ID
WHERE p.LOCK_STATUS = 'PENDING' AND r.PROCESSLIST_ID IS NOT NULL AND b.PROCESSLIST_ID IS NOT NULL"#;

// MariaDB lists only the InnoDB locks that block or wait (it has no data_locks, and its
// metadata lock view is an optional plugin).
pub(crate) const MARIADB_LOCKS: &str = r#"
SELECT CAST(x.trx_mysql_thread_id AS CHAR) AS session, l.lock_table AS object,
       l.lock_type AS lock_type, l.lock_mode AS mode,
       CASE WHEN w.requested_lock_id IS NULL THEN 'granted' ELSE 'waiting' END AS status,
       TIMESTAMPDIFF(SECOND, CASE WHEN w.requested_lock_id IS NULL THEN x.trx_started ELSE x.trx_wait_started END, NOW()) AS seconds,
       CONCAT_WS(', ', CONCAT('index ', l.lock_index), CONCAT('data ', l.lock_data)) AS detail
FROM information_schema.INNODB_LOCKS AS l
JOIN information_schema.INNODB_TRX AS x ON x.trx_id = l.lock_trx_id
LEFT JOIN (SELECT DISTINCT requested_lock_id FROM information_schema.INNODB_LOCK_WAITS) AS w ON w.requested_lock_id = l.lock_id
WHERE x.trx_mysql_thread_id <> CONNECTION_ID()"#;

pub(crate) const MARIADB_WAITS: &str = r#"
SELECT CAST(r.trx_mysql_thread_id AS CHAR) AS waiting, CAST(b.trx_mysql_thread_id AS CHAR) AS blocking
FROM information_schema.INNODB_LOCK_WAITS AS w
JOIN information_schema.INNODB_TRX AS r ON r.trx_id = w.requesting_trx_id
JOIN information_schema.INNODB_TRX AS b ON b.trx_id = w.blocking_trx_id"#;

// Every session holds a shared lock on its database: those are left out. Keys, pages and
// rows are named through sys.partitions, which only knows the current database.
pub(crate) const SQLSERVER_LOCKS: &str = r#"
SELECT CONVERT(varchar(11), l.request_session_id) AS session,
       CASE WHEN l.resource_type = 'OBJECT' THEN COALESCE(DB_NAME(l.resource_database_id) + '.' + OBJECT_SCHEMA_NAME(l.resource_associated_entity_id, l.resource_database_id) + '.' + OBJECT_NAME(l.resource_associated_entity_id, l.resource_database_id), CONVERT(varchar(20), l.resource_associated_entity_id))
            WHEN l.resource_type IN ('KEY', 'PAGE', 'RID', 'HOBT') THEN COALESCE(DB_NAME(l.resource_database_id) + '.' + OBJECT_SCHEMA_NAME(p.object_id, l.resource_database_id) + '.' + OBJECT_NAME(p.object_id, l.resource_database_id), CONVERT(varchar(20), l.resource_associated_entity_id))
            WHEN l.resource_type = 'DATABASE' THEN DB_NAME(l.resource_database_id)
            ELSE l.resource_type END AS object,
       l.resource_type AS lock_type, l.request_mode AS mode,
       CASE WHEN l.request_status = 'GRANT' THEN 'granted' ELSE 'waiting' END AS status,
       CASE WHEN l.request_status = 'GRANT' THEN DATEDIFF_BIG(second, t.transaction_begin_time, SYSDATETIME()) ELSE CONVERT(bigint, wt.wait_duration_ms) / 1000 END AS seconds,
       NULLIF(LTRIM(RTRIM(l.resource_description)), '') AS detail
FROM sys.dm_tran_locks AS l
LEFT JOIN sys.partitions AS p ON p.hobt_id = l.resource_associated_entity_id AND l.resource_database_id = DB_ID() AND l.resource_type IN ('KEY', 'PAGE', 'RID', 'HOBT')
LEFT JOIN sys.dm_tran_active_transactions AS t ON t.transaction_id = l.request_owner_id AND l.request_owner_type = 'TRANSACTION'
OUTER APPLY (SELECT TOP (1) wait_duration_ms FROM sys.dm_os_waiting_tasks WHERE resource_address = l.lock_owner_address) AS wt
WHERE l.request_session_id > 0 AND l.request_session_id <> @@SPID
  AND NOT (l.resource_type = 'DATABASE' AND l.request_mode = 'S' AND l.request_status = 'GRANT')"#;

pub(crate) const SQLSERVER_WAITS: &str = r#"
SELECT DISTINCT CONVERT(varchar(11), session_id) AS waiting, CONVERT(varchar(11), blocking_session_id) AS blocking
FROM sys.dm_os_waiting_tasks
WHERE session_id IS NOT NULL AND blocking_session_id IS NOT NULL AND blocking_session_id > 0 AND blocking_session_id <> session_id"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(waiting: &str, blocking: &str) -> LockWait {
        LockWait {
            waiting: waiting.into(),
            blocking: blocking.into(),
        }
    }

    fn rows(tree: &[BlockingNode]) -> Vec<(String, u32, bool)> {
        tree.iter()
            .map(|n| (n.session.clone(), n.depth, n.cycle))
            .collect()
    }

    #[test]
    fn a_chain_starts_from_the_session_that_waits_for_nobody() {
        // 30 waits for 20, which waits for 10; 11 also waits for 10.
        let tree = blocking_tree(&[wait("30", "20"), wait("20", "10"), wait("11", "10")]);
        assert_eq!(
            rows(&tree),
            vec![
                ("10".into(), 0, false),
                ("11".into(), 1, false),
                ("20".into(), 1, false),
                ("30".into(), 2, false),
            ]
        );
    }

    #[test]
    fn ids_sort_as_numbers() {
        let tree = blocking_tree(&[wait("100", "9"), wait("20", "9")]);
        assert_eq!(
            rows(&tree),
            vec![
                ("9".into(), 0, false),
                ("20".into(), 1, false),
                ("100".into(), 1, false),
            ]
        );
    }

    #[test]
    fn a_deadlock_is_shown_once_and_marked() {
        let tree = blocking_tree(&[wait("2", "1"), wait("1", "2"), wait("3", "2")]);
        assert_eq!(
            rows(&tree),
            vec![
                ("1".into(), 0, false),
                ("2".into(), 1, false),
                ("1".into(), 2, true),
                ("3".into(), 2, false),
            ]
        );
    }

    #[test]
    fn a_session_waiting_for_two_shows_under_both() {
        let tree = blocking_tree(&[wait("3", "1"), wait("3", "2")]);
        assert_eq!(
            rows(&tree),
            vec![
                ("1".into(), 0, false),
                ("3".into(), 1, false),
                ("2".into(), 0, false),
                ("3".into(), 1, false),
            ]
        );
    }

    #[test]
    fn a_tangle_is_bounded() {
        // A ladder where every session waits for the two before it doubles at each step.
        let mut waits = Vec::new();
        for i in 2..40 {
            waits.push(wait(&i.to_string(), &(i - 1).to_string()));
            waits.push(wait(&i.to_string(), &(i - 2).to_string()));
        }
        assert!(blocking_tree(&waits).len() <= TREE_LIMIT);
        assert!(blocking_tree(&[]).is_empty());
    }

    #[test]
    fn every_statement_is_a_read() {
        for (engine, sql) in [
            ("postgres", POSTGRES_LOCKS),
            ("postgres", POSTGRES_WAITS),
            ("mysql", MYSQL_LOCKS),
            ("mysql", MYSQL_WAITS),
            ("mysql", MARIADB_LOCKS),
            ("mysql", MARIADB_WAITS),
            ("sqlserver", SQLSERVER_LOCKS),
            ("sqlserver", SQLSERVER_WAITS),
        ] {
            crate::classify_read_only(engine, sql)
                .unwrap_or_else(|e| panic!("{engine}: {e}\n{sql}"));
        }
    }
}
