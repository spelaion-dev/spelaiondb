//! The unverified-TLS warning (SDB-395) from real configuration files: the connection as the
//! explorer reads it (its file and its network profile, never resolved) and as the connection
//! test resolves it. A network profile with SSH hops, AWS SSM or Kubernetes makes the warning
//! calmer; a proxy alone does not; a host that is this machine never warns; files never do.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{TlsRisk, TlsRoute, TlsWarning, connection_tls_warning, file_tls_warning};

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "spelaion-tls-warning-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("user/connections")).unwrap();
        std::fs::create_dir_all(base.join("user/tunnels")).unwrap();
        Self { base }
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.base.join(path), content).unwrap();
    }

    /// The warning for each personal connection, as the explorer and as the test read it.
    fn warnings(&self, id: &str) -> (Option<TlsWarning>, Option<TlsWarning>) {
        let config = load(None, &self.base.join("user"));
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let entry = config
            .connections
            .iter()
            .find(|c| c.id.path == id)
            .unwrap_or_else(|| panic!("{id} is not loaded"));
        let as_written = file_tls_warning(&config, entry);
        let trust = TrustStore::load(&self.base.join("user")).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::User,
                path: id.into(),
            })
            .expect("resolving");
        (as_written, connection_tls_warning(&resolved))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn connection(engine: &str, host: &str, extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"{engine}\"\nhost = \"{host}\"\nport = 5432\ndatabase = \"d\"\nuser = \"u\"\n{extra}"
    )
}

#[test]
fn the_warning_follows_the_mode_the_host_and_the_route() {
    let fx = Fixture::new();
    fx.write(
        "user/tunnels/bastion.toml",
        "schema_version = 1\n[[hops]]\nhost = \"bastion.example.com\"\nuser = \"u\"\n",
    );
    fx.write(
        "user/tunnels/socks.toml",
        "schema_version = 1\n[proxy]\ntype = \"socks5\"\nhost = \"p.example.com\"\nport = 1080\n",
    );
    fx.write(
        "user/tunnels/aws.toml",
        "schema_version = 1\n[ssm]\ntarget = \"i-1\"\n",
    );
    // A connection's id, its file, and the warning expected (its risk, mode and route).
    type Expected = Option<(TlsRisk, &'static str, TlsRoute)>;
    let cases: Vec<(&str, String, Expected)> = vec![
        // The default mode, `prefer`, on a server on the network.
        (
            "default",
            connection("postgres", "db.example.com", ""),
            Some((TlsRisk::Unverified, "prefer", TlsRoute::Direct)),
        ),
        (
            "require",
            connection("mysql", "10.1.2.3", "[tls]\nmode = \"require\"\n"),
            Some((TlsRisk::Unverified, "require", TlsRoute::Direct)),
        ),
        (
            "disable",
            connection("sqlserver", "db.example.com", "[tls]\nmode = \"disable\"\n"),
            Some((TlsRisk::Unencrypted, "disable", TlsRoute::Direct)),
        ),
        (
            "verified",
            connection(
                "postgres",
                "db.example.com",
                "[tls]\nmode = \"verify-full\"\n",
            ),
            None,
        ),
        (
            "verified-ca",
            connection("mariadb", "db.example.com", "[tls]\nmode = \"verify-ca\"\n"),
            None,
        ),
        ("local", connection("postgres", "localhost", ""), None),
        ("loopback", connection("clickhouse", "127.0.0.1", ""), None),
        // Through SSH hops: calmer; the host is the database as seen from the last hop.
        (
            "tunneled",
            connection("postgres", "db.internal", "tunnel = \"bastion\"\n"),
            Some((TlsRisk::Unverified, "prefer", TlsRoute::Tunneled)),
        ),
        // The database on the last hop itself: nothing crosses a network unencrypted.
        (
            "on-the-bastion",
            connection("postgres", "localhost", "tunnel = \"bastion\"\n"),
            None,
        ),
        (
            "ssm",
            connection("postgres", "db.internal", "tunnel = \"aws\"\n"),
            Some((TlsRisk::Unverified, "prefer", TlsRoute::Tunneled)),
        ),
        // A proxy alone encrypts nothing: as direct.
        (
            "proxied",
            connection("postgres", "db.example.com", "tunnel = \"socks\"\n"),
            Some((TlsRisk::Unverified, "prefer", TlsRoute::Direct)),
        ),
    ];
    for (id, content, _) in &cases {
        fx.write(&format!("user/connections/{id}.toml"), content);
    }
    fx.write(
        "user/connections/file.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/tmp/x.db\"\n",
    );
    for (id, _, expected) in &cases {
        let (as_written, resolved) = fx.warnings(id);
        assert_eq!(
            as_written, resolved,
            "{id}: the explorer and the test agree"
        );
        assert_eq!(
            resolved.as_ref().map(|w| (w.risk, w.mode, w.route)),
            *expected,
            "{id}"
        );
        if let Some(warning) = resolved {
            assert!(!warning.host.is_empty(), "{id}: it names the host");
        }
    }
    assert_eq!(fx.warnings("file"), (None, None), "a file has no TLS");
}
