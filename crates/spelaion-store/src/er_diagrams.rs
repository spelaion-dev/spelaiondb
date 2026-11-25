//! Saved ER diagrams (ADR-084): per workspace and connection like the explorer's filters and
//! bookmarks, and only ever here: local data that never leaves the machine. The store keeps
//! each one's name and its document as text; `spelaion-core`'s `er/saved.rs` writes the
//! document and checks it again every time it is read back, so nothing here interprets it.

use rusqlite::{OptionalExtension, params};

use crate::{Store, StoreError, now_ms};

/// Longest name kept, in characters (the core allows fewer).
pub const ER_DIAGRAM_NAME_MAX: usize = 200;
/// Largest document kept, in bytes (the core's bounds keep one far smaller).
pub const ER_DIAGRAM_DOCUMENT_MAX: usize = 4 * 1024 * 1024;
/// Most diagrams one store keeps; more is refused rather than silently dropped.
pub const MAX_ER_DIAGRAMS: usize = 2_000;

pub(crate) const MIGRATION: &str = "
create table er_diagrams (
    id          integer primary key,
    workspace   text not null,
    connection  text not null,
    name        text not null check (length(name) between 1 and 200),
    document    text not null check (length(cast(document as blob)) <= 4194304),
    updated_ms  integer not null,
    unique (workspace, connection, name)
);
";

/// A saved diagram as listed: what it is called and whose it is, not its document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredErDiagram {
    pub id: i64,
    pub workspace: String,
    pub connection: String,
    pub name: String,
    pub updated_ms: i64,
}

/// A saved diagram with its document, as it was written: not checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredErDiagramDocument {
    pub diagram: StoredErDiagram,
    pub document: String,
}

/// What saving a diagram did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErDiagramSave {
    /// Saved, under this id.
    Saved(i64),
    /// Another diagram of the same workspace and connection already has the name.
    NameTaken,
    /// The diagram to replace is not there any more.
    Missing,
    /// The store already keeps the most diagrams it keeps.
    Full,
}

impl Store {
    /// Every saved diagram, by workspace, connection and name.
    pub fn er_diagrams(&self) -> Result<Vec<StoredErDiagram>, StoreError> {
        let mut statement = self.conn.prepare(
            "select id, workspace, connection, name, updated_ms from er_diagrams
             order by workspace, connection, name collate nocase, id",
        )?;
        let rows = statement.query_map([], |r| {
            Ok(StoredErDiagram {
                id: r.get(0)?,
                workspace: r.get(1)?,
                connection: r.get(2)?,
                name: r.get(3)?,
                updated_ms: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// One saved diagram with its document, if it is there.
    pub fn er_diagram(&self, id: i64) -> Result<Option<StoredErDiagramDocument>, StoreError> {
        Ok(self
            .conn
            .query_row(
                "select id, workspace, connection, name, updated_ms, document
                 from er_diagrams where id = ?1",
                params![id],
                |r| {
                    Ok(StoredErDiagramDocument {
                        diagram: StoredErDiagram {
                            id: r.get(0)?,
                            workspace: r.get(1)?,
                            connection: r.get(2)?,
                            name: r.get(3)?,
                            updated_ms: r.get(4)?,
                        },
                        document: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// Saves a diagram: a new one when `id` is `None`, else over that one (which keeps its
    /// workspace and connection). The name must not be another diagram's of the same
    /// workspace and connection.
    pub fn save_er_diagram(
        &self,
        id: Option<i64>,
        workspace: &str,
        connection: &str,
        name: &str,
        document: &str,
    ) -> Result<ErDiagramSave, StoreError> {
        if name.is_empty() || name.chars().count() > ER_DIAGRAM_NAME_MAX || name.contains('\0') {
            return Err(StoreError::new(
                "a diagram's name is 1 to 200 characters".into(),
            ));
        }
        if document.len() > ER_DIAGRAM_DOCUMENT_MAX || document.contains('\0') {
            return Err(StoreError::new("the diagram is too large to save".into()));
        }
        let transaction = self.conn.unchecked_transaction()?;
        // A replaced diagram keeps its owner: the caller's workspace and connection only name
        // a new one.
        let owner = match id {
            Some(id) => {
                let Some(owner) = transaction
                    .query_row(
                        "select workspace, connection from er_diagrams where id = ?1",
                        params![id],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()?
                else {
                    return Ok(ErDiagramSave::Missing);
                };
                owner
            }
            None => (workspace.to_string(), connection.to_string()),
        };
        let taken: bool = transaction.query_row(
            "select exists(select 1 from er_diagrams
             where workspace = ?1 and connection = ?2 and name = ?3 and id is not ?4)",
            params![owner.0, owner.1, name, id],
            |r| r.get(0),
        )?;
        if taken {
            return Ok(ErDiagramSave::NameTaken);
        }
        let saved = match id {
            Some(id) => {
                transaction.execute(
                    "update er_diagrams set name = ?2, document = ?3, updated_ms = ?4
                     where id = ?1",
                    params![id, name, document, now_ms()],
                )?;
                id
            }
            None => {
                let count: i64 =
                    transaction.query_row("select count(*) from er_diagrams", [], |r| r.get(0))?;
                if count >= MAX_ER_DIAGRAMS as i64 {
                    return Ok(ErDiagramSave::Full);
                }
                transaction.execute(
                    "insert into er_diagrams (workspace, connection, name, document, updated_ms)
                     values (?1, ?2, ?3, ?4, ?5)",
                    params![owner.0, owner.1, name, document, now_ms()],
                )?;
                transaction.last_insert_rowid()
            }
        };
        transaction.commit()?;
        Ok(ErDiagramSave::Saved(saved))
    }

    /// Deletes a saved diagram; whether it was there.
    pub fn delete_er_diagram(&self, id: i64) -> Result<bool, StoreError> {
        Ok(self
            .conn
            .execute("delete from er_diagrams where id = ?1", params![id])?
            > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MIGRATIONS;
    use crate::tests::temp;

    #[test]
    fn diagrams_are_saved_by_name_per_connection_and_kept() {
        let path = temp("er-diagrams");
        let store = Store::open(&path).unwrap();
        let ErDiagramSave::Saved(first) = store
            .save_er_diagram(None, "/repo", "shop", "Orders", "{\"v\":1}")
            .unwrap()
        else {
            panic!("not saved");
        };
        // The same name on another connection, or in another workspace, is another diagram.
        assert!(matches!(
            store
                .save_er_diagram(None, "/repo", "crm", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::Saved(_)
        ));
        assert!(matches!(
            store
                .save_er_diagram(None, "", "shop", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::Saved(_)
        ));
        assert_eq!(
            store
                .save_er_diagram(None, "/repo", "shop", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::NameTaken
        );
        let ErDiagramSave::Saved(second) = store
            .save_er_diagram(None, "/repo", "shop", "Billing", "{}")
            .unwrap()
        else {
            panic!("not saved");
        };
        assert_eq!(
            store
                .save_er_diagram(Some(second), "/repo", "shop", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::NameTaken
        );
        // Saving over one keeps its owner, whatever the caller names.
        assert_eq!(
            store
                .save_er_diagram(Some(first), "/other", "other", "Orders", "{\"v\":2}")
                .unwrap(),
            ErDiagramSave::Saved(first)
        );
        drop(store);
        let store = Store::open(&path).unwrap();
        let one = store.er_diagram(first).unwrap().unwrap();
        assert_eq!(one.document, "{\"v\":2}");
        assert_eq!(
            (
                one.diagram.workspace.as_str(),
                one.diagram.connection.as_str()
            ),
            ("/repo", "shop")
        );
        let names: Vec<_> = store
            .er_diagrams()
            .unwrap()
            .into_iter()
            .map(|d| format!("{}|{}|{}", d.workspace, d.connection, d.name))
            .collect();
        assert_eq!(
            names,
            [
                "|shop|Orders",
                "/repo|crm|Orders",
                "/repo|shop|Billing",
                "/repo|shop|Orders"
            ]
        );
        assert!(store.delete_er_diagram(first).unwrap());
        assert!(!store.delete_er_diagram(first).unwrap());
        assert_eq!(store.er_diagram(first).unwrap(), None);
        assert_eq!(
            store
                .save_er_diagram(Some(first), "/repo", "shop", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::Missing
        );
        assert!(
            store
                .save_er_diagram(None, "/repo", "shop", "", "{}")
                .is_err()
        );
        assert!(
            store
                .save_er_diagram(
                    None,
                    "/repo",
                    "shop",
                    "x",
                    &"x".repeat(ER_DIAGRAM_DOCUMENT_MAX + 1)
                )
                .is_err()
        );
    }

    #[test]
    fn a_moved_connection_keeps_its_diagrams() {
        let store = Store::open(&temp("er-diagrams-move")).unwrap();
        store
            .save_er_diagram(None, "/repo", "shop", "Orders", "{}")
            .unwrap();
        store
            .save_er_diagram(None, "/repo", "team/shop", "Stale", "{}")
            .unwrap();
        store
            .move_connection_settings(Some("/repo"), "shop", "team/shop")
            .unwrap();
        let listed: Vec<_> = store
            .er_diagrams()
            .unwrap()
            .into_iter()
            .map(|d| (d.connection, d.name))
            .collect();
        assert_eq!(listed, [("team/shop".to_string(), "Orders".to_string())]);
    }

    #[test]
    fn the_migration_keeps_what_the_store_already_had() {
        let path = temp("er-diagrams-migration");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let before = MIGRATIONS
                .iter()
                .position(|migration| *migration == MIGRATION)
                .unwrap();
            for migration in &MIGRATIONS[..before] {
                conn.execute_batch(migration).unwrap();
            }
            conn.pragma_update(None, "user_version", before as i64)
                .unwrap();
            conn.execute_batch("insert into personal_settings values('language','es');")
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(
            store.personal_setting("language").unwrap().as_deref(),
            Some("es")
        );
        assert!(matches!(
            store
                .save_er_diagram(None, "", "shop", "Orders", "{}")
                .unwrap(),
            ErDiagramSave::Saved(_)
        ));
        // The checks are the table's own: nothing nameless gets in behind the functions.
        assert!(
            store
                .conn
                .execute(
                    "insert into er_diagrams (workspace, connection, name, document, updated_ms)
                     values ('', 'shop', '', '{}', 0)",
                    [],
                )
                .is_err()
        );
    }
}
