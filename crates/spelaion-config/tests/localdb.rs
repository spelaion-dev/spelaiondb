//! SQL Server LocalDB (SDB-312, ADR-068): a connection names the instance in place of a host,
//! `(localdb)\MSSQLLocalDB`. It may have no user (the Windows account logs in), and nothing that
//! belongs to a network route loads with it: no tunnel, Kubernetes, port or certificate to
//! verify. The name reaches `SqlLocalDB`'s command line in the plugin, so only plain names load.

use crate::common;

use common::{FakeBackend, Fixture, personal};
use spelaion_config::{Engine, Resolver, check_localdb_name, localdb_instance};

fn problems(fx: &Fixture) -> String {
    fx.config()
        .problems
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

const LOCALDB: &str = "schema_version = 1\nname = \"Local\"\nengine = \"sqlserver\"\n\
                       engine_kind = \"localdb\"\nhost = '(localdb)\\MSSQLLocalDB'\ndatabase = \"app\"\n";

#[test]
fn a_localdb_instance_loads_without_a_user_or_a_port() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("localdb-loads");
    fx.write("user/connections/local.toml", LOCALDB);
    fx.write(
        "user/connections/sql-login.toml",
        &LOCALDB.replace("name = \"Local\"", "name = \"Login\"\nuser = \"app\""),
    );
    assert_eq!(problems(&fx), "");
    let config = fx.config();
    let (trust, backend) = (fx.untrusted(), FakeBackend::default());
    let resolver = Resolver::new(&config, &trust, &backend);
    let resolved = resolver.resolve(&personal("local")).unwrap();
    assert_eq!(resolved.params.host, "(localdb)\\MSSQLLocalDB");
    assert_eq!(resolved.params.user, "");
    assert!(resolved.tunnel.is_none() && resolved.kubernetes.is_none());
    assert_eq!(
        resolver
            .resolve(&personal("sql-login"))
            .unwrap()
            .params
            .user,
        "app"
    );
}

#[test]
fn a_localdb_instance_takes_nothing_of_a_network_route() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("localdb-refused");
    for (name, extra) in [
        ("tunnel", "tunnel = \"bastion\"\n"),
        ("port", "port = 1433\n"),
        ("local-port", "local_port = 15000\n"),
        (
            "kubernetes",
            "[kubernetes]\nresource = \"svc/sql\"\nnamespace = \"db\"\n",
        ),
        ("verify", "[tls]\nmode = \"verify-full\"\n"),
        ("ca", "[tls]\nca = \"~/ca.pem\"\n"),
    ] {
        fx.write(
            &format!("user/connections/{name}.toml"),
            &format!("{LOCALDB}{extra}"),
        );
    }
    let found = problems(&fx);
    for what in [
        "tunnel",
        "Kubernetes",
        "takes no port",
        "local_port",
        "certificate to verify",
    ] {
        assert!(found.contains(what), "{what}: {found}");
    }
    assert_eq!(
        found.matches("reached through its pipe").count(),
        6,
        "{found}"
    );
}

#[test]
fn only_plain_instance_names_load() {
    let _turn = crate::serial::file_turn(module_path!());
    let fx = Fixture::new("localdb-names");
    for (file, name) in [
        ("quote", "a\"b"),
        ("amp", "x & calc"),
        ("dash", "-c"),
        ("empty", ""),
    ] {
        fx.write(
            &format!("user/connections/{file}.toml"),
            &LOCALDB.replace("MSSQLLocalDB", name),
        );
    }
    let found = problems(&fx);
    assert_eq!(
        found.matches("is not a LocalDB instance name").count(),
        4,
        "{found}"
    );
    for ok in ["MSSQLLocalDB", "v11.0", "My App", "app_1-test"] {
        assert!(check_localdb_name(ok).is_ok(), "{ok}");
    }
}

#[test]
fn only_sql_server_hosts_name_an_instance() {
    let _turn = crate::serial::file_turn(module_path!());
    assert_eq!(
        localdb_instance(Engine::Sqlserver, Some("(LocalDB)\\v11.0")),
        Some("v11.0")
    );
    assert_eq!(
        localdb_instance(Engine::Sqlserver, Some("db.internal")),
        None
    );
    assert_eq!(
        localdb_instance(Engine::Postgres, Some("(localdb)\\MSSQLLocalDB")),
        None
    );
    assert_eq!(localdb_instance(Engine::Sqlserver, None), None);
    // A SQL Server connection to a server still needs its user.
    let fx = Fixture::new("localdb-not");
    fx.write(
        "user/connections/server.toml",
        "schema_version = 1\nname = \"S\"\nengine = \"sqlserver\"\nhost = \"db.internal\"\n",
    );
    assert!(problems(&fx).contains("missing: user"), "{}", problems(&fx));
}
