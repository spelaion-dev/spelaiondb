//! `spelunk`: MCP server and configuration utilities.
//!
//! For an agent (Claude Code, Claude Desktop…): `{"command": "spelunk", "args": ["mcp"]}`.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use spelunk_config::{Source, TrustStore, default_user_dir, find_workspace, load};
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
    /// Shows the latest entries of the MCP audit log.
    Audit {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
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
    let user_dir = user_dir(&cli)?;
    match cli.command {
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
                trust.trust(&root)?;
                println!(
                    "{} is now trusted: its connections can resolve secrets and run the commands in its configuration.",
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
