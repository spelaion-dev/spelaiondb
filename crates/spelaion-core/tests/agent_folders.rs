//! The MCP page's view of several open folders (ADR-075 point 9, SDB-503): each folder's
//! connections under that folder, decided as a server started there decides them, and the
//! per-connection override kept per folder, so two folders' `prod/orders` never share one.
//! Temporary folders and a temporary store: the user's are never touched.

use std::path::PathBuf;
use std::sync::Arc;

use spelaion_config::{Config, Source, TrustStore, canonical_folder, load};
use spelaion_core::{AgentFolder, AgentView, FolderAgents, QueryHistory, agents_by_folder};

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "spelaion-agent-folders-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        // Two folders with a connection of the same id, and one personal connection.
        let orders = "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"app.db\"\n\n[mcp]\nexposed = true\nwrites = \"allow\"\n";
        for (path, content) in [
            ("a/.spelaion/connections/prod/orders.toml", orders),
            ("b/.spelaion/connections/prod/orders.toml", orders),
            ("user/connections/mine.toml", orders),
        ] {
            let path = base.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        Self { base }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    fn load(&self, folder: &str) -> Arc<Config> {
        Arc::new(load(Some(&self.path(folder)), &self.path("user")))
    }

    fn history(&self) -> QueryHistory {
        QueryHistory::open(self.path("store.db")).unwrap()
    }
}

/// The folder root of each group, `None` for the personal one, with its connections' ids.
fn shape(groups: &[FolderAgents]) -> Vec<(Option<PathBuf>, Vec<String>)> {
    groups
        .iter()
        .map(|group| {
            let folder = match &group.folder {
                AgentFolder::Repository { root, .. } => Some(root.clone()),
                AgentFolder::Personal => None,
            };
            let ids = group
                .connections
                .iter()
                .map(|c| c.entry.id.to_string())
                .collect();
            (folder, ids)
        })
        .collect()
}

#[tokio::test]
async fn each_open_folder_lists_its_own_connections_under_it_with_its_own_trust() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("groups");
    let (a, b) = (fx.load("a"), fx.load("b"));
    // Only `a` is trusted: `b`'s connection is not offered, as a server started in `b` decides.
    let mut trust = TrustStore::load(&fx.path("user")).unwrap();
    trust.trust(&a).unwrap();
    let history = fx.history();
    let groups = agents_by_folder(&[a.clone(), b.clone()], &b, &trust, &history).await;
    let (root_a, root_b) = (
        canonical_folder(&fx.path("a")),
        canonical_folder(&fx.path("b")),
    );
    assert_eq!(
        shape(&groups),
        [
            (Some(root_a.clone()), vec!["prod/orders".to_string()]),
            (Some(root_b.clone()), vec!["prod/orders".to_string()]),
            (None, vec!["~/mine".to_string()]),
        ]
    );
    let trusted: Vec<_> = groups
        .iter()
        .filter_map(|g| match &g.folder {
            AgentFolder::Repository { trusted, .. } => Some(*trusted),
            AgentFolder::Personal => None,
        })
        .collect();
    assert_eq!(trusted, [true, false]);
    let (in_a, in_b) = (&groups[0].connections[0], &groups[1].connections[0]);
    assert!(
        matches!(in_a.view, AgentView::Visible(_)),
        "{:?}",
        in_a.view
    );
    assert_eq!(in_b.view, AgentView::UntrustedRepository);
    // Each is its folder's: the same id, two identities, two override scopes.
    assert_eq!(in_a.key.folder(), Some(root_a.as_path()));
    assert_eq!(in_b.key.folder(), Some(root_b.as_path()));
    assert_ne!(in_a.override_scope, in_b.override_scope);
    let personal = &groups[2].connections[0];
    assert_eq!(personal.entry.id.source, Source::User);
    assert_eq!(personal.key.folder(), None);
    assert_eq!(personal.override_scope.as_deref(), Some(""));
    assert!(matches!(personal.view, AgentView::Visible(_)));

    // With no folder open, only the personal connections.
    let alone = Arc::new(load(None, &fx.path("user")));
    let groups = agents_by_folder(&[], &alone, &trust, &history).await;
    assert_eq!(shape(&groups), [(None, vec!["~/mine".to_string()])]);
}

#[tokio::test]
async fn setting_the_override_on_one_folders_connection_leaves_the_other_folders() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("override");
    let (a, b) = (fx.load("a"), fx.load("b"));
    let mut trust = TrustStore::load(&fx.path("user")).unwrap();
    trust.trust(&a).unwrap();
    trust.trust(&b).unwrap();
    let history = fx.history();
    let open = [a.clone(), b.clone()];
    let before = agents_by_folder(&open, &b, &trust, &history).await;
    let (in_a, in_b) = (&before[0].connections[0], &before[1].connections[0]);
    let initial_b = (in_b.read_only_override, in_b.view);
    let flipped = !in_a.read_only_override.expect("the override is read");

    history
        .set_mcp_connection_read_only(
            in_a.override_scope.clone().unwrap(),
            in_a.entry.id.to_string(),
            flipped,
        )
        .await
        .unwrap();
    let after = agents_by_folder(&open, &b, &trust, &history).await;
    let (in_a, in_b) = (&after[0].connections[0], &after[1].connections[0]);
    assert_eq!(in_a.read_only_override, Some(flipped));
    assert_ne!(in_a.view, before[0].connections[0].view, "a's view follows");
    assert_eq!(
        (in_b.read_only_override, in_b.view),
        initial_b,
        "b's connection of the same id keeps its own override"
    );
}
