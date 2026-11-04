//! Kubernetes tunnels (ADR-015): `kubectl port-forward` to a resource of a cluster, using the
//! user's `kubectl` and kubeconfig (so their authentication plugins just work). One process
//! per remote port, started on the first connection that needs it and stopped with the
//! tunnel; if one exits, the tunnel goes down with kubectl's message and is never restarted
//! on its own, like an SSH tunnel.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use spelaion_config::ResolvedKubernetes;
use spelaion_ssh::TunnelState;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, oneshot, watch};
use tokio::task::JoinHandle;

/// How long `kubectl port-forward` may take to report its local port.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a listing `kubectl` waits for the cluster (`--request-timeout`).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum KubeError {
    #[error("could not run {kubectl}: {message} (is kubectl installed and on the PATH?)")]
    Spawn { kubectl: String, message: String },
    #[error("kubectl: {0}")]
    Kubectl(String),
    #[error("kubectl did not answer within {} seconds", START_TIMEOUT.as_secs())]
    Timeout,
    #[error("the Kubernetes tunnel is down: {0}")]
    Down(String),
}

/// A Kubernetes tunnel: the cluster resource, and the port-forwards opened through it.
pub struct KubeTunnel {
    spec: ResolvedKubernetes,
    forwards: Mutex<HashMap<u16, Forward>>,
    state: watch::Sender<TunnelState>,
}

struct Forward {
    local: SocketAddr,
    /// Stops the process (dropping it also does: `kill_on_drop`).
    stop: Option<oneshot::Sender<()>>,
    /// The task that owns the process; it ends once `kubectl` has exited.
    task: Option<JoinHandle<()>>,
}

impl KubeTunnel {
    pub fn new(spec: ResolvedKubernetes) -> Self {
        Self {
            spec,
            forwards: Mutex::new(HashMap::new()),
            state: watch::channel(TunnelState::Up).0,
        }
    }

    pub fn state(&self) -> watch::Receiver<TunnelState> {
        self.state.subscribe()
    }

    /// The local address that reaches `remote_port` of the resource, starting
    /// `kubectl port-forward` the first time.
    pub async fn forward(self: &Arc<Self>, remote_port: u16) -> Result<SocketAddr, KubeError> {
        if let TunnelState::Down { reason } = &*self.state.borrow() {
            return Err(KubeError::Down(reason.clone()));
        }
        let mut forwards = self.forwards.lock().await;
        if let Some(forward) = forwards.get(&remote_port) {
            return Ok(forward.local);
        }
        let forward = self.start(remote_port).await?;
        let local = forward.local;
        forwards.insert(remote_port, forward);
        Ok(local)
    }

    /// Stops every port-forward and waits for each `kubectl` to exit; the tunnel is down from
    /// then on.
    pub async fn close(&self) {
        let forwards: Vec<Forward> = self.forwards.lock().await.drain().map(|(_, f)| f).collect();
        for mut forward in forwards {
            if let Some(stop) = forward.stop.take() {
                let _ = stop.send(());
            }
            if let Some(task) = forward.task.take() {
                let _ = task.await;
            }
        }
        self.state.send_replace(TunnelState::Down {
            reason: "closed".into(),
        });
    }

    async fn start(self: &Arc<Self>, remote_port: u16) -> Result<Forward, KubeError> {
        let mut command = kubectl(&self.spec);
        command
            .args(["port-forward", "--address", "127.0.0.1"])
            .arg(&self.spec.resource)
            .arg(format!(":{remote_port}"));
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| KubeError::Spawn {
                kubectl: self.spec.kubectl.clone(),
                message: e.to_string(),
            })?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        // kubectl's last error line, for when it exits.
        let last_error = Arc::new(std::sync::Mutex::new(String::new()));
        let errors = {
            let last_error = last_error.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if !line.trim().is_empty()
                        && let Ok(mut last) = last_error.lock()
                    {
                        *last = line.trim().to_string();
                    }
                }
            })
        };
        let mut lines = BufReader::new(stdout).lines();
        // "Forwarding from 127.0.0.1:54321 -> 5432"
        let local = tokio::time::timeout(START_TIMEOUT, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(address) = line
                    .strip_prefix("Forwarding from ")
                    .and_then(|rest| rest.split(" -> ").next())
                    && let Ok(address) = address.trim().parse::<SocketAddr>()
                {
                    return Some(address);
                }
            }
            None
        })
        .await;
        let local = match local {
            Ok(Some(local)) => local,
            Ok(None) => {
                // kubectl exited before forwarding: its own message says why.
                let _ = child.wait().await;
                let _ = errors.await;
                let message = last_error.lock().map(|m| m.clone()).unwrap_or_default();
                return Err(KubeError::Kubectl(if message.is_empty() {
                    "port-forward ended without forwarding".into()
                } else {
                    message
                }));
            }
            Err(_) => return Err(KubeError::Timeout),
        };
        let (stop, stopped) = oneshot::channel();
        let tunnel = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            // Keep reading stdout ("Handling connection…") so kubectl never blocks on it.
            let drain = async { while let Ok(Some(_)) = lines.next_line().await {} };
            tokio::select! {
                _ = stopped => {
                    let _ = child.kill().await;
                }
                _ = async { drain.await; let _ = child.wait().await; } => {
                    let _ = errors.await;
                    let message = last_error.lock().map(|m| m.clone()).unwrap_or_default();
                    if let Some(tunnel) = tunnel.upgrade() {
                        tunnel.state.send_replace(TunnelState::Down {
                            reason: if message.is_empty() {
                                "kubectl port-forward exited".into()
                            } else {
                                message
                            },
                        });
                    }
                }
            }
        });
        Ok(Forward {
            local,
            stop: Some(stop),
            task: Some(task),
        })
    }
}

/// `kubectl` with the tunnel's kubeconfig, context and namespace.
fn kubectl(spec: &ResolvedKubernetes) -> Command {
    let mut command = Command::new(&spec.kubectl);
    if let Some(kubeconfig) = &spec.kubeconfig {
        command.arg("--kubeconfig").arg(kubeconfig);
    }
    if let Some(context) = &spec.context {
        command.args(["--context", context]);
    }
    if let Some(namespace) = &spec.namespace {
        command.args(["--namespace", namespace]);
    }
    #[cfg(windows)]
    {
        // A GUI app must not flash a console window for each kubectl.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Runs a short `kubectl` command and returns its output lines, or its error.
async fn run(spec: &ResolvedKubernetes, args: &[&str]) -> Result<Vec<String>, KubeError> {
    // kubectl gives up on its own before our timer (an unreachable cluster says so in its
    // words), and a kubectl left behind by the timer is killed rather than orphaned.
    let output = tokio::time::timeout(
        START_TIMEOUT,
        kubectl(spec)
            .arg(format!("--request-timeout={}s", REQUEST_TIMEOUT.as_secs()))
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| KubeError::Timeout)?
    .map_err(|e| KubeError::Spawn {
        kubectl: spec.kubectl.clone(),
        message: e.to_string(),
    })?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        let message = message
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        return Err(KubeError::Kubectl(message.trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect())
}

/// The contexts of the kubeconfig, for the Kubernetes tab of the connection form.
pub async fn contexts(spec: &ResolvedKubernetes) -> Result<Vec<String>, KubeError> {
    let without_context = ResolvedKubernetes {
        context: None,
        namespace: None,
        ..spec.clone()
    };
    run(&without_context, &["config", "get-contexts", "-o", "name"]).await
}

/// The namespaces of the context, for the Kubernetes tab of the connection form.
pub async fn namespaces(spec: &ResolvedKubernetes) -> Result<Vec<String>, KubeError> {
    let without_namespace = ResolvedKubernetes {
        namespace: None,
        ..spec.clone()
    };
    Ok(
        run(&without_namespace, &["get", "namespaces", "-o", "name"])
            .await?
            .into_iter()
            .map(|n| n.trim_start_matches("namespace/").to_string())
            .collect(),
    )
}

/// Names of one kind of resource (`svc`, `pod` or `deployment`) in the namespace.
pub async fn names(spec: &ResolvedKubernetes, kind: &str) -> Result<Vec<String>, KubeError> {
    let kind = match kind {
        "svc" | "service" => "services",
        "pod" => "pods",
        "deployment" => "deployments",
        other => {
            return Err(KubeError::Kubectl(format!(
                "unknown resource type {other:?}"
            )));
        }
    };
    Ok(run(spec, &["get", kind, "-o", "name"])
        .await?
        .into_iter()
        .map(|name| {
            name.split_once('/')
                .map_or(name.clone(), |(_, n)| n.to_string())
        })
        .collect())
}

/// A TCP port of a resource, as `kubectl port-forward` takes it: a service's own port, or a
/// container port of a pod or deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePort {
    pub port: u16,
    /// The port's name in the manifest (`postgres`, `http`…), if it has one.
    pub name: Option<String>,
    /// The container behind it (pods and deployments).
    pub container: Option<String>,
}

/// The ports of the resource, read from its manifest; an error if it does not exist.
pub async fn ports(spec: &ResolvedKubernetes) -> Result<Vec<ResourcePort>, KubeError> {
    let json = run(spec, &["get", &spec.resource, "-o", "json"])
        .await?
        .join("\n");
    let manifest: serde_json::Value = serde_json::from_str(&json)
        .map_err(|e| KubeError::Kubectl(format!("unexpected output: {e}")))?;
    Ok(ports_of(&manifest))
}

fn ports_of(manifest: &serde_json::Value) -> Vec<ResourcePort> {
    let tcp = |p: &serde_json::Value| {
        p.get("protocol")
            .and_then(|v| v.as_str())
            .is_none_or(|v| v == "TCP")
    };
    let name = |p: &serde_json::Value| p.get("name").and_then(|v| v.as_str()).map(String::from);
    let number = |p: &serde_json::Value, key: &str| {
        p.get(key)
            .and_then(|v| v.as_u64())
            .and_then(|v| u16::try_from(v).ok())
    };
    let mut out = Vec::new();
    match manifest.get("kind").and_then(|k| k.as_str()) {
        Some("Service") => {
            for p in manifest
                .pointer("/spec/ports")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let (true, Some(port)) = (tcp(p), number(p, "port")) {
                    out.push(ResourcePort {
                        port,
                        name: name(p),
                        container: None,
                    });
                }
            }
        }
        kind => {
            let containers = if kind == Some("Pod") {
                "/spec/containers"
            } else {
                "/spec/template/spec/containers"
            };
            for c in manifest
                .pointer(containers)
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                let container = c.get("name").and_then(|v| v.as_str()).map(String::from);
                for p in c
                    .get("ports")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                {
                    if let (true, Some(port)) = (tcp(p), number(p, "containerPort")) {
                        out.push(ResourcePort {
                            port,
                            name: name(p),
                            container: container.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports_come_from_services_and_containers() {
        let service = serde_json::json!({"kind": "Service", "spec": {"ports": [
            {"name": "pg", "port": 5432, "protocol": "TCP", "targetPort": 5432},
            {"name": "dns", "port": 53, "protocol": "UDP"}
        ]}});
        assert_eq!(
            ports_of(&service),
            [ResourcePort {
                port: 5432,
                name: Some("pg".into()),
                container: None
            }]
        );
        let deployment = serde_json::json!({"kind": "Deployment", "spec": {"template": {"spec": {"containers": [
            {"name": "db", "ports": [{"containerPort": 3306}]},
            {"name": "metrics", "ports": [{"containerPort": 9104, "name": "metrics"}]}
        ]}}}});
        assert_eq!(
            ports_of(&deployment),
            [
                ResourcePort {
                    port: 3306,
                    name: None,
                    container: Some("db".into())
                },
                ResourcePort {
                    port: 9104,
                    name: Some("metrics".into()),
                    container: Some("metrics".into())
                },
            ]
        );
        let pod = serde_json::json!({"kind": "Pod", "spec": {"containers": [{"name": "pg", "ports": [{"containerPort": 5432}]}]}});
        assert_eq!(ports_of(&pod)[0].port, 5432);
    }

    #[tokio::test]
    async fn a_missing_kubectl_says_so() {
        let spec = ResolvedKubernetes {
            context: None,
            namespace: None,
            resource: "svc/x".into(),
            kubeconfig: None,
            kubectl: "spelaion-no-such-kubectl".into(),
        };
        let tunnel = Arc::new(KubeTunnel::new(spec.clone()));
        let err = tunnel.forward(5432).await.unwrap_err();
        assert!(matches!(err, KubeError::Spawn { .. }), "{err}");
        assert!(err.to_string().contains("is kubectl installed"), "{err}");
        assert!(matches!(ports(&spec).await, Err(KubeError::Spawn { .. })));
    }
}
