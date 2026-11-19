//! A stand-in for `kubectl port-forward`, for the Kubernetes tunnel tests that need no cluster
//! (ADR-015). Never shipped. It is called as the tunnel calls kubectl:
//! `[--kubeconfig <file>] [--context …] [--namespace …] port-forward --address 127.0.0.1
//! <resource> :<port>`, and the resource says what it does:
//!
//! - `svc/missing` fails as kubectl does for a resource that does not exist.
//! - `svc/gated` waits until a file named `release` exists next to the kubeconfig before it
//!   forwards, so a test can look at a tunnel whose kubectl has not listened yet.
//! - Anything else forwards at once.
//!
//! Forwarding, it listens on a free local port and says so as kubectl does. A connection to the
//! remote port 6379 ends it as a lost pod connection ends kubectl; any other connection is
//! closed and it keeps listening.

use std::io::Write;
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The remote port whose first connection ends the forward.
const ENDS_ON_CONNECTION: &str = "6379";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let kubeconfig = args
        .iter()
        .position(|a| a == "--kubeconfig")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let Some(start) = args.iter().position(|a| a == "port-forward") else {
        eprintln!("fake kubectl: only port-forward is supported: {args:?}");
        return ExitCode::from(2);
    };
    let mut rest = args[start + 1..].iter();
    let mut positional = Vec::new();
    while let Some(arg) = rest.next() {
        if arg == "--address" {
            rest.next();
        } else {
            positional.push(arg.clone());
        }
    }
    let (Some(resource), Some(port)) = (positional.first(), positional.get(1)) else {
        eprintln!("fake kubectl: expected a resource and a port: {args:?}");
        return ExitCode::from(2);
    };
    let port = port.trim_start_matches(':').to_string();
    if resource == "svc/missing" {
        eprintln!("Error from server (NotFound): services \"missing\" not found");
        return ExitCode::from(1);
    }
    if resource == "svc/gated" {
        let release = kubeconfig
            .as_deref()
            .and_then(|k| k.parent())
            .map(|dir| dir.join("release"));
        let Some(release) = release else {
            eprintln!("fake kubectl: svc/gated needs --kubeconfig");
            return ExitCode::from(2);
        };
        let started = Instant::now();
        while !release.exists() {
            if started.elapsed() > Duration::from_secs(60) {
                eprintln!("fake kubectl: never released");
                return ExitCode::from(1);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("fake kubectl: {e}");
            return ExitCode::from(1);
        }
    };
    let local = listener
        .local_addr()
        .expect("a bound listener has an address");
    println!("Forwarding from {local} -> {port}");
    let _ = std::io::stdout().flush();
    for connection in listener.incoming() {
        if connection.is_err() {
            continue;
        }
        if port == ENDS_ON_CONNECTION {
            eprintln!("error: lost connection to pod");
            return ExitCode::from(1);
        }
        println!("Handling connection for {port}");
        let _ = std::io::stdout().flush();
    }
    ExitCode::SUCCESS
}
