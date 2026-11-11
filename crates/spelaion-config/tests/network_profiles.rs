//! Kubernetes network profiles (ADR-050): a tunnel file with a `[kubernetes]` table, shared by
//! the connections that name it. Each setting comes from the profile or the connection, never
//! both; repository profiles are pinned by trust like any tunnel file (ADR-022), so the tests
//! below also try to get around that.

mod common;

use common::{FakeBackend, Fixture};
use spelaion_config::{
    ConnectionDraft, ConnectionId, KubernetesConfig, KubernetesDraft, KubernetesProfileDraft,
    PasswordChoice, ResolveError, Resolver, Source, TrustStore, TunnelDraft, TunnelDraftOptions,
    TunnelPasswords, add_tunnel, update_tunnel,
};

fn repo_id(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::Workspace,
        path: path.into(),
    }
}

fn user_id(path: &str) -> ConnectionId {
    ConnectionId {
        source: Source::User,
        path: path.into(),
    }
}

const PROFILE: &str = "schema_version = 1\n\n[kubernetes]\ncontext = \"kind-test\"\nnamespace = \"data\"\nkubeconfig = \"k8s/kubeconfig\"\nkubectl = \"/opt/bin/kubectl\"\n";

fn connection(extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"postgres\"\nport = 5432\ndatabase = \"app\"\nuser = \"app\"\ntunnel = \"cluster\"\n{extra}"
    )
}

fn resolve(
    fx: &Fixture,
    trust: &TrustStore,
    id: &ConnectionId,
) -> Result<spelaion_config::ResolvedConnection, ResolveError> {
    let config = fx.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    Resolver::new(&config, trust, &FakeBackend::default()).resolve(id)
}

fn message(result: Result<spelaion_config::ResolvedConnection, ResolveError>) -> String {
    match result {
        Ok(resolved) => panic!("resolved: {:?}", resolved.kubernetes),
        Err(e) => e.to_string(),
    }
}

#[test]
fn connections_share_a_profile_and_add_what_it_leaves_out() {
    let fx = Fixture::new("k8s-profile-merge");
    fx.write("repo/.spelaion/tunnels/cluster.toml", PROFILE);
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &connection("[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    fx.write(
        "repo/.spelaion/connections/billing.toml",
        &connection("[kubernetes]\nresource = \"deployment/billing\"\n"),
    );
    let trust = fx.trusted();
    let orders = resolve(&fx, &trust, &repo_id("orders")).unwrap();
    assert!(
        orders.tunnel.is_none(),
        "a Kubernetes profile is no SSH tunnel"
    );
    let k = orders.kubernetes.expect("through Kubernetes");
    assert_eq!(k.context.as_deref(), Some("kind-test"));
    assert_eq!(k.namespace.as_deref(), Some("data"));
    assert_eq!(k.resource, "svc/orders");
    assert_eq!(k.kubectl, "/opt/bin/kubectl");
    // The profile's relative kubeconfig is the repository's, where the profile lives.
    assert_eq!(k.kubeconfig, Some(fx.repo().join("k8s/kubeconfig")));
    // Through Kubernetes the host only names the server for TLS.
    assert_eq!(orders.params.host, "localhost");
    let billing = resolve(&fx, &trust, &repo_id("billing")).unwrap();
    assert_eq!(billing.kubernetes.unwrap().resource, "deployment/billing");
}

#[test]
fn a_profile_may_name_the_resource_and_connections_only_point_to_it() {
    let fx = Fixture::new("k8s-profile-resource");
    fx.write(
        "user/tunnels/cluster.toml",
        "schema_version = 1\n[kubernetes]\nnamespace = \"data\"\nresource = \"svc/orders\"\n",
    );
    fx.write("user/connections/reader.toml", &connection(""));
    fx.write(
        "user/connections/other-ns.toml",
        &connection("host = \"orders.internal\"\n"),
    );
    let trust = fx.untrusted();
    let reader = resolve(&fx, &trust, &user_id("reader")).unwrap();
    let k = reader.kubernetes.unwrap();
    assert_eq!(
        (k.resource.as_str(), k.kubectl.as_str()),
        ("svc/orders", "kubectl")
    );
    assert_eq!(k.kubeconfig, None);
    let named = resolve(&fx, &trust, &user_id("other-ns")).unwrap();
    assert_eq!(named.params.host, "orders.internal");
}

#[test]
fn a_setting_is_never_taken_from_both_places() {
    let fx = Fixture::new("k8s-profile-both");
    fx.write("user/tunnels/cluster.toml", PROFILE);
    let trust = fx.untrusted();
    for (field, value) in [
        ("context", "\"other\""),
        ("namespace", "\"other\""),
        ("kubeconfig", "\"/tmp/other\""),
        ("kubectl", "\"/tmp/evil\""),
    ] {
        fx.write(
            "user/connections/both.toml",
            &connection(&format!(
                "[kubernetes]\nresource = \"svc/orders\"\n{field} = {value}\n"
            )),
        );
        let error = message(resolve(&fx, &trust, &user_id("both")));
        assert!(
            error.contains(&format!("kubernetes.{field}")) && error.contains("one place"),
            "{field}: {error}"
        );
    }
    fx.write(
        "user/tunnels/cluster.toml",
        "schema_version = 1\n[kubernetes]\nresource = \"svc/orders\"\n",
    );
    fx.write(
        "user/connections/both.toml",
        &connection("[kubernetes]\nresource = \"svc/other\"\n"),
    );
    assert!(message(resolve(&fx, &trust, &user_id("both"))).contains("kubernetes.resource"));
}

#[test]
fn the_resource_must_come_from_somewhere() {
    let fx = Fixture::new("k8s-profile-no-resource");
    fx.write("user/tunnels/cluster.toml", PROFILE);
    fx.write("user/connections/none.toml", &connection(""));
    fx.write(
        "user/connections/blank.toml",
        &connection("[kubernetes]\nresource = \"\"\n"),
    );
    let trust = fx.untrusted();
    for id in ["none", "blank"] {
        let error = message(resolve(&fx, &trust, &user_id(id)));
        assert!(error.contains("choose the Kubernetes resource"), "{error}");
    }
}

#[test]
fn ssh_and_kubernetes_still_exclude_each_other() {
    let fx = Fixture::new("k8s-profile-ssh");
    fx.write(
        "user/tunnels/cluster.toml",
        "schema_version = 1\n[[hops]]\nhost = \"bastion\"\nuser = \"ops\"\n",
    );
    fx.write(
        "user/connections/mixed.toml",
        &connection("host = \"db\"\n[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    let error = message(resolve(&fx, &fx.untrusted(), &user_id("mixed")));
    assert!(error.contains("not a Kubernetes one"), "{error}");

    // Without a resource in a connection that has no profile, the file does not load.
    fx.write(
        "user/connections/mixed.toml",
        "schema_version = 1\nengine = \"postgres\"\nport = 5432\ndatabase = \"a\"\nuser = \"a\"\n[kubernetes]\nnamespace = \"x\"\n",
    );
    assert!(!fx.config().problems.is_empty());
    std::fs::remove_file(fx.user().join("connections/mixed.toml")).unwrap();

    // A Kubernetes profile has no SSH settings, hops or proxy.
    for body in [
        "[[hops]]\nhost = \"b\"\nuser = \"u\"\n",
        "[proxy]\ntype = \"socks5\"\nhost = \"p\"\nport = 1080\n",
        "keepalive_interval = 30\n",
        "connect_timeout = 5\n",
    ] {
        fx.write(
            "user/tunnels/cluster.toml",
            &format!("schema_version = 1\n{body}[kubernetes]\nresource = \"svc/orders\"\n"),
        );
        let problems = fx.config().problems;
        assert_eq!(problems.len(), 1, "{body}: {problems:?}");
    }
    fx.write(
        "user/tunnels/cluster.toml",
        "schema_version = 1\n[kubernetes]\nresource = \"svc/Bad Name\"\n",
    );
    assert_eq!(fx.config().problems.len(), 1);
    fx.write(
        "user/tunnels/cluster.toml",
        "schema_version = 1\n[kubernetes]\nkubectl = \"kube\u{202E}ctl\"\n",
    );
    assert_eq!(fx.config().problems.len(), 1, "hidden characters");
}

#[test]
fn local_files_and_fixed_ports_do_not_go_through_kubernetes() {
    let fx = Fixture::new("k8s-profile-files");
    fx.write("user/tunnels/cluster.toml", PROFILE);
    fx.write(
        "user/connections/fixed.toml",
        &connection("local_port = 15432\n[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    fx.write(
        "user/connections/fixed-only.toml",
        &format!(
            "{}local_port = 15432\n",
            connection("").replace(
                "tunnel = \"cluster\"\n",
                "tunnel = \"cluster\"\nhost = \"h\"\n"
            )
        ),
    );
    fx.write(
        "user/connections/file.toml",
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"/srv/a.db\"\ntunnel = \"cluster\"\n",
    );
    let config = fx.config();
    // `local_port` with `[kubernetes]` is refused when loading; the others when resolving.
    assert_eq!(config.problems.len(), 1, "{:?}", config.problems);
    let trust = fx.untrusted();
    let error = message(resolve_loaded(&fx, &trust, &user_id("fixed-only")));
    assert!(error.contains("local_port"), "{error}");
    let error = message(resolve_loaded(&fx, &trust, &user_id("file")));
    assert!(error.contains("local file"), "{error}");
}

fn resolve_loaded(
    fx: &Fixture,
    trust: &TrustStore,
    id: &ConnectionId,
) -> Result<spelaion_config::ResolvedConnection, ResolveError> {
    Resolver::new(&fx.config(), trust, &FakeBackend::default()).resolve(id)
}

/// Trust pins the profile with the rest of the file: a pull that changes what runs, where it
/// points or which cluster it uses is reviewed before anything is resolved.
#[test]
fn a_changed_repository_profile_waits_for_review() {
    let fx = Fixture::new("k8s-profile-trust");
    fx.write("repo/.spelaion/tunnels/cluster.toml", PROFILE);
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &connection("[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    fx.write(
        "user/connections/mine.toml",
        &connection("[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    let trust = fx.trusted();
    resolve(&fx, &trust, &repo_id("orders")).unwrap();
    resolve(&fx, &trust, &user_id("mine")).unwrap();

    for (field, changed) in [
        (
            "kubernetes.kubectl",
            PROFILE.replace("/opt/bin/kubectl", "/tmp/evil"),
        ),
        (
            "kubernetes.kubeconfig",
            PROFILE.replace("k8s/kubeconfig", "k8s/other"),
        ),
        ("kubernetes.context", PROFILE.replace("kind-test", "prod")),
        (
            "kubernetes.namespace",
            PROFILE.replace("\"data\"", "\"prod\""),
        ),
        (
            "kubernetes.resource",
            format!("{PROFILE}resource = \"svc/orders\"\n"),
        ),
        (
            "kubernetes.kubectl",
            PROFILE.replace("kubectl = \"/opt/bin/kubectl\"\n", ""),
        ),
    ] {
        fx.write("repo/.spelaion/tunnels/cluster.toml", &changed);
        for id in [repo_id("orders"), user_id("mine")] {
            match resolve_loaded(&fx, &trust, &id) {
                Err(ResolveError::Changed { change, .. }) => {
                    assert_eq!(change.item, "tunnel:cluster");
                    assert!(
                        change.fields.iter().any(|f| f.field == field),
                        "{field}: {:?}",
                        change.fields
                    );
                }
                other => panic!("{field}: {:?}", other.map(|r| r.kubernetes)),
            }
        }
    }

    // An SSH profile that becomes a Kubernetes one (or the other way round) is a change too.
    fx.write(
        "repo/.spelaion/tunnels/cluster.toml",
        "schema_version = 1\n[[hops]]\nhost = \"bastion\"\nuser = \"ops\"\n",
    );
    fx.write(
        "repo/.spelaion/connections/orders.toml",
        &connection("host = \"db\"\n"),
    );
    let trust = fx.trusted();
    resolve(&fx, &trust, &repo_id("orders")).unwrap();
    fx.write(
        "repo/.spelaion/tunnels/cluster.toml",
        "schema_version = 1\n[kubernetes]\nresource = \"svc/orders\"\nkubectl = \"/tmp/evil\"\n",
    );
    assert!(matches!(
        resolve_loaded(&fx, &trust, &repo_id("orders")),
        Err(ResolveError::Changed { .. })
    ));

    // A new profile in an untrusted repository is not used by a personal connection either.
    let fresh = Fixture::new("k8s-profile-untrusted");
    fresh.write("repo/.spelaion/tunnels/cluster.toml", PROFILE);
    fresh.write(
        "user/connections/mine.toml",
        &connection("[kubernetes]\nresource = \"svc/orders\"\n"),
    );
    assert!(matches!(
        resolve(&fresh, &fresh.untrusted(), &user_id("mine")),
        Err(ResolveError::Untrusted { .. })
    ));
}

/// The editors run `kubectl` to fill their lists: from a repository profile, only as trusted.
#[test]
fn editors_list_with_a_profile_only_as_trusted() {
    let fx = Fixture::new("k8s-profile-editor");
    fx.write("repo/.spelaion/tunnels/cluster.toml", PROFILE);
    let trust = fx.trusted();
    let config = fx.config();
    let backend = FakeBackend::default();
    let resolver = Resolver::new(&config, &trust, &backend);
    let form = KubernetesConfig {
        context: None,
        namespace: None,
        resource: String::new(),
        kubeconfig: None,
        kubectl: None,
    };
    let spec = resolver
        .kubernetes_profile_for_editor(Source::Workspace, "cluster", &form)
        .unwrap();
    assert_eq!(spec.kubectl, "/opt/bin/kubectl");
    assert_eq!(
        spec.resource, "",
        "lists are asked before a resource is chosen"
    );
    // The form cannot replace what the profile sets.
    let evil = KubernetesConfig {
        kubectl: Some("/tmp/evil".into()),
        ..form.clone()
    };
    assert!(
        resolver
            .kubernetes_profile_for_editor(Source::Workspace, "cluster", &evil)
            .is_err()
    );
    resolver
        .check_tunnel_editor_access(Source::Workspace, Some("cluster"))
        .unwrap();

    fx.write(
        "repo/.spelaion/tunnels/cluster.toml",
        &PROFILE.replace("/opt/bin/kubectl", "/tmp/evil"),
    );
    let config = fx.config();
    let resolver = Resolver::new(&config, &trust, &backend);
    assert!(matches!(
        resolver.kubernetes_profile_for_editor(Source::User, "cluster", &form),
        Err(ResolveError::Changed { .. })
    ));
    assert!(matches!(
        resolver.check_tunnel_editor_access(Source::Workspace, Some("cluster")),
        Err(ResolveError::Changed { .. })
    ));
    // A profile that is not a Kubernetes one gives no kubectl to run.
    fx.write(
        "user/tunnels/ssh.toml",
        "schema_version = 1\n[[hops]]\nhost = \"b\"\nuser = \"u\"\n",
    );
    let config = fx.config();
    let resolver = Resolver::new(&config, &trust, &backend);
    assert!(
        resolver
            .kubernetes_profile_for_editor(Source::User, "ssh", &form)
            .is_err()
    );
    // Nothing from an untrusted repository, not even a new profile's form.
    let fresh = Fixture::new("k8s-profile-editor-untrusted");
    let config = fresh.config();
    let untrusted = fresh.untrusted();
    let resolver = Resolver::new(&config, &untrusted, &backend);
    assert!(matches!(
        resolver.check_tunnel_editor_access(Source::Workspace, None),
        Err(ResolveError::Untrusted { .. })
    ));
}

fn k8s_draft(source: Source) -> TunnelDraft {
    TunnelDraft {
        source,
        name: "Cluster".into(),
        hops: Vec::new(),
        // Left over from the SSH part of the form: not written for Kubernetes.
        options: TunnelDraftOptions {
            keepalive_interval: Some(30),
            keepalive_count: None,
            connect_timeout: Some(5),
        },
        proxy: None,
        kubernetes: Some(KubernetesProfileDraft {
            context: "kind-test".into(),
            namespace: " ".into(),
            resource: String::new(),
            kubeconfig: String::new(),
            kubectl: "/opt/bin/kubectl".into(),
        }),
    }
}

#[test]
fn profiles_are_written_from_the_editor_and_edited_in_place() {
    let fx = Fixture::new("k8s-profile-draft");
    let config = fx.config();
    let backend = FakeBackend::default();
    let name = add_tunnel(
        &config,
        &k8s_draft(Source::User),
        &TunnelPasswords::default(),
        &backend,
    )
    .unwrap();
    assert_eq!(name, "cluster");
    let written = fx.read("user/tunnels/cluster.toml");
    assert!(written.contains("[kubernetes]"), "{written}");
    assert!(
        !written.contains("keepalive")
            && !written.contains("connect_timeout")
            && !written.contains("namespace"),
        "{written}"
    );

    // From Kubernetes to SSH and back, by editing; a comment of the user's stays.
    let path = fx.user().join("tunnels/cluster.toml");
    std::fs::write(&path, format!("# ours\n{written}")).unwrap();
    let config = fx.config();
    let entry = config.tunnels.iter().find(|t| t.name == "cluster").unwrap();
    let mut draft = TunnelDraft::from_entry(entry);
    assert_eq!(
        draft.kubernetes.as_ref().unwrap().kubectl,
        "/opt/bin/kubectl"
    );
    draft.kubernetes.as_mut().unwrap().resource = "svc/orders".into();
    update_tunnel(
        &config,
        Source::User,
        "cluster",
        &draft,
        &TunnelPasswords::default(),
        &backend,
    )
    .unwrap();
    let edited = fx.read("user/tunnels/cluster.toml");
    assert!(
        edited.starts_with("# ours") && edited.contains("resource = \"svc/orders\""),
        "{edited}"
    );

    let config = fx.config();
    let mut ssh = draft.clone();
    ssh.kubernetes = None;
    ssh.hops = vec![spelaion_config::HopDraft {
        host: "bastion".into(),
        port: None,
        user: "ops".into(),
        auth: vec![spelaion_config::SshAuth::Agent],
        identity_file: String::new(),
        password: PasswordChoice::None,
    }];
    let file = update_tunnel(
        &config,
        Source::User,
        "cluster",
        &ssh,
        &TunnelPasswords::default(),
        &backend,
    )
    .unwrap();
    assert!(file.kubernetes.is_none() && file.hops.len() == 1);
    assert!(!fx.read("user/tunnels/cluster.toml").contains("kubernetes"));

    // Both at once is refused.
    let mut both = ssh.clone();
    both.kubernetes = k8s_draft(Source::User).kubernetes;
    assert!(both.to_file(&fx.config()).is_err());
}

#[test]
fn the_connection_form_keeps_the_profile_and_writes_only_its_own_fields() {
    let fx = Fixture::new("k8s-profile-form");
    fx.write("user/tunnels/cluster.toml", PROFILE);
    fx.write(
        "user/tunnels/bastion.toml",
        "schema_version = 1\n[[hops]]\nhost = \"b\"\nuser = \"u\"\n",
    );
    let config = fx.config();
    let draft = |tunnel: &str, resource: &str| ConnectionDraft {
        source: Source::User,
        folder: String::new(),
        name: "Orders".into(),
        description: String::new(),
        tags: Vec::new(),
        engine: spelaion_config::Engine::Postgres,
        env: String::new(),
        host: String::new(),
        port: Some(5432),
        database: "app".into(),
        user: "app".into(),
        password: PasswordChoice::Prompt,
        tls_mode: spelaion_config::TlsModeConfig::Disable,
        tls_ca: String::new(),
        tls_cert: String::new(),
        tls_key: String::new(),
        tunnel: tunnel.into(),
        local_port: Some(15432),
        sqlite3: String::new(),
        kubernetes: Some(KubernetesDraft {
            resource: resource.into(),
            ..KubernetesDraft::default()
        }),
        read_only: false,
        keepalive: None,
        connect_timeout: None,
        statement_timeout: None,
        mcp_exposed: false,
        mcp_writes: false,
        session: Default::default(),
    };
    let file = draft("cluster", "svc/orders").to_file(&config).unwrap();
    assert_eq!(file.tunnel.as_deref(), Some("cluster"));
    assert_eq!(file.kubernetes.unwrap().resource, "svc/orders");
    assert_eq!(file.local_port, None, "kubectl chooses its own port");

    // Nothing of its own: no section at all; the profile names the resource.
    let file = draft("cluster", "").to_file(&config).unwrap();
    assert_eq!(file.tunnel.as_deref(), Some("cluster"));
    assert!(file.kubernetes.is_none());

    // An SSH profile with Kubernetes: Kubernetes wins, as before (ADR-015).
    let file = draft("bastion", "svc/orders").to_file(&config).unwrap();
    assert!(file.tunnel.is_none() && file.kubernetes.is_some());
    assert!(
        draft("", "").to_file(&config).is_err(),
        "no resource anywhere"
    );
}
