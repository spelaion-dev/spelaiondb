//! Several folders open at once (ADR-075, SDB-497): each folder is one `Config`, loaded alone
//! with the personal layer. A folder's connection sees its own folder and the personal layer,
//! never another folder; a personal connection takes from every open folder only what
//! tightens. Every test tries to get around that, and checks that one folder open behaves as
//! before. Temporary directories and a fake secret backend: the real keychain and environment
//! are never touched.

use std::path::PathBuf;

use spelaion_config::{
    Config, ConnectionId, ResolveError, ResolvedConnection, Resolver, Source, TrustStore, load,
};

use crate::common::FakeBackend;

struct Folders {
    base: PathBuf,
}

impl Folders {
    fn new(name: &str) -> Self {
        let base =
            std::env::temp_dir().join(format!("spelaion-folders-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        for dir in ["a/.spelaion", "b/.spelaion", "user"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        Self { base }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.base.join(relative)
    }

    fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Folder `a` or `b`, loaded alone with the personal layer, as the window loads each.
    fn load(&self, folder: &str) -> Config {
        load(Some(&self.path(folder)), &self.path("user"))
    }

    /// The personal layer alone (no folder open).
    fn personal_only(&self) -> Config {
        load(None, &self.path("user"))
    }

    /// Both folders trusted as they are now.
    fn trust_both(&self) -> TrustStore {
        let mut trust = TrustStore::load(&self.path("user")).unwrap();
        trust.trust(&self.load("a")).unwrap();
        trust.trust(&self.load("b")).unwrap();
        trust
    }
}

impl Drop for Folders {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn ws(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::Workspace,
        path: path.into(),
    }
}

fn personal(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::User,
        path: path.into(),
    }
}

/// Resolves `id` from `own` (the folder it is resolved in) with `open` folders open.
fn resolve(
    own: &Config,
    open: &[&Config],
    trust: &TrustStore,
    id: &ConnectionId,
) -> Result<ResolvedConnection, ResolveError> {
    Resolver::new(own, trust, &FakeBackend::default())
        .with_open_folders(open)
        .resolve(id)
}

fn sqlite(env: &str) -> String {
    format!("schema_version = 1\nengine = \"sqlite\"\nenv = \"{env}\"\ndatabase = \"app.db\"\n")
}

fn through(tunnel: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"postgres\"\nhost = \"db.internal\"\ndatabase = \"orders\"\nuser = \"app\"\ntunnel = \"{tunnel}\"\n"
    )
}

fn bastion(host: &str) -> String {
    format!(
        "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nuser = \"ops\"\nauth = [\"agent\"]\n"
    )
}

const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIC0Cx5aAdFPmIIPx6ZxdIYFKbARZbBbB5NV0QIO3Qdp2";
const OTHER_KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIA6rWI3G1sz07DnfFlrouTcysQlj2P+jpNSOEWD9OJ3X";

fn hosts(key: &str) -> String {
    format!("bastion.internal ssh-ed25519 {key}\n")
}

fn message(result: Result<ResolvedConnection, ResolveError>) -> String {
    match result {
        Ok(_) => panic!("expected a refusal"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn several_folders_a_personal_connection_gets_every_folders_safeguards_and_a_folders_only_its_own()
{
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Folders::new("types");
    // Only folder B makes Production ask before scripts, and only B defines `critical`.
    fx.write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nname = \"Prod B\"\nconfirm_execution = true\nclose_idle_after = 600\nmask_columns = [\"*ssn*\"]\n\n[types.critical]\nconfirm_execution = true\n",
    );
    fx.write(
        "a/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nname = \"Prod A\"\nclose_idle_after = 1800\nmask_columns = [\"*card*\"]\n",
    );
    fx.write("user/connections/mine.toml", &sqlite("prod"));
    fx.write("user/connections/crit.toml", &sqlite("critical"));
    fx.write("a/.spelaion/connections/orders.toml", &sqlite("critical"));
    fx.write("a/.spelaion/connections/billing.toml", &sqlite("prod"));
    let trust = fx.trust_both();
    let (a, b) = (fx.load("a"), fx.load("b"));

    // One folder open: as before. A personal connection takes A's Production and nothing of B.
    let alone = resolve(&a, &[], &trust, &personal("mine")).unwrap();
    let kind = alone.kind.unwrap();
    assert!(!kind.safeguards.confirm_execution);
    assert_eq!(kind.safeguards.close_idle_after, Some(1800));
    assert_eq!(kind.name, "Prod A");
    assert_eq!(
        resolve(&a, &[&a], &trust, &personal("mine")).unwrap().kind,
        Some(kind)
    );

    // Both open: B's safeguards add to A's, the shorter idle time wins, every folder's masked
    // columns apply, and the looks come from the first folder opened.
    for own in [&a, &b] {
        let kind = resolve(own, &[&a, &b], &trust, &personal("mine"))
            .unwrap()
            .kind
            .unwrap();
        assert!(
            kind.safeguards.confirm_execution,
            "B's safeguard reaches it"
        );
        assert!(
            kind.safeguards.confirm_data_changes,
            "Production's own stays"
        );
        assert_eq!(kind.safeguards.close_idle_after, Some(600));
        assert_eq!(kind.mask_columns, ["*card*", "*ssn*"]);
        assert_eq!(kind.name, "Prod A");
    }
    let opened_b_first = resolve(&a, &[&b, &a], &trust, &personal("mine")).unwrap();
    assert_eq!(opened_b_first.kind.unwrap().name, "Prod B");
    // A type only B defines is B's for a personal connection.
    let crit = resolve(&a, &[&a, &b], &trust, &personal("crit")).unwrap();
    assert!(crit.kind.unwrap().safeguards.confirm_execution);

    // A's own connections never see B, whatever is open: `critical` names no type in A, and
    // A's Production is A's alone.
    let orders = resolve(&a, &[&a, &b], &trust, &ws("orders")).unwrap();
    assert_eq!(orders.kind, None);
    let billing = resolve(&a, &[&a, &b], &trust, &ws("billing"))
        .unwrap()
        .kind
        .unwrap();
    assert!(!billing.safeguards.confirm_execution);
    assert_eq!(billing.safeguards.close_idle_after, Some(1800));
    assert_eq!(billing.mask_columns, ["*card*"]);

    // Nothing from an untrusted folder reaches a personal connection (B revoked).
    let mut only_a = TrustStore::load(&fx.path("user")).unwrap();
    only_a.revoke(b.workspace_root.as_deref().unwrap()).unwrap();
    let kind = resolve(&a, &[&a, &b], &only_a, &personal("mine"))
        .unwrap()
        .kind
        .unwrap();
    assert!(!kind.safeguards.confirm_execution);
    // With no folder at all, only the user's and the built-in types.
    let none = fx.personal_only();
    let kind = resolve(&none, &[], &trust, &personal("mine"))
        .unwrap()
        .kind
        .unwrap();
    assert_eq!(kind.name, "Production");
    assert!(!kind.safeguards.confirm_execution);
}

#[test]
fn several_folders_a_broken_or_changed_types_file_refuses_typed_personal_connections_only() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Folders::new("broken-types");
    fx.write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nconfirm_execution = true\n",
    );
    fx.write("user/connections/mine.toml", &sqlite("prod"));
    fx.write(
        "user/connections/plain.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"app.db\"\n",
    );
    fx.write("a/.spelaion/connections/orders.toml", &sqlite("prod"));
    let trust = fx.trust_both();

    // B's file changed since it was trusted, loosening Production: the typed personal
    // connection waits for B's review; A's connections and an untyped personal one work.
    fx.write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n\n[types.prod]\nname = \"Loose\"\n",
    );
    let (a, b) = (fx.load("a"), fx.load("b"));
    match resolve(&a, &[&a, &b], &trust, &personal("mine")) {
        Err(ResolveError::Changed { root, .. }) => {
            assert_eq!(root, b.workspace_root.clone().unwrap())
        }
        other => panic!("expected B's review, got {:?}", other.map(|r| r.kind)),
    }
    resolve(&a, &[&a, &b], &trust, &ws("orders")).unwrap();
    resolve(&a, &[&a, &b], &trust, &personal("plain")).unwrap();
    // With A alone open, B's change is nobody's business: as before.
    resolve(&a, &[], &trust, &personal("mine")).unwrap();

    // B's file no longer loads: every typed personal connection waits; A's still work.
    fx.write(
        "b/.spelaion/connection-types.toml",
        "schema_version = 1\n[types.prod\n",
    );
    let b = fx.load("b");
    let refused = message(resolve(&a, &[&a, &b], &trust, &personal("mine")));
    assert!(
        refused.contains("connection types could not be read"),
        "{refused}"
    );
    resolve(&a, &[&a, &b], &trust, &ws("orders")).unwrap();
    resolve(&a, &[&a, &b], &trust, &personal("plain")).unwrap();
    resolve(&a, &[], &trust, &personal("mine")).unwrap();
}

#[test]
fn several_folders_every_folders_known_hosts_checks_a_personal_tunnel_and_a_folders_only_its_own() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Folders::new("hosts");
    let a_hosts = fx.write("a/.spelaion/known_hosts", &hosts(KEY));
    // B pins another key for the same bastion: it must be checked for a personal tunnel.
    let b_hosts = fx.write("b/.spelaion/known_hosts", &hosts(OTHER_KEY));
    fx.write("user/tunnels/bastion.toml", &bastion("bastion.internal"));
    fx.write("user/connections/mine.toml", &through("bastion"));
    fx.write("a/.spelaion/connections/orders.toml", &through("bastion"));
    let trust = fx.trust_both();
    let (a, b) = (fx.load("a"), fx.load("b"));
    let known = |own: &Config, open: &[&Config], id: &ConnectionId| {
        resolve(own, open, &trust, id)
            .unwrap()
            .tunnel
            .unwrap()
            .known_hosts
    };

    // One folder open: its file, as before.
    assert_eq!(
        known(&a, &[], &personal("mine")),
        std::slice::from_ref(&a_hosts)
    );
    // Both: every folder's, so B's different pin blocks the personal tunnel (spelaion-ssh
    // reads them all, `a_key_any_open_folder_pins_differently_blocks`).
    assert_eq!(
        known(&a, &[&a, &b], &personal("mine")),
        [a_hosts.clone(), b_hosts.clone()]
    );
    assert_eq!(
        known(&b, &[&a, &b], &personal("mine")),
        [a_hosts.clone(), b_hosts.clone()]
    );
    // A's own connection, even through the personal tunnel, only ever checks A's.
    assert_eq!(
        known(&a, &[&a, &b], &ws("orders")),
        std::slice::from_ref(&a_hosts)
    );

    // A folder whose file changed since it was trusted stops the personal tunnel until it is
    // reviewed; A's own connection does not depend on B.
    fx.write("b/.spelaion/known_hosts", &hosts(KEY));
    let b = fx.load("b");
    assert!(matches!(
        resolve(&a, &[&a, &b], &trust, &personal("mine")),
        Err(ResolveError::Changed { .. })
    ));
    assert_eq!(
        known(&a, &[&a, &b], &ws("orders")),
        std::slice::from_ref(&a_hosts)
    );

    // An untrusted folder's file is never used (B revoked).
    let mut only_a = TrustStore::load(&fx.path("user")).unwrap();
    only_a.revoke(b.workspace_root.as_deref().unwrap()).unwrap();
    let tunnel = resolve(&a, &[&a, &b], &only_a, &personal("mine"))
        .unwrap()
        .tunnel
        .unwrap();
    assert_eq!(tunnel.known_hosts, [a_hosts]);
}

#[test]
fn several_folders_a_tunnel_name_in_two_folders_is_refused_for_a_personal_connection() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Folders::new("tunnels");
    fx.write(
        "a/.spelaion/tunnels/shared.toml",
        &bastion("jump.a.internal"),
    );
    fx.write(
        "b/.spelaion/tunnels/shared.toml",
        &bastion("jump.b.internal"),
    );
    fx.write(
        "b/.spelaion/tunnels/only-b.toml",
        &bastion("jump.b.internal"),
    );
    fx.write("user/connections/mine.toml", &through("shared"));
    fx.write("user/connections/via-b.toml", &through("only-b"));
    fx.write("a/.spelaion/connections/orders.toml", &through("shared"));
    fx.write("a/.spelaion/connections/sneaky.toml", &through("only-b"));
    let trust = fx.trust_both();
    let (a, b) = (fx.load("a"), fx.load("b"));
    let first_hop = |result: Result<ResolvedConnection, ResolveError>| {
        let tunnel = result.unwrap().tunnel.unwrap();
        (tunnel.hops[0].host.clone(), tunnel.folder)
    };

    // One folder open: a personal connection uses its tunnel, as before.
    assert_eq!(
        first_hop(resolve(&a, &[], &trust, &personal("mine"))),
        ("jump.a.internal".to_string(), a.workspace_root.clone())
    );
    // Both have `shared`: refused, naming both, rather than picking one.
    let refused = message(resolve(&a, &[&a, &b], &trust, &personal("mine")));
    for root in [&a, &b].map(|c| c.workspace_root.clone().unwrap()) {
        assert!(refused.contains(&root.display().to_string()), "{refused}");
    }
    // A tunnel only B has is B's: its trust, its paths and its folder.
    assert_eq!(
        first_hop(resolve(&a, &[&a, &b], &trust, &personal("via-b"))),
        ("jump.b.internal".to_string(), b.workspace_root.clone())
    );

    // A's connection never sees B's tunnels: its own `shared`, and `only-b` does not exist.
    assert_eq!(
        first_hop(resolve(&a, &[&a, &b], &trust, &ws("orders"))),
        ("jump.a.internal".to_string(), a.workspace_root.clone())
    );
    let missing = message(resolve(&a, &[&a, &b], &trust, &ws("sneaky")));
    assert!(missing.contains("does not exist"), "{missing}");

    // A personal tunnel of the same name comes first, and is nobody's folder.
    fx.write("user/tunnels/shared.toml", &bastion("jump.mine.internal"));
    let (a, b) = (fx.load("a"), fx.load("b"));
    assert_eq!(
        first_hop(resolve(&a, &[&a, &b], &trust, &personal("mine"))),
        ("jump.mine.internal".to_string(), None)
    );
    // B's tunnel changed since it was trusted: the personal connection that uses it waits.
    fx.write(
        "b/.spelaion/tunnels/only-b.toml",
        &bastion("evil.example.com"),
    );
    let b = fx.load("b");
    assert!(matches!(
        resolve(&a, &[&a, &b], &trust, &personal("via-b")),
        Err(ResolveError::Changed { .. })
    ));
}

#[test]
fn several_folders_the_same_folder_twice_is_one_folder() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Folders::new("twice");
    fx.write(
        "a/.spelaion/tunnels/shared.toml",
        &bastion("jump.a.internal"),
    );
    let a_hosts = fx.write("a/.spelaion/known_hosts", &hosts(KEY));
    fx.write("user/connections/mine.toml", &through("shared"));
    let trust = fx.trust_both();
    let (a, again) = (fx.load("a"), fx.load("a"));
    let tunnel = resolve(&a, &[&a, &again], &trust, &personal("mine"))
        .unwrap()
        .tunnel
        .unwrap();
    assert_eq!(tunnel.hops[0].host, "jump.a.internal");
    assert_eq!(tunnel.known_hosts, [a_hosts]);
}
