//! `spelunk`: MCP server and configuration utilities.
//!
//! For an agent (Claude Code, Claude Desktop…): `{"command": "spelunk", "args": ["mcp"]}`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use spelunk_config::{
    Severity, Source, TrustStore, check_workspace, default_user_dir, find_workspace, load,
};
use spelunk_store::{Decision, Store};

#[derive(Parser)]
#[command(name = "spelunk", version, about = "SpelunkDB from the command line")]
struct Cli {
    /// Personal configuration directory (defaults to the system one).
    #[arg(long, global = true, env = "SPELUNK_USER_DIR")]
    user_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read-only MCP server over stdio, for AI agents.
    Mcp {
        /// Directory from which to search for `.spelunk/` (defaults to the current one).
        #[arg(long)]
        workspace: Option<PathBuf>,
        /// Maximum time per query, in seconds.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Lists the visible connections and whether they are exposed to the MCP.
    Connections {
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
    /// Trusts a repository: allows resolving the secrets and commands of its `.spelunk/`.
    Trust {
        /// Repository directory (defaults to the one containing the current directory).
        path: Option<PathBuf>,
        /// Revokes trust instead of granting it.
        #[arg(long)]
        revoke: bool,
    },
    /// Checks a repository's `.spelunk/` for a team's CI or pre-commit hook: files that do
    /// not load, passwords written in commands, private keys in the repository… Exits with 1
    /// when it finds an error (or a warning, with --strict). Reads nothing personal.
    Check {
        /// Repository directory (defaults to the one containing the current directory).
        path: Option<PathBuf>,
        /// Fail on warnings too.
        #[arg(long)]
        strict: bool,
    },
    /// Shows the latest entries of the MCP audit log.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

/// `spelunk check`: prints each finding and fails when it must (ADR-022).
fn check(path: Option<PathBuf>, strict: bool) -> Result<()> {
    let start = workspace_or_cwd(path)?;
    let root = find_workspace(&start).ok_or_else(|| {
        anyhow!(
            "{} is not inside a repository with .spelunk/",
            start.display()
        )
    })?;
    let findings = check_workspace(&root);
    let (mut errors, mut warnings) = (0, 0);
    for finding in &findings {
        let shown = finding.path.strip_prefix(&root).unwrap_or(&finding.path);
        let label = match finding.severity {
            Severity::Error => {
                errors += 1;
                "error"
            }
            Severity::Warning => {
                warnings += 1;
                "warning"
            }
        };
        println!("{}: {label}: {}", shown.display(), finding.message);
    }
    println!(
        "{}: {errors} error(s), {warnings} warning(s)",
        root.join(spelunk_config::WORKSPACE_DIR).display()
    );
    if errors > 0 || (strict && warnings > 0) {
        return Err(anyhow!("the check failed"));
    }
    Ok(())
}

fn user_dir(cli: &Cli) -> Result<PathBuf> {
    cli.user_dir
        .clone()
        .or_else(default_user_dir)
        .ok_or_else(|| anyhow!("the configuration directory cannot be found"))
}

fn workspace_or_cwd(path: Option<PathBuf>) -> Result<PathBuf> {
    match path {
        Some(p) => Ok(p),
        None => std::env::current_dir().context("reading the current directory"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Checking a repository needs no personal directory (it runs in CI).
    if let Command::Check { path, strict } = cli.command {
        return check(path, strict);
    }
    let user_dir = user_dir(&cli)?;
    match cli.command {
        Command::Check { .. } => unreachable!("handled above"),
        Command::Mcp { workspace, timeout } => {
            // stdout is the protocol channel: notices go to stderr.
            let options = spelunk_mcp::ServerOptions {
                workspace: Some(workspace_or_cwd(workspace)?),
                user_dir,
                timeout: Duration::from_secs(timeout),
            };
            spelunk_mcp::serve_stdio(options)
                .await
                .map_err(|e| anyhow!(e))
        }
        Command::Connections { workspace } => {
            let config = load(Some(&workspace_or_cwd(workspace)?), &user_dir);
            let trust = TrustStore::load(&user_dir)?;
            if let Some(root) = &config.workspace_root {
                let status = if trust.is_trusted(root) {
                    "trusted"
                } else {
                    "NOT TRUSTED (spelunk trust)"
                };
                println!("Repository: {} — {status}", root.display());
            }
            for c in &config.connections {
                let origin = match c.id.source {
                    Source::Workspace => "repo",
                    Source::User => "personal",
                };
                let mcp = if c.file.mcp.exposed {
                    "exposed to the MCP"
                } else {
                    "not exposed"
                };
                println!(
                    "{:<30} {:<9} {:<9} {} — {mcp}",
                    c.id.to_string(),
                    c.file.engine.as_str(),
                    origin,
                    c.name()
                );
            }
            for problem in &config.problems {
                eprintln!("problem: {problem}");
            }
            Ok(())
        }
        Command::Trust { path, revoke } => {
            let start = workspace_or_cwd(path)?;
            let root = find_workspace(&start).ok_or_else(|| {
                anyhow!(
                    "{} is not inside a repository with .spelunk/",
                    start.display()
                )
            })?;
            let mut trust = TrustStore::load(&user_dir)?;
            if revoke {
                trust.revoke(&root)?;
                println!("Trust revoked for {}", root.display());
            } else {
                let config = load(Some(&root), &user_dir);
                let changes = trust.trust(&config)?;
                for change in &changes {
                    println!("Accepted: {change}");
                }
                println!(
                    "{} is now trusted as it is: its connections can resolve secrets and run the commands in its configuration. A later change to one of them is asked about again.",
                    root.display()
                );
            }
            Ok(())
        }
        Command::Audit { limit } => {
            let store = Store::open(&user_dir.join("state.db"))?;
            for e in store.recent(limit)? {
                let decision = match e.decision {
                    Decision::Executed => "executed",
                    Decision::Rejected => "REJECTED",
                    Decision::Failed => "failed",
                };
                let detail = e.detail.map(|d| format!(" — {d}")).unwrap_or_default();
                println!(
                    "[{}] {} · {} · {decision}: {}{detail}",
                    e.at_ms, e.client, e.connection, e.statement
                );
            }
            Ok(())
        }
    }
}
