//! Kubernetes tunnels against a real cluster (ADR-015). Needs
//! `SPELUNK_TEST_K8S=<kubeconfig>` with `testing/k8s/postgres.yaml` applied (see the README
//! of `testing/k8s`); without it the tests are skipped.

use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use spelunk_config::{
    ConnectionId, ResolvedKubernetes, Resolver, Source, SystemBackend, TrustStore, load,
};
use spelunk_core::kube::{KubeError, KubeTunnel};
use spelunk_core::{
    CoreEvent, Engines, OpenError, OpenOptions, ResultStore, TunnelPool, open_connection,
};
use spelunk_driver_api::Secret;
use spelunk_ssh::{KnownHosts, NonInteractive, TunnelState};

fn kubeconfig() -> Option<PathBuf> {
    match std::env::var("SPELUNK_TEST_K8S") {
        Ok(path) => Some(PathBuf::from(path)),
        Err(_) => {
            eprintln!("SPELUNK_TEST_K8S not set: test skipped");
            None
        }
    }
}

fn spec(kubeconfig: &std::path::Path, resource: &str) -> ResolvedKubernetes {
    ResolvedKubernetes {
        context: None,
        namespace: Some("spelunk-test".into()),
        resource: resource.into(),
        kubeconfig: Some(kubeconfig.to_path_buf()),
        kubectl: "kubectl".into(),
    }
}

/// A personal connection to the cluster's PostgreSQL through a Kubernetes tunnel, opened
/// from its files like the app does; two of them share one `kubectl port-forward`.
#[tokio::test]
async fn connections_reach_a_database_inside_the_cluster() {
    let Some(kubeconfig) = kubeconfig() else {
        return;
    };
    let base = std::env::temp_dir().join(format!("spelunk-kube-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let user = base.join("user");
    std::fs::create_dir_all(user.join("tunnels")).unwrap();
    std::fs::create_dir_all(user.join("connections")).unwrap();
    std::fs::write(
        user.join("tunnels/cluster.toml"),
        format!(
            "schema_version = 1\n[kubernetes]\nnamespace = \"spelunk-test\"\nresource = \"svc/postgres\"\nkubeconfig = {:?}\n",
            kubeconfig.display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(
        user.join("connections/in-cluster.toml"),
        "schema_version = 1\nname = \"In cluster\"\nengine = \"postgres\"\nhost = \"postgres\"\nport = 5432\n\
         database = \"spelunk\"\nuser = \"spelunk\"\npassword = { secret = \"prompt\" }\ntunnel = \"cluster\"\n\
         [tls]\nmode = \"disable\"\n",
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
                    user_file: None,
                    app_file: user.join("known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: Some(Secret::new("spelunk")),
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

    spelunk_core::kube::check(&spec(&kubeconfig, "svc/postgres"))
        .await
        .unwrap();
    let err = spelunk_core::kube::check(&spec(&kubeconfig, "svc/no-such-service"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not found"), "{err}");

    let resources = spelunk_core::kube::resources(&spec(&kubeconfig, "svc/postgres"))
        .await
        .unwrap();
    assert!(
        resources.contains(&"svc/postgres".to_string()),
        "{resources:?}"
    );
    assert!(
        resources.contains(&"deployment/postgres".to_string()),
        "{resources:?}"
    );
    let namespaces = spelunk_core::kube::namespaces(&spec(&kubeconfig, "svc/postgres"))
        .await
        .unwrap();
    assert!(
        namespaces.contains(&"spelunk-test".to_string()),
        "{namespaces:?}"
    );
    assert_eq!(
        spelunk_core::kube::contexts(&spec(&kubeconfig, "svc/postgres"))
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
    let _ = OpenError::Kube(KubeError::Timeout).to_string();
}
