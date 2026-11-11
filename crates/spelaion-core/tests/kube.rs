//! Kubernetes tunnels against a real cluster (ADR-015). Needs
//! `SPELAION_TEST_K8S=<kubeconfig>` with `testing/k8s/postgres.yaml` applied (see the README
//! of `testing/k8s`); without it the tests are skipped.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{
    ConnectionId, ResolvedKubernetes, Resolver, Source, SystemBackend, TrustStore, load,
};
use spelaion_core::kube::{KubeError, KubeTunnel};
use spelaion_core::{
    CoreEvent, Engines, OpenError, OpenOptions, ResultStore, TunnelPool, open_connection,
};
use spelaion_driver_api::Secret;
use spelaion_ssh::{KnownHosts, NonInteractive, TunnelState};

fn kubeconfig() -> Option<PathBuf> {
    match std::env::var("SPELAION_TEST_K8S") {
        Ok(path) => Some(PathBuf::from(path)),
        Err(_) => {
            eprintln!("SPELAION_TEST_K8S not set: test skipped");
            None
        }
    }
}

fn spec(kubeconfig: &std::path::Path, resource: &str) -> ResolvedKubernetes {
    ResolvedKubernetes {
        context: None,
        namespace: Some("spelaion-test".into()),
        resource: resource.into(),
        kubeconfig: Some(kubeconfig.to_path_buf()),
        kubectl: "kubectl".into(),
    }
}

/// A personal connection to the cluster's PostgreSQL through its `[kubernetes]` section,
/// opened from its file like the app does; two of them share one `kubectl port-forward`.
#[tokio::test]
async fn connections_reach_a_database_inside_the_cluster() {
    let Some(kubeconfig) = kubeconfig() else {
        return;
    };
    let base = std::env::temp_dir().join(format!("spelaion-kube-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let user = base.join("user");
    std::fs::create_dir_all(user.join("connections")).unwrap();
    std::fs::write(
        user.join("connections/in-cluster.toml"),
        format!(
            "schema_version = 1\nname = \"In cluster\"\nengine = \"postgres\"\nport = 5432\n\
             database = \"spelaion\"\nuser = \"spelaion\"\npassword = {{ secret = \"prompt\" }}\n\
             [tls]\nmode = \"disable\"\n[kubernetes]\nnamespace = \"spelaion-test\"\nresource = \"svc/postgres\"\nkubeconfig = {:?}\n",
            kubeconfig.display().to_string()
        ),
    )
    .unwrap();
    let config = load(None, &user);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let trust = TrustStore::load(&user).unwrap();
    let id = ConnectionId {
        source: Source::User,
        path: "in-cluster".into(),
    };
    let pool = Arc::new(TunnelPool::new());
    let open = || async {
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&id)
            .unwrap();
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: pool.clone(),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: user.join("known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: Some(Secret::new("spelaion")),
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
    };
    let first = open().await.expect("opening through kubectl port-forward");
    let second = open()
        .await
        .expect("a second connection, same port-forward");
    assert_eq!(pool.active().await, 1);
    for conn in [&first, &second] {
        let events: Vec<CoreEvent> = conn
            .session
            .run_script("select current_database(), inet_server_port()")
            .collect()
            .await;
        assert!(
            matches!(
                events.last(),
                Some(CoreEvent::ScriptFinished { failed: false, .. })
            ),
            "{events:?}"
        );
    }
    let state = first.tunnel_state().expect("goes through a tunnel");
    assert_eq!(*state.borrow(), TunnelState::Up);
    first.close().await;
    assert_eq!(
        *state.borrow(),
        TunnelState::Up,
        "still used by the second one"
    );
    second.close().await;
    assert!(
        matches!(*state.borrow(), TunnelState::Down { .. }),
        "closed with the last one"
    );
    assert_eq!(pool.active().await, 0);

    // The app's exit stops what its pool started, even with connections still open.
    let third = open().await.expect("a new port-forward");
    let state = third.tunnel_state().expect("goes through a tunnel");
    pool.close_all().await;
    assert!(matches!(*state.borrow(), TunnelState::Down { .. }));
    assert_eq!(pool.active().await, 0);
    drop(third);
    let _ = std::fs::remove_dir_all(base);
}

/// What kubectl says is what the user sees; checking finds the resource or explains why not.
#[tokio::test]
async fn kubectl_errors_reach_the_user() {
    let Some(kubeconfig) = kubeconfig() else {
        return;
    };
    let missing = Arc::new(KubeTunnel::new(spec(&kubeconfig, "svc/no-such-service")));
    let err = missing.forward(5432).await.unwrap_err();
    assert!(
        matches!(&err, KubeError::Kubectl(m) if m.contains("not found")),
        "{err}"
    );

    let names = spelaion_core::kube::names(&spec(&kubeconfig, "svc/postgres"), "pod")
        .await
        .unwrap();
    assert!(
        names.iter().any(|n| n.starts_with("postgres-")),
        "{names:?}"
    );
    let ports = spelaion_core::kube::ports(&spec(&kubeconfig, "svc/postgres"))
        .await
        .unwrap();
    assert_eq!(ports.iter().map(|p| p.port).collect::<Vec<_>>(), [5432]);
    let err = spelaion_core::kube::ports(&spec(&kubeconfig, "svc/no-such-service"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");
    let services = spelaion_core::kube::names(&spec(&kubeconfig, "svc/postgres"), "svc")
        .await
        .unwrap();
    assert_eq!(services, ["postgres"]);
    let ports = spelaion_core::kube::ports(&spec(&kubeconfig, "deployment/postgres"))
        .await
        .unwrap();
    assert_eq!(
        (ports[0].port, ports[0].container.as_deref()),
        (5432, Some("postgres"))
    );
    let namespaces = spelaion_core::kube::namespaces(&spec(&kubeconfig, "svc/postgres"))
        .await
        .unwrap();
    assert!(
        namespaces.contains(&"spelaion-test".to_string()),
        "{namespaces:?}"
    );
    assert_eq!(
        spelaion_core::kube::contexts(&spec(&kubeconfig, "svc/postgres"))
            .await
            .unwrap()
            .len(),
        1
    );

    // A port nothing listens on: kubectl forwards, the database connection then fails.
    let tunnel = Arc::new(KubeTunnel::new(spec(&kubeconfig, "svc/postgres")));
    let local = tunnel.forward(5432).await.unwrap();
    assert!(local.ip().is_loopback());
    assert_eq!(
        tunnel.forward(5432).await.unwrap(),
        local,
        "one process per port"
    );
    tunnel.close().await;
    assert!(matches!(
        tunnel.forward(5432).await,
        Err(KubeError::Down(_))
    ));
    // Closed means kubectl has exited: nothing listens on its port any more.
    assert!(tokio::net::TcpStream::connect(local).await.is_err());
    let _ = OpenError::Kube(KubeError::Timeout).to_string();
}

/// Kubernetes network profiles in a trusted repository (ADR-050): connections name their own
/// resource through a profile that leaves it out, or use a profile that names it. The same
/// resolved forward (cluster, namespace and resource) is one `kubectl port-forward`.
#[tokio::test]
async fn connections_share_a_kubernetes_network_profile() {
    let Some(kubeconfig) = kubeconfig() else {
        return;
    };
    let base = std::env::temp_dir().join(format!("spelaion-kube-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let user = base.join("user");
    let repo = base.join("repo");
    let spelaion = repo.join(".spelaion");
    std::fs::create_dir_all(user.join("connections")).unwrap();
    std::fs::create_dir_all(spelaion.join("tunnels")).unwrap();
    std::fs::create_dir_all(spelaion.join("connections")).unwrap();
    // The kubeconfig is each user's: a variable, so the shared file names no local path.
    std::fs::write(
        user.join("user.toml"),
        format!(
            "schema_version = 1\n[vars]\nkubeconfig = {:?}\n",
            kubeconfig.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        spelaion.join("tunnels/test-cluster.toml"),
        "schema_version = 1\n[kubernetes]\nnamespace = \"spelaion-test\"\nkubeconfig = \"${user.kubeconfig}\"\n",
    )
    .unwrap();
    std::fs::write(
        spelaion.join("tunnels/test-postgres.toml"),
        "schema_version = 1\n[kubernetes]\nnamespace = \"spelaion-test\"\nresource = \"svc/postgres\"\nkubeconfig = \"${user.kubeconfig}\"\n",
    )
    .unwrap();
    let connection = |tunnel: &str, extra: &str| {
        format!(
            "schema_version = 1\nengine = \"postgres\"\nport = 5432\ndatabase = \"spelaion\"\nuser = \"spelaion\"\n\
             password = {{ secret = \"prompt\" }}\ntunnel = \"{tunnel}\"\n[tls]\nmode = \"disable\"\n{extra}"
        )
    };
    for (name, tunnel, extra) in [
        (
            "svc-a",
            "test-cluster",
            "[kubernetes]\nresource = \"svc/postgres\"\n",
        ),
        (
            "svc-b",
            "test-cluster",
            "[kubernetes]\nresource = \"svc/postgres\"\n",
        ),
        (
            "deploy",
            "test-cluster",
            "[kubernetes]\nresource = \"deployment/postgres\"\n",
        ),
        ("named", "test-postgres", ""),
    ] {
        std::fs::write(
            spelaion.join(format!("connections/{name}.toml")),
            connection(tunnel, extra),
        )
        .unwrap();
    }
    let config = load(Some(&repo), &user);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let mut trust = TrustStore::load(&user).unwrap();
    trust.trust(&config).unwrap();
    let pool = Arc::new(TunnelPool::new());
    let open = |path: &'static str| {
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: path.into(),
            })
            .unwrap();
        let pool = pool.clone();
        let app_file = user.join("known_hosts");
        async move {
            open_connection(
                resolved,
                &Engines::builtin(),
                OpenOptions {
                    tunnels: pool,
                    known_hosts: KnownHosts {
                        team_file: None,
                        user_file: None,
                        app_file,
                    },
                    interaction: Arc::new(NonInteractive),
                    password: Some(Secret::new("spelaion")),
                    read_only_session: false,
                },
                Arc::new(ResultStore::new()),
            )
            .await
            .expect("opening through the profile")
        }
    };
    let a = open("svc-a").await;
    let b = open("svc-b").await;
    assert_eq!(
        pool.active().await,
        1,
        "same profile and service: one kubectl"
    );
    let deploy = open("deploy").await;
    assert_eq!(pool.active().await, 2, "another resource: another kubectl");
    let named = open("named").await;
    assert_eq!(
        pool.active().await,
        2,
        "the same service through another profile is the same forward"
    );
    for conn in [&a, &b, &deploy, &named] {
        let events: Vec<CoreEvent> = conn
            .session
            .run_script("select current_database()")
            .collect()
            .await;
        assert!(
            matches!(
                events.last(),
                Some(CoreEvent::ScriptFinished { failed: false, .. })
            ),
            "{events:?}"
        );
    }
    for conn in [a, b, deploy, named] {
        conn.close().await;
    }
    assert_eq!(pool.active().await, 0);

    // A pull that points the profile at another kubectl is reviewed before anything runs.
    std::fs::write(
        spelaion.join("tunnels/test-cluster.toml"),
        "schema_version = 1\n[kubernetes]\nnamespace = \"spelaion-test\"\nkubeconfig = \"${user.kubeconfig}\"\nkubectl = \"./evil\"\n",
    )
    .unwrap();
    let config = load(Some(&repo), &user);
    let changed = Resolver::new(&config, &trust, &SystemBackend).resolve(&ConnectionId {
        source: Source::Workspace,
        path: "svc-a".into(),
    });
    assert!(
        matches!(changed, Err(spelaion_config::ResolveError::Changed { .. })),
        "{:?}",
        changed.map(|r| r.kubernetes)
    );
    let _ = std::fs::remove_dir_all(base);
}
