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

/// The stand-in for kubectl (`tests/support/fake_kubectl.rs`), with a folder of its own. These
/// tests need no cluster and always run.
struct FakeKubectl {
    dir: PathBuf,
}

impl FakeKubectl {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-fake-kubectl-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn spec(&self, resource: &str) -> ResolvedKubernetes {
        ResolvedKubernetes {
            context: None,
            namespace: None,
            resource: resource.into(),
            kubeconfig: Some(self.dir.join("kubeconfig")),
            kubectl: env!("CARGO_BIN_EXE_spelaion-test-fake-kubectl").into(),
        }
    }

    /// Lets a `svc/gated` forward listen.
    fn release(&self) {
        std::fs::write(self.dir.join("release"), "").unwrap();
    }
}

impl Drop for FakeKubectl {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Waits until nothing listens on `local` any more; false if something still does after
/// `limit`.
async fn stops_listening(local: std::net::SocketAddr, limit: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    while started.elapsed() < limit {
        if tokio::net::TcpStream::connect(local).await.is_err() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

async fn down_reason(state: &mut tokio::sync::watch::Receiver<TunnelState>) -> String {
    let down = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        state.wait_for(|s| matches!(s, TunnelState::Down { .. })),
    )
    .await
    .expect("the tunnel goes down")
    .expect("the tunnel is still there");
    match &*down {
        TunnelState::Down { reason } => reason.clone(),
        other => panic!("not down: {other:?}"),
    }
}

/// SDB-366: a Kubernetes tunnel is not up before its first kubectl listens.
#[tokio::test]
async fn a_kubernetes_tunnel_is_opening_until_its_first_forward_listens() {
    let fake = FakeKubectl::new("opening");
    let tunnel = Arc::new(KubeTunnel::new(fake.spec("svc/gated")));
    let state = tunnel.state();
    assert_eq!(
        *state.borrow(),
        TunnelState::Opening,
        "no kubectl has run yet"
    );
    let forwarding = tokio::spawn({
        let tunnel = tunnel.clone();
        async move { tunnel.forward(5432).await }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        *state.borrow(),
        TunnelState::Opening,
        "kubectl has not listened yet"
    );
    fake.release();
    let local = tokio::time::timeout(std::time::Duration::from_secs(20), forwarding)
        .await
        .expect("kubectl listens once released")
        .unwrap()
        .unwrap();
    assert!(local.ip().is_loopback());
    assert_eq!(*state.borrow(), TunnelState::Up);
    tunnel.close().await;
}

/// SDB-366: the first forward failing fails the opening with kubectl's words; the tunnel is
/// down, never up.
#[tokio::test]
async fn a_first_forward_that_fails_takes_the_opening_tunnel_down() {
    let fake = FakeKubectl::new("missing");
    let tunnel = Arc::new(KubeTunnel::new(fake.spec("svc/missing")));
    let err = tunnel.forward(5432).await.unwrap_err();
    assert!(
        matches!(&err, KubeError::Kubectl(m) if m.contains("not found")),
        "{err}"
    );
    let state = tunnel.state().borrow().clone();
    assert!(
        matches!(&state, TunnelState::Down { reason } if reason.contains("not found")),
        "{state:?}"
    );
}

/// SDB-366: when one kubectl ends, the tunnel goes down with its words and the other forwards
/// are stopped (nothing listens on their ports); closing it later keeps that reason.
#[tokio::test]
async fn when_one_forward_ends_the_others_are_stopped() {
    let fake = FakeKubectl::new("ends");
    let tunnel = Arc::new(KubeTunnel::new(fake.spec("svc/postgres")));
    let kept = tunnel.forward(5432).await.unwrap();
    let ending = tunnel.forward(6379).await.unwrap();
    assert!(tokio::net::TcpStream::connect(kept).await.is_ok());
    let mut state = tunnel.state();
    // The stand-in ends this forward on its first connection.
    let _ = tokio::net::TcpStream::connect(ending).await;
    assert_eq!(
        down_reason(&mut state).await,
        "error: lost connection to pod"
    );
    assert!(
        stops_listening(kept, std::time::Duration::from_secs(10)).await,
        "the other kubectl was left running"
    );
    assert!(matches!(
        tunnel.forward(5432).await,
        Err(KubeError::Down(_))
    ));
    tunnel.close().await;
    assert_eq!(
        *tunnel.state().borrow(),
        TunnelState::Down {
            reason: "error: lost connection to pod".into()
        },
        "closing keeps why it went down"
    );
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
    assert_eq!(
        *state.borrow(),
        TunnelState::Closed,
        "closed with the last one"
    );
    assert_eq!(pool.active().await, 0);

    // The app's exit stops what its pool started, even with connections still open.
    let third = open().await.expect("a new port-forward");
    let state = third.tunnel_state().expect("goes through a tunnel");
    pool.close_all().await;
    assert_eq!(*state.borrow(), TunnelState::Closed);
    assert_eq!(pool.active().await, 0);
    drop(third);
    let _ = std::fs::remove_dir_all(base);
}

/// Stops the program listening on local `port` (a kubectl), as if it had died.
fn kill_listener(port: u16) {
    #[cfg(windows)]
    let status = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!(
                "Stop-Process -Force -Id (Get-NetTCPConnection -State Listen -LocalPort {port}).OwningProcess"
            ),
        ])
        .status()
        .unwrap();
    #[cfg(not(windows))]
    let status = std::process::Command::new("sh")
        .args(["-c", &format!("kill $(lsof -t -iTCP:{port} -sTCP:LISTEN)")])
        .status()
        .unwrap();
    assert!(
        status.success(),
        "could not stop the kubectl on port {port}"
    );
}

/// SDB-366 with a real kubectl: the tunnel is opening until it listens; when one kubectl dies,
/// the tunnel goes down and the other one is stopped; closing keeps the reason.
#[tokio::test]
async fn with_a_cluster_one_forward_ending_stops_the_others() {
    let Some(kubeconfig) = kubeconfig() else {
        return;
    };
    let tunnel = Arc::new(KubeTunnel::new(spec(&kubeconfig, "deployment/postgres")));
    assert_eq!(*tunnel.state().borrow(), TunnelState::Opening);
    let kept = tunnel.forward(5432).await.unwrap();
    assert_eq!(*tunnel.state().borrow(), TunnelState::Up);
    // Nothing listens on this port in the pod, but kubectl forwards it.
    let ending = tunnel.forward(9999).await.unwrap();
    assert!(tokio::net::TcpStream::connect(kept).await.is_ok());
    let mut state = tunnel.state();
    kill_listener(ending.port());
    let reason = down_reason(&mut state).await;
    assert!(
        stops_listening(kept, std::time::Duration::from_secs(30)).await,
        "the other kubectl was left running"
    );
    tunnel.close().await;
    assert_eq!(*tunnel.state().borrow(), TunnelState::Down { reason });
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
