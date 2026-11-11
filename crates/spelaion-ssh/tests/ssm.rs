//! AWS SSM tunnels (ADR-050) with stand-ins for the AWS CLI and its Session Manager plugin
//! (`tests/support/fake_aws.rs`): they record the arguments and environment they get and
//! forward the port as the real ones do. No AWS account is used: a real one is tested by hand.
//!
//! The tests with hops also need the bastions of `testing/docker` (`SPELAION_TEST_SSH`).

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdListener, TcpStream as StdStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use spelaion_driver_api::Secret;
use spelaion_ssh::{
    AuthMethod, HopSpec, KnownHosts, NonInteractive, SshError, SsmFailure, SsmSpec, Target, Tunnel,
    TunnelSpec, TunnelState,
};

/// A folder with the stand-ins as `aws` and `session-manager-plugin`, as the installers leave
/// them; they write what they were given here.
struct FakeAws(PathBuf);

impl FakeAws {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("spelaion-ssm-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fake = PathBuf::from(env!("CARGO_BIN_EXE_spelaion-test-fake-aws"));
        let ext = if cfg!(windows) { ".exe" } else { "" };
        for name in ["aws", "session-manager-plugin"] {
            std::fs::copy(&fake, dir.join(format!("{name}{ext}"))).unwrap();
        }
        Self(dir)
    }

    fn aws(&self) -> String {
        let ext = if cfg!(windows) { ".exe" } else { "" };
        self.0.join(format!("aws{ext}")).display().to_string()
    }

    fn spec(&self, target: &str) -> SsmSpec {
        SsmSpec {
            target: target.into(),
            region: Some("eu-west-1".into()),
            profile: Some("prod".into()),
            aws: Some(self.aws()),
        }
    }

    fn args(&self) -> Vec<String> {
        read(&self.0.join("aws-args.txt"))
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn env(&self) -> String {
        read(&self.0.join("aws-env.txt"))
    }

    /// The local port the plugin was told to listen on.
    fn plugin_port(&self) -> u16 {
        let args = self.args();
        let parameters = args
            .iter()
            .find_map(|a| a.strip_prefix("--parameters="))
            .expect("parameters");
        parameters
            .split(',')
            .find_map(|p| p.strip_prefix("localPortNumber="))
            .expect("local port")
            .parse()
            .unwrap()
    }
}

impl Drop for FakeAws {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// A server on 127.0.0.1 that answers every line with the same line, upper-cased.
fn echo_server() -> SocketAddr {
    let listener = StdListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let reply = String::from_utf8_lossy(&buf[..n]).to_uppercase();
                    if stream.write_all(reply.as_bytes()).is_err() {
                        break;
                    }
                }
            });
        }
    });
    addr
}

fn round_trip(addr: SocketAddr, text: &str) -> String {
    let mut stream = StdStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(text.as_bytes()).unwrap();
    let mut buf = vec![0u8; text.len()];
    stream.read_exact(&mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

fn known() -> KnownHosts {
    KnownHosts {
        team_file: None,
        user_file: None,
        app_file: std::env::temp_dir().join(format!("spelaion-ssm-kh-{}", std::process::id())),
    }
}

fn ssm_only(fake: &FakeAws, target: &str) -> TunnelSpec {
    TunnelSpec {
        ssm: Some(fake.spec(target)),
        ..Default::default()
    }
}

/// SSM alone reaches a port on the managed node: the CLI gets exactly the arguments ADR-050
/// says, each option as `--name=value`, and the tunnel's forward carries the data.
#[tokio::test]
async fn ssm_alone_forwards_to_a_port_on_the_node() {
    let fake = FakeAws::new("node");
    let echo = echo_server();
    let tunnel = Tunnel::connect(
        &ssm_only(&fake, "i-0123456789abcdef0"),
        &known(),
        std::sync::Arc::new(NonInteractive),
    )
    .await
    .expect("the tools are found");
    let local = tunnel
        .forward(Target {
            host: "localhost".into(),
            port: echo.port(),
        })
        .await
        .expect("an SSM session to the node");
    assert!(local.ip().is_loopback());
    let plugin_port = fake.plugin_port();
    assert_eq!(
        fake.args(),
        [
            "ssm".to_string(),
            "start-session".into(),
            "--target=i-0123456789abcdef0".into(),
            "--document-name=AWS-StartPortForwardingSession".into(),
            format!(
                "--parameters=portNumber={},localPortNumber={plugin_port}",
                echo.port()
            ),
            "--region=eu-west-1".into(),
            "--profile=prod".into(),
        ]
    );
    let answer = tokio::task::spawn_blocking(move || round_trip(local, "through ssm"))
        .await
        .unwrap();
    assert_eq!(answer, "THROUGH SSM");
    // The same target again is the same session.
    assert_eq!(
        tunnel
            .forward(Target {
                host: "localhost".into(),
                port: echo.port(),
            })
            .await
            .unwrap(),
        local
    );

    // Closing stops the CLI and the plugin it started: nothing listens there any more.
    tunnel.close().await;
    assert!(
        StdStream::connect(("127.0.0.1", plugin_port)).is_err(),
        "the plugin outlived the tunnel"
    );
}

/// Another host is reached through the node (RDS, ElastiCache): its name goes to the CLI in the
/// remote-host document's parameters, and a dropped tunnel stops the plugin too.
#[tokio::test]
async fn ssm_reaches_another_host_through_the_node() {
    let fake = FakeAws::new("remote");
    let tunnel = Tunnel::connect(
        &ssm_only(&fake, "mi-0123456789abcdef0"),
        &known(),
        std::sync::Arc::new(NonInteractive),
    )
    .await
    .unwrap();
    tunnel
        .forward(Target {
            host: "orders.cluster-abc.eu-west-1.rds.amazonaws.com".into(),
            port: 5432,
        })
        .await
        .unwrap();
    let args = fake.args();
    assert!(
        args.contains(&"--document-name=AWS-StartPortForwardingSessionToRemoteHost".to_string()),
        "{args:?}"
    );
    let plugin_port = fake.plugin_port();
    assert!(args.contains(&format!(
        "--parameters=host=orders.cluster-abc.eu-west-1.rds.amazonaws.com,portNumber=5432,localPortNumber={plugin_port}"
    )));
    assert!(StdStream::connect(("127.0.0.1", plugin_port)).is_ok());
    drop(tunnel);
    let mut stopped = false;
    for _ in 0..50 {
        if StdStream::connect(("127.0.0.1", plugin_port)).is_err() {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped, "the plugin outlived a dropped tunnel");
}

/// What the CLI says when AWS refuses is what the user sees; the tunnel stays usable for
/// nothing else and is never restarted.
#[tokio::test]
async fn aws_errors_reach_the_user() {
    let fake = FakeAws::new("refused");
    let tunnel = Tunnel::connect(
        &ssm_only(&fake, "i-unreachable"),
        &known(),
        std::sync::Arc::new(NonInteractive),
    )
    .await
    .unwrap();
    let error = tunnel
        .forward(Target {
            host: "localhost".into(),
            port: 5432,
        })
        .await
        .unwrap_err();
    match &error {
        SshError::Ssm {
            failure: SsmFailure::Exited,
            message,
        } => assert!(message.contains("TargetNotConnected"), "{message}"),
        other => panic!("{other}"),
    }
    let probe = tunnel
        .probe(
            &Target {
                host: "localhost".into(),
                port: 5432,
            },
            Duration::from_secs(5),
        )
        .await;
    assert!(probe.is_err());
}

/// Missing tools are named before anything runs; values that could change the command are
/// refused before the CLI starts.
#[tokio::test]
async fn missing_tools_and_bad_values_stop_before_running() {
    let fake = FakeAws::new("missing");
    let mut spec = fake.spec("i-0123456789abcdef0");
    spec.aws = Some(fake.0.join("nowhere").join("aws").display().to_string());
    let error = Tunnel::connect(
        &TunnelSpec {
            ssm: Some(spec),
            ..Default::default()
        },
        &known(),
        std::sync::Arc::new(NonInteractive),
    )
    .await
    .err()
    .expect("refused");
    assert!(
        matches!(
            error,
            SshError::Ssm {
                failure: SsmFailure::MissingCli,
                ..
            }
        ),
        "{error}"
    );

    let bad = TunnelSpec {
        ssm: Some(fake.spec("i-1 --endpoint-url=http://evil")),
        ..Default::default()
    };
    let error = Tunnel::connect(&bad, &known(), std::sync::Arc::new(NonInteractive))
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(
            error,
            SshError::Ssm {
                failure: SsmFailure::Invalid,
                ..
            }
        ),
        "{error}"
    );
    assert!(fake.args().is_empty(), "the CLI never ran");

    // A host that could end the parameters list never reaches the CLI either.
    let tunnel = Tunnel::connect(
        &ssm_only(&fake, "i-0123456789abcdef0"),
        &known(),
        std::sync::Arc::new(NonInteractive),
    )
    .await
    .unwrap();
    let error = tunnel
        .forward(Target {
            host: "db,localPortNumber=22".into(),
            port: 5432,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            SshError::Ssm {
                failure: SsmFailure::Invalid,
                ..
            }
        ),
        "{error}"
    );
    assert!(fake.args().is_empty(), "the CLI never ran");

    // A proxy and SSM both before the first hop are refused.
    let both = TunnelSpec {
        ssm: Some(fake.spec("i-0123456789abcdef0")),
        proxy: Some(spelaion_ssh::ProxySpec {
            kind: spelaion_ssh::ProxyKind::Socks5,
            host: "proxy".into(),
            port: 1080,
            user: None,
            password: None,
        }),
        ..Default::default()
    };
    assert!(
        Tunnel::connect(&both, &known(), std::sync::Arc::new(NonInteractive))
            .await
            .is_err()
    );
}

fn bastion() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

fn key(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../testing/docker/keys")
        .join(name)
}

struct AcceptHosts;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

/// SSH over SSM: the session reaches the first hop's SSH port on the node, the hops go on from
/// there to PostgreSQL. No secret of the tunnel reaches the CLI's arguments or environment.
#[tokio::test]
async fn hops_go_on_from_an_ssm_session() {
    let Some((host, port)) = bastion() else {
        return;
    };
    let fake = FakeAws::new("hops");
    let dir = std::env::temp_dir().join(format!("spelaion-ssm-hops-kh-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hop = |host: &str, port: u16| HopSpec {
        host: host.into(),
        port,
        user: "spelaion".into(),
        auth: vec![AuthMethod::Key],
        identity_file: Some(key("id_ed25519")),
        // Never needed (the key is), and never given to the CLI.
        password: Some(Secret::new("hop-secret-7f3a9c")),
        command: None,
    };
    let spec = TunnelSpec {
        ssm: Some(fake.spec("i-0123456789abcdef0")),
        hops: vec![hop(&host, port), hop("bastion2", 22)],
        ..Default::default()
    };
    let tunnel = Tunnel::connect(
        &spec,
        &KnownHosts {
            team_file: None,
            user_file: None,
            app_file: dir.join("known_hosts"),
        },
        std::sync::Arc::new(AcceptHosts),
    )
    .await
    .expect("SSH through the SSM session");
    // The first hop is reached from the node: here, the bastion's own address.
    let args = fake.args();
    assert!(
        args.contains(&"--document-name=AWS-StartPortForwardingSession".to_string())
            || args.contains(
                &"--document-name=AWS-StartPortForwardingSessionToRemoteHost".to_string()
            ),
        "{args:?}"
    );
    assert!(
        args.iter()
            .any(|a| a.contains(&format!("portNumber={port}"))),
        "{args:?}"
    );
    // The host key was checked against the hop's own name, not the local end.
    let known_hosts = read(&dir.join("known_hosts"));
    assert!(
        known_hosts.contains(&format!("[{host}]:{port}")) || known_hosts.contains(&host),
        "{known_hosts}"
    );
    tunnel
        .forward(Target {
            host: "postgres".into(),
            port: 5432,
        })
        .await
        .unwrap();
    let local = tunnel.local_addr();
    // PostgreSQL answers an SSLRequest with one byte: S or N.
    let answer = tokio::task::spawn_blocking(move || {
        let mut stream = StdStream::connect(local).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .write_all(&[0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f])
            .unwrap();
        let mut byte = [0u8; 1];
        stream.read_exact(&mut byte).unwrap();
        byte[0]
    })
    .await
    .unwrap();
    assert!(answer == b'S' || answer == b'N', "{answer}");
    for text in [fake.args().join("\n"), fake.env()] {
        assert!(
            !text.contains("hop-secret-7f3a9c"),
            "a secret reached the CLI"
        );
    }
    let plugin_port = fake.plugin_port();
    let state = tunnel.state();
    tunnel.close().await;
    assert!(matches!(*state.borrow(), TunnelState::Down { .. }));
    assert!(StdStream::connect(("127.0.0.1", plugin_port)).is_err());
    let _ = std::fs::remove_dir_all(dir);
}
