//! A stand-in for the AWS CLI and its Session Manager plugin, for the SSM tests (ADR-050). Never
//! shipped. The tests copy this program next to itself as `aws` and `session-manager-plugin`:
//!
//! - As `aws`, it writes its arguments (one per line) to `aws-args.txt` and its environment to
//!   `aws-env.txt`, next to itself, checks that it was asked for an SSM port forward, and starts
//!   `session-manager-plugin` from its own folder, as the real CLI does, waiting for it.
//! - As `session-manager-plugin`, it listens on the local port it was given, says what the real
//!   plugin says once it is ready, and carries every connection to the host and port (as seen
//!   from "the managed node", which here is this machine).
//!
//! The target `i-unreachable` fails like the real CLI does for a node that is not connected.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    let exe = std::env::current_exe().expect("own path");
    let dir = exe.parent().expect("own folder").to_path_buf();
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if stem == "session-manager-plugin" {
        plugin(&args)
    } else {
        aws(&dir, &args)
    }
}

fn aws(dir: &std::path::Path, args: &[String]) -> ExitCode {
    let _ = std::fs::write(dir.join("aws-args.txt"), args.join("\n"));
    let env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect();
    let _ = std::fs::write(dir.join("aws-env.txt"), env.join("\n"));
    if args.first().map(String::as_str) == Some("--version") {
        println!("aws-cli/2.99.0 fake");
        return ExitCode::SUCCESS;
    }
    if args.get(..2) != Some(&["ssm".to_string(), "start-session".to_string()][..]) {
        eprintln!("fake aws: not an SSM session: {args:?}");
        return ExitCode::from(2);
    }
    let value = |name: &str| {
        args.iter()
            .find_map(|a| a.strip_prefix(&format!("--{name}=")))
            .map(str::to_string)
    };
    let target = value("target").unwrap_or_default();
    if target == "i-unreachable" {
        eprintln!();
        eprintln!(
            "An error occurred (TargetNotConnected) when calling the StartSession operation: {target} is not connected."
        );
        return ExitCode::from(254);
    }
    let document = value("document-name").unwrap_or_default();
    let parameters = value("parameters").unwrap_or_default();
    let mut host = None;
    let mut port = None;
    let mut local = None;
    for pair in parameters.split(',') {
        match pair.split_once('=') {
            Some(("host", v)) => host = Some(v.to_string()),
            Some(("portNumber", v)) => port = Some(v.to_string()),
            Some(("localPortNumber", v)) => local = Some(v.to_string()),
            _ => {
                eprintln!("fake aws: unexpected parameter {pair:?}");
                return ExitCode::from(2);
            }
        }
    }
    let host = match (document.as_str(), host) {
        ("AWS-StartPortForwardingSession", None) => "127.0.0.1".to_string(),
        ("AWS-StartPortForwardingSessionToRemoteHost", Some(host)) => host,
        _ => {
            eprintln!("fake aws: document and parameters do not match");
            return ExitCode::from(2);
        }
    };
    let (Some(port), Some(local)) = (port, local) else {
        eprintln!("fake aws: missing portNumber or localPortNumber");
        return ExitCode::from(2);
    };
    let plugin = dir.join(if cfg!(windows) {
        "session-manager-plugin.exe"
    } else {
        "session-manager-plugin"
    });
    println!();
    println!("Starting session with SessionId: fake-0123456789");
    let _ = std::io::stdout().flush();
    match Command::new(plugin).args([&host, &port, &local]).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(e) => {
            eprintln!("SessionManagerPlugin is not found: {e}");
            ExitCode::from(255)
        }
    }
}

fn plugin(args: &[String]) -> ExitCode {
    let [host, port, local] = args else {
        eprintln!("fake plugin: expected host, port and local port");
        return ExitCode::from(2);
    };
    let listener = match TcpListener::bind(("127.0.0.1", local.parse::<u16>().unwrap_or(0))) {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("fake plugin: {e}");
            return ExitCode::from(1);
        }
    };
    println!("Port {local} opened for sessionId fake-0123456789.");
    println!("Waiting for connections...");
    let _ = std::io::stdout().flush();
    let remote = format!("{host}:{port}");
    for client in listener.incoming() {
        let Ok(client) = client else { continue };
        println!();
        println!("Connection accepted for session [fake-0123456789]");
        let _ = std::io::stdout().flush();
        let remote = remote.clone();
        std::thread::spawn(move || {
            if let Ok(server) = TcpStream::connect(&remote) {
                relay(client, server);
            }
        });
    }
    ExitCode::SUCCESS
}

fn relay(client: TcpStream, server: TcpStream) {
    let (Ok(mut client_r), Ok(mut server_r)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let mut client_w = client;
    let mut server_w = server;
    let up = std::thread::spawn(move || copy(&mut client_r, &mut server_w));
    copy(&mut server_r, &mut client_w);
    let _ = up.join();
}

fn copy(from: &mut TcpStream, to: &mut TcpStream) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(std::net::Shutdown::Write);
}
