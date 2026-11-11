//! AWS SSM Session Manager port forwarding (ADR-050): `aws ssm start-session` with the
//! `AWS-StartPortForwardingSession` documents, run with an argument list (never a shell) and the
//! user's AWS credentials as the AWS CLI finds them. It forwards a local port on 127.0.0.1 to a
//! port on the managed node, or through it to another host; the tunnel connects its first hop,
//! or each connection of a forward, to that local port.
//!
//! Nothing secret is ever given to it: no argument or environment variable of ours carries a
//! password. The AWS CLI starts the Session Manager plugin as its child, so stopping a session
//! stops the whole process tree.

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::{SshError, SsmFailure, SsmSpec};

/// How long a session may take to be ready at least: the CLI first asks AWS for it.
const MIN_START_TIMEOUT: Duration = Duration::from_secs(30);

/// The documents AWS provides for port forwarding.
const TO_NODE: &str = "AWS-StartPortForwardingSession";
const TO_REMOTE_HOST: &str = "AWS-StartPortForwardingSessionToRemoteHost";

fn fail(failure: SsmFailure, message: impl Into<String>) -> SshError {
    SshError::Ssm {
        message: message.into(),
        failure,
    }
}

/// The programs a session needs, found before anything starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SsmTools {
    pub aws: PathBuf,
    pub plugin: PathBuf,
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// Where the AWS CLI v2 installers put `aws`.
fn aws_install_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut dirs = Vec::new();
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(var) {
                dirs.push(PathBuf::from(base).join("Amazon").join("AWSCLIV2"));
            }
        }
        dirs
    } else {
        [
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/local/aws-cli/v2/current/bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// Where the Session Manager plugin's installers put it.
fn plugin_install_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        let mut dirs = Vec::new();
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(var) {
                dirs.push(
                    PathBuf::from(base)
                        .join("Amazon")
                        .join("SessionManagerPlugin")
                        .join("bin"),
                );
            }
        }
        dirs
    } else {
        [
            "/usr/local/sessionmanagerplugin/bin",
            "/usr/local/bin",
            "/opt/homebrew/bin",
            "/usr/bin",
        ]
        .into_iter()
        .map(PathBuf::from)
        .collect()
    }
}

/// `name` in the folders of `path` (a PATH value), then in `extra`, the first that exists.
fn find_in(name: &str, path: Option<&OsString>, extra: &[PathBuf]) -> Option<PathBuf> {
    let file = exe(name);
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(extra.iter().cloned())
        .map(|dir| dir.join(&file))
        .find(|candidate| candidate.is_file())
}

/// Finds `aws` (the one named, else on `path`, else in its installer's folders) and the plugin
/// (on `path`, in its installer's folders, or next to `aws`), or says which is missing.
pub(crate) fn find_tools_in(
    configured: Option<&str>,
    path: Option<&OsString>,
    aws_dirs: &[PathBuf],
    plugin_dirs: &[PathBuf],
) -> Result<SsmTools, SshError> {
    let aws = match configured {
        Some(program) => {
            let given = PathBuf::from(program);
            if given.components().count() > 1 || given.is_absolute() {
                given.is_file().then_some(given)
            } else {
                find_in(program.trim_end_matches(".exe"), path, &[])
            }
        }
        None => find_in("aws", path, aws_dirs),
    }
    .ok_or_else(|| {
        fail(
            SsmFailure::MissingCli,
            match configured {
                Some(program) => format!(
                    "the AWS CLI was not found at {program}: install the AWS CLI v2, or correct the path in the network profile"
                ),
                None => "the AWS CLI (aws) was not found on the PATH: install the AWS CLI v2".to_string(),
            },
        )
    })?;
    let mut dirs = plugin_dirs.to_vec();
    if let Some(next_to_aws) = aws.parent() {
        dirs.push(next_to_aws.to_path_buf());
    }
    let plugin = find_in("session-manager-plugin", path, &dirs).ok_or_else(|| {
        fail(
            SsmFailure::MissingPlugin,
            "the Session Manager plugin for the AWS CLI (session-manager-plugin) was not found: install it from AWS's \"Install the Session Manager plugin for the AWS CLI\" page",
        )
    })?;
    Ok(SsmTools { aws, plugin })
}

/// The tools for `spec` on this machine.
pub fn find_tools(spec: &SsmSpec) -> Result<SsmTools, SshError> {
    let path = std::env::var_os("PATH");
    find_tools_in(
        spec.aws.as_deref(),
        path.as_ref(),
        &aws_install_dirs(),
        &plugin_install_dirs(),
    )
}

/// A value that may become part of an argument: letters, digits and `extra`, not empty and not
/// starting with `-`, so it can neither be an option nor change the `--parameters` list.
fn checked<'a>(what: &str, value: &'a str, extra: &[char]) -> Result<&'a str, SshError> {
    let ok = !value.is_empty()
        && !value.starts_with('-')
        && value.len() <= 255
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(&c));
    if ok {
        Ok(value)
    } else {
        Err(fail(
            SsmFailure::Invalid,
            format!("the SSM {what} {value:?} has characters that cannot be passed to the AWS CLI"),
        ))
    }
}

fn is_node_itself(host: &str) -> bool {
    matches!(
        host.to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1" | "[::1]"
    )
}

/// The arguments of `aws` for a session to `host:port` (seen from the managed node) on
/// `local_port`: every option as `--name=value`, each value checked.
pub fn ssm_arguments(
    spec: &SsmSpec,
    host: &str,
    port: u16,
    local_port: u16,
) -> Result<Vec<String>, SshError> {
    // Instance (`i-…`), on-premises (`mi-…`) and ECS (`ecs:cluster_task_container`) targets.
    let target = checked("target", spec.target.trim(), &['-', '_', '.', ':'])?;
    let mut args = vec![
        "ssm".to_string(),
        "start-session".to_string(),
        format!("--target={target}"),
    ];
    if is_node_itself(host) {
        args.push(format!("--document-name={TO_NODE}"));
        args.push(format!(
            "--parameters=portNumber={port},localPortNumber={local_port}"
        ));
    } else {
        // Host names and IPv4 or IPv6 addresses; no `,` or `=` that would end the value.
        let host = checked("host", host, &['-', '_', '.', ':'])?;
        args.push(format!("--document-name={TO_REMOTE_HOST}"));
        args.push(format!(
            "--parameters=host={host},portNumber={port},localPortNumber={local_port}"
        ));
    }
    if let Some(region) = spec
        .region
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        args.push(format!("--region={}", checked("region", region, &['-'])?));
    }
    if let Some(profile) = spec
        .profile
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        // AWS profile names are free text in the config file; these cover what people use.
        args.push(format!(
            "--profile={}",
            checked("profile", profile, &['-', '_', '.', '@', '+', '/'])?
        ));
    }
    Ok(args)
}

/// A running session: `aws ssm start-session` (and the plugin it started), forwarding `local`.
pub(crate) struct SsmSession {
    pub local: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    pid: Option<u32>,
}

impl SsmSession {
    /// Starts a session to `host:port` and waits until it accepts connections. `on_exit` is
    /// told the CLI's last words if it exits on its own afterwards.
    pub(crate) async fn start(
        spec: &SsmSpec,
        host: &str,
        port: u16,
        timeout: Duration,
        on_exit: impl FnOnce(String) + Send + 'static,
    ) -> Result<SsmSession, SshError> {
        let tools = find_tools(spec)?;
        let local_port = free_port()?;
        let args = ssm_arguments(spec, host, port, local_port)?;
        let mut command = Command::new(&tools.aws);
        command.args(&args);
        // A plugin found outside the PATH is put on the PATH of the CLI, which looks there.
        if let Some(dir) = tools.plugin.parent() {
            let path = std::env::var_os("PATH");
            let on_path = path
                .as_ref()
                .is_some_and(|p| std::env::split_paths(p).any(|d| d == dir));
            if !on_path {
                let dirs = std::iter::once(dir.to_path_buf())
                    .chain(path.iter().flat_map(std::env::split_paths));
                if let Ok(joined) = std::env::join_paths(dirs) {
                    command.env("PATH", joined);
                }
            }
        }
        // The plugin is the CLI's child: a group (Unix) so both can be stopped together.
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        {
            // A GUI app must not flash a console window for the CLI.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                fail(
                    SsmFailure::MissingCli,
                    format!("could not run {}: {e}", tools.aws.display()),
                )
            })?;
        let pid = child.id();
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let last_error = Arc::new(Mutex::new(String::new()));
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
        let said = |last_error: &Arc<Mutex<String>>| {
            last_error.lock().map(|m| m.clone()).unwrap_or_default()
        };
        let mut lines = BufReader::new(stdout).lines();
        let wait = timeout.max(MIN_START_TIMEOUT);
        let ready = tokio::time::timeout(wait, async {
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim_start().starts_with("Waiting for connections") {
                    return true;
                }
            }
            false
        })
        .await;
        match ready {
            Ok(true) => {}
            Ok(false) => {
                let _ = child.wait().await;
                let _ = errors.await;
                let message = said(&last_error);
                return Err(fail(
                    SsmFailure::Exited,
                    if message.is_empty() {
                        "the AWS CLI ended the session before forwarding".to_string()
                    } else {
                        message
                    },
                ));
            }
            Err(_) => {
                kill_tree(pid);
                return Err(fail(
                    SsmFailure::Timeout,
                    format!(
                        "the SSM session was not ready within {}",
                        crate::tunnel::seconds(wait)
                    ),
                ));
            }
        }
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            // Keep reading ("Connection accepted…") so the plugin never blocks on its output.
            let drain = async { while let Ok(Some(_)) = lines.next_line().await {} };
            tokio::select! {
                _ = stopped => {
                    kill_tree(pid);
                    let _ = child.kill().await;
                }
                _ = async { drain.await; let _ = child.wait().await; } => {
                    let _ = errors.await;
                    let message = said(&last_error);
                    on_exit(if message.is_empty() {
                        "the SSM session ended".to_string()
                    } else {
                        message
                    });
                }
            }
        });
        Ok(SsmSession {
            local: SocketAddr::from(([127, 0, 0, 1], local_port)),
            stop: Some(stop),
            task: Some(task),
            pid,
        })
    }

    /// Stops the session (the CLI and its plugin) and waits for them to exit.
    pub(crate) async fn close(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SsmSession {
    fn drop(&mut self) {
        if self.stop.is_some() {
            // Not closed in order: stop the plugin too, which `kill_on_drop` would leave.
            kill_tree(self.pid);
        }
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// A free port on 127.0.0.1 for the plugin to listen on.
fn free_port() -> Result<u16, SshError> {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .map_err(|e| SshError::Other(format!("could not open a local port: {e}")))
}

/// Stops a process and everything it started: the AWS CLI and its plugin. With the system's
/// own tool and an argument list, so nothing here needs `unsafe`.
fn kill_tree(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let system = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let _ = std::process::Command::new(system.join("System32").join("taskkill.exe"))
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .status();
    }
    #[cfg(unix)]
    {
        // The CLI leads its own process group (`process_group(0)`): its id is the group's.
        let kill = if std::path::Path::new("/bin/kill").is_file() {
            "/bin/kill"
        } else {
            "/usr/bin/kill"
        };
        let _ = std::process::Command::new(kill)
            .args(["-KILL", "--", &format!("-{pid}")])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(target: &str) -> SsmSpec {
        SsmSpec {
            target: target.into(),
            region: Some("eu-west-1".into()),
            profile: Some("prod".into()),
            aws: None,
        }
    }

    #[test]
    fn arguments_name_every_option_with_its_value() {
        assert_eq!(
            ssm_arguments(
                &spec("i-0123456789abcdef0"),
                "orders.cluster-x.eu-west-1.rds.amazonaws.com",
                5432,
                40001
            )
            .unwrap(),
            [
                "ssm",
                "start-session",
                "--target=i-0123456789abcdef0",
                "--document-name=AWS-StartPortForwardingSessionToRemoteHost",
                "--parameters=host=orders.cluster-x.eu-west-1.rds.amazonaws.com,portNumber=5432,localPortNumber=40001",
                "--region=eu-west-1",
                "--profile=prod",
            ]
        );
        // A port on the node itself; no region or profile: the CLI's defaults.
        let plain = SsmSpec {
            region: None,
            profile: Some("  ".into()),
            ..spec("mi-0123456789abcdef0")
        };
        for node in ["localhost", "127.0.0.1", "::1", "LocalHost"] {
            assert_eq!(
                ssm_arguments(&plain, node, 6379, 40002).unwrap(),
                [
                    "ssm",
                    "start-session",
                    "--target=mi-0123456789abcdef0",
                    "--document-name=AWS-StartPortForwardingSession",
                    "--parameters=portNumber=6379,localPortNumber=40002",
                ]
            );
        }
        // ECS targets and IPv6 hosts.
        let ecs = ssm_arguments(&spec("ecs:prod_0123abcd_0123abcd-1"), "fd00::5", 3306, 1).unwrap();
        assert!(
            ecs.contains(
                &"--parameters=host=fd00::5,portNumber=3306,localPortNumber=1".to_string()
            )
        );
    }

    /// Values come from a team's file: none may become an option, end the `--parameters` list
    /// early or smuggle text past the AWS CLI's parser.
    #[test]
    fn values_that_could_change_the_command_are_refused() {
        let bad_targets = [
            "",
            "--debug",
            "-i-123",
            "i-123 --debug",
            "i-123,host=evil",
            "i-123=x",
            "i-123\n--endpoint-url=http://evil",
            "i-123;calc",
            "i-123&calc",
            "i-123|calc",
            "i-123\"",
            "i-123'",
            "i-123`x`",
            "$(calc)",
            "i-123%PATH%",
            "i-12\u{202E}3",
        ];
        for target in bad_targets {
            let error = ssm_arguments(&spec(target), "localhost", 5432, 1).unwrap_err();
            assert!(
                matches!(
                    error,
                    SshError::Ssm {
                        failure: SsmFailure::Invalid,
                        ..
                    }
                ),
                "{target:?}: {error}"
            );
        }
        for host in [
            "db,localPortNumber=1",
            "db=1",
            "db host",
            "-db",
            "db\r",
            "db/x",
            "",
        ] {
            assert!(
                ssm_arguments(&spec("i-1"), host, 5432, 1).is_err(),
                "{host:?}"
            );
        }
        for region in ["eu west", "--debug", "eu-west-1,x", "eu_west"] {
            let s = SsmSpec {
                region: Some(region.into()),
                ..spec("i-1")
            };
            assert!(
                ssm_arguments(&s, "localhost", 5432, 1).is_err(),
                "{region:?}"
            );
        }
        for profile in ["--debug", "a b", "a,b", "a=b", "a\"b", "a;b", "-x"] {
            let s = SsmSpec {
                profile: Some(profile.into()),
                ..spec("i-1")
            };
            assert!(
                ssm_arguments(&s, "localhost", 5432, 1).is_err(),
                "{profile:?}"
            );
        }
        assert!(ssm_arguments(&spec(&"i".repeat(300)), "localhost", 1, 1).is_err());
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-ssm-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(dir: &std::path::Path, name: &str) -> PathBuf {
        let path = dir.join(exe(name));
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn missing_tools_are_named() {
        let empty = temp("empty");
        let error = find_tools_in(None, None, std::slice::from_ref(&empty), &[]).unwrap_err();
        assert!(
            matches!(&error, SshError::Ssm { failure: SsmFailure::MissingCli, message } if message.contains("AWS CLI")),
            "{error}"
        );
        let configured = empty.join("nowhere").join(exe("aws"));
        let error = find_tools_in(Some(configured.to_str().unwrap()), None, &[], &[]).unwrap_err();
        assert!(error.to_string().contains("nowhere"), "{error}");

        // The CLI on the PATH, the plugin nowhere.
        let cli = temp("cli");
        let aws = touch(&cli, "aws");
        let path = std::env::join_paths([&cli]).unwrap();
        let error = find_tools_in(None, Some(&path), &[], &[]).unwrap_err();
        assert!(
            matches!(&error, SshError::Ssm { failure: SsmFailure::MissingPlugin, message } if message.contains("session-manager-plugin")),
            "{error}"
        );

        // The plugin in its installer's folder (not on the PATH), or next to the CLI.
        let installed = temp("plugin");
        let plugin = touch(&installed, "session-manager-plugin");
        let found =
            find_tools_in(None, Some(&path), &[], std::slice::from_ref(&installed)).unwrap();
        assert_eq!(
            found,
            SsmTools {
                aws: aws.clone(),
                plugin
            }
        );
        let beside = touch(&cli, "session-manager-plugin");
        let found = find_tools_in(Some(aws.to_str().unwrap()), None, &[], &[]).unwrap();
        assert_eq!(
            found,
            SsmTools {
                aws: aws.clone(),
                plugin: beside
            }
        );
        // The CLI in its installer's folder, not on the PATH.
        let found = find_tools_in(None, None, std::slice::from_ref(&cli), &[]).unwrap();
        assert_eq!(found.aws, aws);
        for dir in [empty, cli, installed] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
