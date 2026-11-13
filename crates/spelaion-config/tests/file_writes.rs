//! What the configuration writers leave on disk on Unix: an edited file keeps its mode, a
//! symbolic link stays a link and its target gets the edit, a new file gets the mode of the
//! folder it lives in (owner-only in the personal folder, the umask's in the repository), and
//! nothing temporary is left behind, not even when several writes race.
#![cfg(unix)]

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::Path;

use common::{BASTION, FakeBackend, Fixture, personal};
use spelaion_config::{
    ConnectionDraft, ConnectionTypeConfig, Source, TrustStore, TunnelDraft, TunnelPasswords,
    add_connection, add_tunnel, duplicate_connection, move_connection, set_personal_type,
    set_tunnel_ports, set_user_vars, set_value, update_connection, update_tunnel,
};

const ORDERS: &str = r#"schema_version = 1
# The team's orders.
name = "Orders"
engine = "postgres"
host = "orders.internal"
database = "orders"
user = "app"
"#;

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn set_mode(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

fn is_link(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .unwrap()
        .file_type()
        .is_symlink()
}

/// The folder's entries, sorted: a leftover temporary file shows up here.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The mode the umask gives an ordinary new file in `dir`.
fn umask_mode(dir: &Path) -> u32 {
    let probe = dir.join(".umask-probe");
    std::fs::write(&probe, "").unwrap();
    let mode = mode(&probe);
    std::fs::remove_file(&probe).unwrap();
    mode
}

/// Changes the personal connection `id`'s host through the form.
fn edit_host(fx: &Fixture, id: &str, host: &str) {
    let config = fx.config();
    let entry = config.connection(&personal(id)).unwrap();
    let mut draft = ConnectionDraft::from_entry(entry);
    draft.host = host.into();
    update_connection(
        &config,
        &personal(id),
        &draft,
        None,
        &FakeBackend::default(),
    )
    .unwrap();
}

#[test]
fn editing_a_connection_keeps_its_mode() {
    let fx = Fixture::new("writes-mode");
    for kept in [0o600, 0o640, 0o644] {
        let path = fx.write("user/connections/orders.toml", ORDERS);
        set_mode(&path, kept);
        edit_host(&fx, "orders", "orders.example.com");
        let text = fx.read("user/connections/orders.toml");
        assert!(text.contains("host = \"orders.example.com\""), "{text}");
        assert!(text.contains("# The team's orders."), "{text}");
        assert_eq!(mode(&path), kept, "{kept:o}");
        assert_eq!(names(path.parent().unwrap()), ["orders.toml"]);
    }
}

#[test]
fn editing_a_connection_through_a_link_edits_the_target() {
    let fx = Fixture::new("writes-link");
    let target = fx.write("dotfiles/spelaion/orders.toml", ORDERS);
    set_mode(&target, 0o600);
    std::fs::create_dir_all(fx.user().join("connections")).unwrap();
    let link = fx.user().join("connections/orders.toml");
    symlink("../../dotfiles/spelaion/orders.toml", &link).unwrap();

    edit_host(&fx, "orders", "orders.example.com");
    assert!(is_link(&link), "the link was replaced by a file");
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.contains("host = \"orders.example.com\""), "{text}");
    assert_eq!(mode(&target), 0o600);
    assert_eq!(names(&fx.user().join("connections")), ["orders.toml"]);
    assert_eq!(names(target.parent().unwrap()), ["orders.toml"]);
    // And it still loads through the link, edited.
    let config = fx.config();
    let entry = config.connection(&personal("orders")).unwrap();
    assert_eq!(entry.file.host.as_deref(), Some("orders.example.com"));
}

#[test]
fn editing_a_tunnel_keeps_its_mode_and_its_link() {
    let fx = Fixture::new("writes-tunnel");
    let target = fx.write("dotfiles/bastion.toml", BASTION);
    set_mode(&target, 0o640);
    std::fs::create_dir_all(fx.user().join("tunnels")).unwrap();
    let link = fx.user().join("tunnels/bastion.toml");
    symlink(&target, &link).unwrap();

    let config = fx.config();
    let entry = config.tunnel_for(Source::User, "bastion").unwrap();
    let mut draft = TunnelDraft::from_entry(entry);
    draft.hops[0].port = Some(2200);
    update_tunnel(
        &config,
        Source::User,
        "bastion",
        &draft,
        &TunnelPasswords::default(),
        &FakeBackend::default(),
    )
    .unwrap();
    assert!(is_link(&link), "the link was replaced by a file");
    let text = std::fs::read_to_string(&target).unwrap();
    assert!(text.contains("port = 2200"), "{text}");
    assert_eq!(mode(&target), 0o640);
    assert_eq!(names(&fx.user().join("tunnels")), ["bastion.toml"]);
}

#[test]
fn personal_files_edited_through_links_stay_links_with_their_mode() {
    let fx = Fixture::new("writes-personal");
    let user_toml = fx.write("dotfiles/user.toml", "schema_version = 1\n# mine\n");
    set_mode(&user_toml, 0o644);
    symlink(&user_toml, fx.user().join("user.toml")).unwrap();
    let types = fx.write("dotfiles/connection-types.toml", "schema_version = 1\n");
    set_mode(&types, 0o640);
    symlink(&types, fx.user().join("connection-types.toml")).unwrap();

    let vars: BTreeMap<String, String> = [("team".to_string(), "data".to_string())].into();
    set_user_vars(&fx.user(), &vars).unwrap();
    set_tunnel_ports(&fx.user(), Some((40000, 40100))).unwrap();
    let staging = ConnectionTypeConfig {
        name: Some("Staging".into()),
        confirm_execution: true,
        ..Default::default()
    };
    set_personal_type(&fx.user(), "staging", &staging).unwrap();

    for link in ["user.toml", "connection-types.toml"] {
        assert!(is_link(&fx.user().join(link)), "{link}");
    }
    let text = std::fs::read_to_string(&user_toml).unwrap();
    assert!(
        text.contains("# mine") && text.contains("team = \"data\""),
        "{text}"
    );
    assert!(text.contains("port_min = 40000"), "{text}");
    assert_eq!(mode(&user_toml), 0o644, "an existing file keeps its mode");
    assert!(
        std::fs::read_to_string(&types)
            .unwrap()
            .contains("[types.staging]")
    );
    assert_eq!(mode(&types), 0o640);
    assert_eq!(names(&fx.user()), ["connection-types.toml", "user.toml"]);
    assert_eq!(
        names(&fx.base.join("dotfiles")),
        ["connection-types.toml", "user.toml"]
    );
}

#[test]
fn new_files_get_the_mode_of_the_folder_they_live_in() {
    let fx = Fixture::new("writes-new");
    let backend = FakeBackend::default();
    // A connection to copy the form from.
    fx.write("user/connections/orders.toml", ORDERS);
    let config = fx.config();
    let entry = config.connection(&personal("orders")).unwrap();

    let mut draft = ConnectionDraft::from_entry(entry);
    draft.name = "Billing".into();
    let id = add_connection(&config, &draft, None, &backend).unwrap();
    let personal_file = fx.user().join(format!("connections/{}.toml", id.path));
    assert_eq!(mode(&personal_file), 0o600);

    draft.source = Source::Workspace;
    let id = add_connection(&config, &draft, None, &backend).unwrap();
    let team_file = fx
        .repo()
        .join(format!(".spelaion/connections/{}.toml", id.path));
    assert_eq!(mode(&team_file), umask_mode(team_file.parent().unwrap()));

    fx.write("user/tunnels/bastion.toml", BASTION);
    let config = fx.config();
    let entry = config.tunnel_for(Source::User, "bastion").unwrap();
    let mut tunnel = TunnelDraft::from_entry(entry);
    tunnel.name = "Jump".into();
    let name = add_tunnel(&config, &tunnel, &TunnelPasswords::default(), &backend).unwrap();
    assert_eq!(mode(&fx.user().join(format!("tunnels/{name}.toml"))), 0o600);
    tunnel.source = Source::Workspace;
    let name = add_tunnel(&config, &tunnel, &TunnelPasswords::default(), &backend).unwrap();
    let team_tunnel = fx.repo().join(format!(".spelaion/tunnels/{name}.toml"));
    assert_eq!(
        mode(&team_tunnel),
        umask_mode(team_tunnel.parent().unwrap())
    );
    // Adding it again is refused, and the file stays as it was.
    let before = std::fs::read(&team_tunnel).unwrap();
    assert!(add_tunnel(&config, &tunnel, &TunnelPasswords::default(), &backend).is_err());
    assert_eq!(std::fs::read(&team_tunnel).unwrap(), before);

    let vars: BTreeMap<String, String> = [("team".to_string(), "data".to_string())].into();
    set_user_vars(&fx.user(), &vars).unwrap();
    assert_eq!(mode(&fx.user().join("user.toml")), 0o600);
    set_personal_type(&fx.user(), "staging", &ConnectionTypeConfig::default()).unwrap();
    assert_eq!(mode(&fx.user().join("connection-types.toml")), 0o600);
}

#[test]
fn a_copy_and_a_move_keep_the_original_mode() {
    let fx = Fixture::new("writes-copy");
    let backend = FakeBackend::default();
    let path = fx.write("user/connections/orders.toml", ORDERS);
    set_mode(&path, 0o640);
    let mut trust = fx.untrusted();

    let copy =
        duplicate_connection(&fx.config(), &mut trust, &personal("orders"), &backend).unwrap();
    assert_eq!(
        mode(&fx.user().join(format!("connections/{}.toml", copy.id.path))),
        0o640
    );
    let moved = move_connection(
        &fx.config(),
        &mut trust,
        &personal("orders"),
        "eu",
        &backend,
    )
    .unwrap();
    assert_eq!(
        mode(
            &fx.user()
                .join(format!("connections/{}.toml", moved.id.path))
        ),
        0o640
    );
}

#[test]
fn the_trust_store_keeps_its_mode_and_is_replaced_atomically() {
    let fx = Fixture::new("writes-trust");
    fx.write("repo/.spelaion/connections/orders.toml", ORDERS);
    let store = fx.user().join("trusted-workspaces.toml");

    // New: owner-only, whatever the umask.
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    trust.trust(&fx.config()).unwrap();
    assert_eq!(mode(&store), 0o600);

    // Saved again: the mode the user gave it stays, and the old file is replaced by a new
    // one (a rename), never truncated and written in place.
    set_mode(&store, 0o640);
    let inode = std::fs::metadata(&store).unwrap().ino();
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    trust.trust(&fx.config()).unwrap();
    assert_eq!(mode(&store), 0o640);
    assert_ne!(std::fs::metadata(&store).unwrap().ino(), inode);
    assert_eq!(names(&fx.user()), ["trusted-workspaces.toml"]);

    // Through a link: the link stays, its target gets the new store with its mode.
    let target = fx.base.join("dotfiles/trusted-workspaces.toml");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::rename(&store, &target).unwrap();
    set_mode(&target, 0o600);
    symlink(&target, &store).unwrap();
    let mut trust = TrustStore::load(&fx.user()).unwrap();
    trust.revoke(&fx.repo()).unwrap();
    assert!(is_link(&store));
    assert_eq!(mode(&target), 0o600);
    assert!(!TrustStore::load(&fx.user()).unwrap().is_trusted(&fx.repo()));
    assert_eq!(names(target.parent().unwrap()), ["trusted-workspaces.toml"]);
}

#[test]
fn concurrent_edits_of_one_file_never_share_a_temporary_file() {
    let fx = Fixture::new("writes-race");
    let path = fx.write("user/connections/orders.toml", ORDERS);
    std::thread::scope(|scope| {
        for writer in 0..8 {
            let path = &path;
            scope.spawn(move || {
                for round in 0..25 {
                    set_value(path, &["name"], format!("Orders {writer}-{round}")).unwrap();
                }
            });
        }
    });
    let text = fx.read("user/connections/orders.toml");
    assert!(text.contains("name = \"Orders "), "{text}");
    assert!(text.contains("# The team's orders."), "{text}");
    assert_eq!(names(path.parent().unwrap()), ["orders.toml"]);
}
