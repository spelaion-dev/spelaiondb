//! Phase 0, experiment 2: PostgreSQL through N SSH hops with `russh`.
//!
//! Opens one SSH session per hop (each one running over a `direct-tcpip` channel of the
//! previous one), forwards an ephemeral local port to the target and checks with
//! `tokio-postgres`: a query, cancellation (which opens a second connection through the
//! tunnel) and streaming performance.
//!
//! Throwaway code: it validates risks, it is not the product design (see ADR-007).

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::PublicKeyOrCertificate;
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg};
use tokio::net::TcpListener;

#[derive(Parser)]
#[command(about = "Experiment 2: PostgreSQL through N SSH hops")]
struct Args {
    /// Hops in order, comma-separated: user@host:port,user@host:port
    #[arg(long, value_delimiter = ',', required = true)]
    hops: Vec<Hop>,

    /// Final target as seen from the last hop
    #[arg(long, default_value = "postgres:5432")]
    target: String,

    /// Authentication method (the same for every hop)
    #[arg(long, value_enum)]
    auth: AuthMethod,

    /// Private key (for --auth key)
    #[arg(long)]
    key: Option<PathBuf>,

    /// Key passphrase (for --auth key)
    #[arg(long, env = "SPELUNK_KEY_PASSPHRASE", hide_env_values = true)]
    passphrase: Option<String>,

    /// PostgreSQL user, password and database (host and port come from the tunnel)
    #[arg(long, default_value = "user=spelunk password=spelunk dbname=spelunk")]
    pg: String,
}

#[derive(Clone, Copy, ValueEnum)]
enum AuthMethod {
    /// Private key file
    Key,
    /// Agent via SSH_AUTH_SOCK (Linux/macOS)
    Agent,
    /// Windows OpenSSH agent (\\.\pipe\openssh-ssh-agent)
    OpensshPipe,
    /// Pageant (PuTTY)
    Pageant,
}

#[derive(Clone)]
struct Hop {
    user: String,
    host: String,
    port: u16,
}

impl FromStr for Hop {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let (user, rest) = s
            .split_once('@')
            .ok_or_else(|| anyhow!("missing user@ in '{s}'"))?;
        let (host, port) = match rest.rsplit_once(':') {
            Some((h, p)) => (
                h,
                p.parse()
                    .with_context(|| format!("invalid port in '{s}'"))?,
            ),
            None => (rest, 22),
        };
        Ok(Hop {
            user: user.into(),
            host: host.into(),
            port,
        })
    }
}

/// Accepts any host key and shows its fingerprint. The product will do TOFU with
/// known_hosts (ADR-007); here only connectivity matters.
struct Client {
    label: String,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let fingerprint = match server_key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
            PublicKeyOrCertificate::Certificate(cert) => {
                keys::PublicKey::new(cert.public_key().clone(), "").fingerprint(HashAlg::Sha256)
            }
        };
        println!("  [{}] host fingerprint: {fingerprint}", self.label);
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(15)),
        ..Default::default()
    });

    // 1. Chain the hops.
    let mut sessions: Vec<Handle<Client>> = Vec::new();
    for (i, hop) in args.hops.iter().enumerate() {
        let label = format!("hop {} {}@{}:{}", i + 1, hop.user, hop.host, hop.port);
        let started = Instant::now();
        let handler = Client {
            label: label.clone(),
        };

        let mut session = match sessions.last() {
            None => client::connect(config.clone(), (hop.host.as_str(), hop.port), handler)
                .await
                .with_context(|| format!("connecting to {label}"))?,
            Some(prev) => {
                let channel = prev
                    .channel_open_direct_tcpip(hop.host.clone(), hop.port.into(), "127.0.0.1", 0)
                    .await
                    .with_context(|| format!("opening a channel to {label}"))?;
                client::connect_stream(config.clone(), channel.into_stream(), handler)
                    .await
                    .with_context(|| format!("negotiating SSH with {label}"))?
            }
        };

        authenticate(&mut session, &hop.user, &args)
            .await
            .with_context(|| format!("authenticating at {label}"))?;
        println!("  [{label}] OK en {:?}", started.elapsed());
        sessions.push(session);
    }

    // 2. Forward an ephemeral local port to the target through the last hop.
    let (target_host, target_port) = args
        .target
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("--target must be host:port"))?;
    let target_host = target_host.to_string();
    let target_port: u32 = target_port.parse().context("invalid --target port")?;

    let last = Arc::new(sessions.pop().expect("at least one hop"));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_port = listener.local_addr()?.port();
    println!("  tunnel: 127.0.0.1:{local_port} -> {}", args.target);

    let forwarder = {
        let last = last.clone();
        tokio::spawn(async move {
            loop {
                let (mut tcp, _) = match listener.accept().await {
                    Ok(conn) => conn,
                    Err(e) => {
                        eprintln!("  accept failed: {e}");
                        return;
                    }
                };
                let last = last.clone();
                let host = target_host.clone();
                tokio::spawn(async move {
                    match last
                        .channel_open_direct_tcpip(host, target_port, "127.0.0.1", 0)
                        .await
                    {
                        Ok(channel) => {
                            let mut stream = channel.into_stream();
                            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
                        }
                        Err(e) => eprintln!("  could not open the channel to the target: {e}"),
                    }
                });
            }
        })
    };

    // 3. PostgreSQL checks through the tunnel.
    let pg_config = format!("host=127.0.0.1 port={local_port} {}", args.pg);
    let result = postgres_checks(&pg_config).await;

    forwarder.abort();
    result?;
    println!("\nPRUEBA 2: OK");
    Ok(())
}

async fn authenticate(session: &mut Handle<Client>, user: &str, args: &Args) -> Result<()> {
    match args.auth {
        AuthMethod::Key => {
            let path = args
                .key
                .as_ref()
                .ok_or_else(|| anyhow!("--auth key needs --key"))?;
            let key = keys::load_secret_key(path, args.passphrase.as_deref())
                .with_context(|| format!("loading {}", path.display()))?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            let res = session
                .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?;
            if !res.success() {
                bail!("the server rejected the key");
            }
            Ok(())
        }
        AuthMethod::Agent => {
            #[cfg(unix)]
            {
                let agent = keys::agent::client::AgentClient::connect_env()
                    .await
                    .context("connecting to SSH_AUTH_SOCK")?;
                authenticate_with_agent(session, user, agent).await
            }
            #[cfg(not(unix))]
            bail!("--auth agent is for Linux/macOS; on Windows use openssh-pipe or pageant")
        }
        AuthMethod::OpensshPipe => {
            #[cfg(windows)]
            {
                let agent = keys::agent::client::AgentClient::connect_named_pipe(
                    r"\\.\pipe\openssh-ssh-agent",
                )
                .await
                .context("connecting to the Windows OpenSSH agent (ssh-agent service stopped?)")?;
                authenticate_with_agent(session, user, agent).await
            }
            #[cfg(not(windows))]
            bail!("--auth openssh-pipe only exists on Windows")
        }
        AuthMethod::Pageant => {
            #[cfg(windows)]
            {
                let agent = keys::agent::client::AgentClient::connect_pageant()
                    .await
                    .context("connecting to Pageant (is it running?)")?;
                authenticate_with_agent(session, user, agent).await
            }
            #[cfg(not(windows))]
            bail!("--auth pageant only exists on Windows")
        }
    }
}

#[allow(dead_code)] // unused on platforms without any agent compiled in
async fn authenticate_with_agent<S>(
    session: &mut Handle<Client>,
    user: &str,
    mut agent: keys::agent::client::AgentClient<S>,
) -> Result<()>
where
    S: keys::agent::client::AgentStream + Unpin + Send + 'static,
{
    let identities = agent
        .request_identities()
        .await
        .context("requesting keys from the agent")?;
    if identities.is_empty() {
        bail!("the agent has no keys loaded");
    }
    let hash = session.best_supported_rsa_hash().await?.flatten();
    for identity in identities {
        let comment = identity.comment().to_string();
        let res = match identity {
            AgentIdentity::PublicKey { key, .. } => {
                session
                    .authenticate_publickey_with(user, key, hash, &mut agent)
                    .await
            }
            AgentIdentity::Certificate { certificate, .. } => {
                session
                    .authenticate_certificate_with(user, certificate, hash, &mut agent)
                    .await
            }
        }
        .map_err(|e| anyhow!("signing with the agent: {e:?}"))?;
        if res.success() {
            println!("  authenticated with the agent key '{comment}'");
            return Ok(());
        }
    }
    bail!("the server rejected all the agent's keys")
}

async fn postgres_checks(pg_config: &str) -> Result<()> {
    use tokio_postgres::NoTls;

    let started = Instant::now();
    let (client, connection) = tokio_postgres::connect(pg_config, NoTls)
        .await
        .context("connecting to PostgreSQL through the tunnel")?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("  PG connection ended: {e}");
        }
    });
    println!("  PG connected in {:?}", started.elapsed());

    let row = client
        .query_one(
            "select version(), inet_server_addr()::text, (select count(*) from big)",
            &[],
        )
        .await?;
    let version: String = row.get(0);
    let addr: Option<String> = row.get(1);
    let count: i64 = row.get(2);
    println!(
        "  {} | server {} | big = {count} rows",
        version.split(',').next().unwrap_or(&version),
        addr.unwrap_or_else(|| "?".into())
    );

    // Cancellation: PostgreSQL sends it over a new connection, which has to go through the
    // same tunnel. If the forward does not accept several connections, this fails.
    let cancel = client.cancel_token();
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        cancel.cancel_query(NoTls).await
    });
    let started = Instant::now();
    let sleep = client.simple_query("select pg_sleep(30)").await;
    canceller.await?.context("sending the cancellation")?;
    match sleep {
        Err(e) if e.code() == Some(&tokio_postgres::error::SqlState::QUERY_CANCELED) => {
            println!(
                "  cancellation OK: pg_sleep(30) cut after {:?}",
                started.elapsed()
            );
        }
        Err(e) => bail!("pg_sleep failed for another reason: {e}"),
        Ok(_) => bail!("pg_sleep(30) was not cancelled"),
    }

    // Streaming: 1 M rows through the hops.
    use futures_util::TryStreamExt;
    let started = Instant::now();
    let stream = client
        .query_raw(
            "select id, txt, ts, amount, flag from big",
            std::iter::empty::<i32>(),
        )
        .await?;
    let mut stream = std::pin::pin!(stream);
    let mut first = None;
    let mut rows = 0u64;
    while let Some(_row) = stream.try_next().await? {
        if first.is_none() {
            first = Some(started.elapsed());
        }
        rows += 1;
    }
    let total = started.elapsed();
    println!(
        "  streaming: {rows} rows, first after {:?}, total {:?} ({:.0} rows/s)",
        first.unwrap_or_default(),
        total,
        rows as f64 / total.as_secs_f64()
    );
    Ok(())
}
