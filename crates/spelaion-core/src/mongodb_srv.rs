//! Initial MongoDB DNS seed discovery belongs to the core: a plugin is given one routed
//! endpoint and must never discover addresses that bypass that route (ADR-041, ADR-056).

use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RData;
use spelaion_config::{Engine, ResolvedConnection};
use spelaion_driver_api::{ConnectParams, DriverError, TlsMode};

use crate::open::OpenError;

const PREFIX: &str = "mongodb+srv://";

fn invalid(reason: &str) -> OpenError {
    DriverError::Connection(format!("MongoDB SRV discovery: {reason}")).into()
}

/// Resolve before opening any route. An ordinary host never creates a DNS resolver here.
pub(crate) async fn resolve(
    resolved: &ResolvedConnection,
    params: &mut ConnectParams,
) -> Result<(), OpenError> {
    if resolved.kubernetes.is_some() && params.host.starts_with(PREFIX) {
        return Err(invalid("an SRV seed cannot use a Kubernetes port-forward"));
    }
    resolve_host(resolved.engine, params).await
}

async fn resolve_host(engine: Engine, params: &mut ConnectParams) -> Result<(), OpenError> {
    let Some(seed) = seed(engine, params)? else {
        return Ok(());
    };
    let resolver = TokioResolver::builder_tokio()
        .map_err(|_| invalid("could not read the system DNS configuration"))?
        .build()
        .map_err(|_| invalid("could not configure the system DNS resolver"))?;
    resolve_bounded(&resolver, &seed, params).await
}

async fn resolve_bounded(
    resolver: &TokioResolver,
    seed: &str,
    params: &mut ConnectParams,
) -> Result<(), OpenError> {
    let after = params.connect_timeout();
    tokio::time::timeout(after, resolve_with(resolver, seed, params))
        .await
        .map_err(|_| OpenError::Timeout { after })?
}

fn seed(engine: Engine, params: &ConnectParams) -> Result<Option<String>, OpenError> {
    let Some(host) = params.host.strip_prefix(PREFIX) else {
        return Ok(None);
    };
    if engine != Engine::Mongodb {
        return Err(invalid("a mongodb+srv host is only for MongoDB"));
    }
    let host = host.strip_suffix('/').unwrap_or(host);
    // This is a host field, never a credential-bearing URL. Do not echo its input in errors.
    if !dns_host(host) || host.split('.').count() < 3 || host.parse::<std::net::IpAddr>().is_ok() {
        return Err(invalid(
            "give one DNS host with at least three labels; put credentials and the database in their own fields",
        ));
    }
    if params.port != 0 && params.port != 27017 {
        return Err(invalid(
            "the port comes from DNS; leave the port at its default",
        ));
    }
    if params.connect_addr.is_some() {
        return Err(invalid("resolve the seed before opening its route"));
    }
    Ok(Some(host.to_ascii_lowercase()))
}

fn dns_host(host: &str) -> bool {
    host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

async fn resolve_with(
    resolver: &TokioResolver,
    seed: &str,
    params: &mut ConnectParams,
) -> Result<(), OpenError> {
    // Absolute names: a failed query must not fall through to a system search domain.
    let records = resolver
        .srv_lookup(format!("_mongodb._tcp.{seed}."))
        .await
        .map_err(|_| invalid("the SRV lookup failed or returned no servers"))?;
    let parent = seed.split_once('.').expect("validated seed").1;
    let suffix = format!(".{parent}");
    let mut hosts = Vec::new();
    for record in records.answers() {
        let RData::SRV(record) = &record.data else {
            continue;
        };
        let host = record.target.to_utf8();
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        // Validate every answer, including ones not selected: a DNS record cannot redirect
        // credentials or an SSH channel to a different parent domain.
        if !dns_host(&host) || !host.ends_with(&suffix) || record.port == 0 {
            return Err(invalid(
                "an SRV target is outside the seed's parent domain or has an invalid host or port",
            ));
        }
        hosts.push((host, record.port));
    }
    let Some((host, port)) = hosts.into_iter().next() else {
        return Err(invalid("the SRV lookup returned no servers"));
    };
    let mut options = std::collections::BTreeMap::new();
    match resolver.txt_lookup(format!("{seed}.")).await {
        Ok(records) => {
            let mut records = records
                .answers()
                .iter()
                .filter_map(|record| match &record.data {
                    RData::TXT(text) => Some(text),
                    _ => None,
                });
            if let Some(record) = records.next() {
                if records.next().is_some() {
                    return Err(invalid("DNS returned more than one TXT record"));
                }
                // A TXT record may contain several strings; they form one option list.
                let text: Vec<_> = record
                    .txt_data
                    .iter()
                    .flat_map(|s| s.iter().copied())
                    .collect();
                let text = std::str::from_utf8(&text)
                    .map_err(|_| invalid("the TXT record is not UTF-8"))?;
                for pair in text.split('&') {
                    let Some((key, value)) = pair.split_once('=') else {
                        return Err(invalid("the TXT record is not a list of key=value options"));
                    };
                    if !matches!(key, "authSource" | "replicaSet")
                        || value.is_empty()
                        || value.len() > 1024
                        || value.chars().any(char::is_control)
                        || options.insert(key.to_string(), value.to_string()).is_some()
                    {
                        return Err(invalid(
                            "TXT options must be unique, nonempty authSource or replicaSet values",
                        ));
                    }
                }
            }
        }
        // An absent TXT record is valid; a timeout or a server failure is not absence.
        Err(error) if error.is_no_records_found() => {}
        Err(_) => return Err(invalid("the TXT lookup failed")),
    }
    // The core's own settings win over DNS. Only the TXT allowlist above can enter here.
    options.extend(params.options.clone());
    params.host = host;
    params.port = port;
    params.options = options;
    // SRV always encrypts and checks the resolved hostname, including through a route.
    // Never silently weaken TLS for a plugin that cannot honour it through a tunnel.
    params.tls = TlsMode::VerifyFull;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use hickory_resolver::config::{NameServerConfig, ResolverConfig};
    use hickory_resolver::net::runtime::TokioRuntimeProvider;
    use hickory_resolver::proto::op::{Message, OpCode, ResponseCode};
    use hickory_resolver::proto::rr::rdata::{SRV, TXT};
    use hickory_resolver::proto::rr::{Name, RData, Record, RecordType};

    struct Dns {
        resolver: TokioResolver,
        task: tokio::task::JoinHandle<()>,
        queries: Arc<Mutex<Vec<(String, RecordType)>>>,
    }

    impl Drop for Dns {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn config_at(addr: std::net::SocketAddr) -> ResolverConfig {
        let mut server = NameServerConfig::udp(addr.ip());
        server.connections[0].port = addr.port();
        ResolverConfig::from_name_servers(vec![server])
    }

    impl Dns {
        async fn new(hosts: Vec<(&str, u16)>, texts: Vec<Vec<&str>>, code: ResponseCode) -> Self {
            Self::with_codes(hosts, texts, code, code).await
        }

        async fn with_codes(
            hosts: Vec<(&str, u16)>,
            texts: Vec<Vec<&str>>,
            srv_code: ResponseCode,
            txt_code: ResponseCode,
        ) -> Self {
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let config = config_at(socket.local_addr().unwrap());
            let mut builder =
                TokioResolver::builder_with_config(config, TokioRuntimeProvider::default());
            builder.options_mut().attempts = 1;
            builder.options_mut().timeout = Duration::from_millis(500);
            let resolver = builder.build().unwrap();
            let hosts: Vec<_> = hosts
                .into_iter()
                .map(|(host, port)| (host.to_string(), port))
                .collect();
            let texts: Vec<Vec<String>> = texts
                .into_iter()
                .map(|v| v.into_iter().map(str::to_string).collect())
                .collect();
            let queries = Arc::new(Mutex::new(Vec::new()));
            let seen = queries.clone();
            let task = tokio::spawn(async move {
                let mut bytes = [0; 4096];
                loop {
                    let (size, sender) = socket.recv_from(&mut bytes).await.unwrap();
                    let request = Message::from_vec(&bytes[..size]).unwrap();
                    let query = request.queries[0].clone();
                    let code = if query.query_type == RecordType::TXT {
                        txt_code
                    } else {
                        srv_code
                    };
                    seen.lock()
                        .unwrap()
                        .push((query.name.to_utf8(), query.query_type));
                    let mut answer = Message::response(request.metadata.id, OpCode::Query);
                    answer.metadata.recursion_desired = true;
                    answer.metadata.recursion_available = true;
                    answer.metadata.response_code = code;
                    answer.add_query(query.clone());
                    if code == ResponseCode::NoError {
                        match query.query_type {
                            RecordType::SRV => {
                                for (host, port) in &hosts {
                                    answer.add_answer(Record::from_rdata(
                                        query.name.clone(),
                                        0,
                                        RData::SRV(SRV::new(
                                            0,
                                            0,
                                            *port,
                                            Name::from_ascii(host).unwrap(),
                                        )),
                                    ));
                                }
                            }
                            RecordType::TXT => {
                                for text in &texts {
                                    answer.add_answer(Record::from_rdata(
                                        query.name.clone(),
                                        0,
                                        RData::TXT(TXT::new(text.clone())),
                                    ));
                                }
                            }
                            _ => panic!("unexpected DNS query"),
                        }
                    }
                    socket
                        .send_to(&answer.to_vec().unwrap(), sender)
                        .await
                        .unwrap();
                }
            });
            Self {
                resolver,
                task,
                queries,
            }
        }

        async fn resolve(&self, params: &mut ConnectParams) -> Result<(), OpenError> {
            let seed = seed(Engine::Mongodb, params)?.expect("an SRV seed");
            tokio::time::timeout(
                Duration::from_secs(2),
                resolve_with(&self.resolver, &seed, params),
            )
            .await
            .expect("local DNS lookup must finish")
        }
    }

    fn params() -> ConnectParams {
        ConnectParams {
            host: "mongodb+srv://cluster.example.test/".into(),
            port: 27017,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn local_dns_resolves_the_route_and_only_txt_settings_with_tls() {
        let dns = Dns::new(
            vec![("node.example.test.", 27123)],
            vec![vec!["authSource=ad", "min&replicaSet=atlas-1"]],
            ResponseCode::NoError,
        )
        .await;
        let mut params = params();
        params.tls = TlsMode::Disable;
        dns.resolve(&mut params).await.unwrap();
        assert_eq!((&*params.host, params.port), ("node.example.test", 27123));
        assert_eq!(params.tls, TlsMode::VerifyFull);
        assert_eq!(
            params.options.get("authSource").map(String::as_str),
            Some("admin")
        );
        assert_eq!(
            params.options.get("replicaSet").map(String::as_str),
            Some("atlas-1")
        );
        // This is the same target start_tunnel passes to forward_on, and the hostname the
        // plugin receives for TLS even when connect_addr becomes the tunnel's loopback end.
        let target = crate::open::target(&params);
        assert_eq!((&*target.host, target.port), ("node.example.test", 27123));
        params.connect_addr = Some("127.0.0.1:12345".parse().unwrap());
        assert_eq!(params.host, "node.example.test");
        assert_eq!(
            *dns.queries.lock().unwrap(),
            vec![
                (
                    "_mongodb._tcp.cluster.example.test.".into(),
                    RecordType::SRV
                ),
                ("cluster.example.test.".into(), RecordType::TXT)
            ]
        );
    }

    #[tokio::test]
    async fn no_txt_is_valid_and_explicit_options_win_over_dns() {
        for texts in [vec![], vec![vec!["authSource=dns"]]] {
            let dns = Dns::new(
                vec![("node.example.test.", 27017)],
                texts,
                ResponseCode::NoError,
            )
            .await;
            let mut params = params();
            params
                .options
                .insert("authSource".into(), "explicit".into());
            dns.resolve(&mut params).await.unwrap();
            assert_eq!(params.options["authSource"], "explicit");
        }
    }

    #[tokio::test]
    async fn every_srv_target_is_checked_before_any_route_or_credentials_are_used() {
        for target in ["node.evil.test.", "example.test.", "nodeexample.test.", "."] {
            let dns = Dns::new(
                vec![("good.example.test.", 27017), (target, 27017)],
                vec![],
                ResponseCode::NoError,
            )
            .await;
            let mut params = params();
            let before = params.host.clone();
            assert!(dns.resolve(&mut params).await.is_err(), "{target}");
            assert_eq!(params.host, before);
            assert!(params.options.is_empty());
        }
    }

    #[tokio::test]
    async fn empty_srv_and_zero_port_are_refused() {
        for hosts in [vec![], vec![("node.example.test.", 0)]] {
            let dns = Dns::new(hosts, vec![], ResponseCode::NoError).await;
            assert!(dns.resolve(&mut params()).await.is_err());
        }
    }

    #[tokio::test]
    async fn txt_cannot_change_tls_routing_or_other_options() {
        for text in [
            "tls=false",
            "ssl=false",
            "directConnection=false",
            "authMechanism=PLAIN",
            "loadBalanced=true",
            "authSource=admin&replicaSet=a&tls=false",
            "authSource=",
            "authSource=a&authSource=b",
            "AuthSource=a",
            "authSource",
            "authSource=a\n",
        ] {
            let dns = Dns::new(
                vec![("node.example.test.", 27017)],
                vec![vec![text]],
                ResponseCode::NoError,
            )
            .await;
            let mut params = params();
            assert!(dns.resolve(&mut params).await.is_err(), "{text:?}");
            assert!(params.options.is_empty());
            assert!(params.host.starts_with(PREFIX));
        }
    }

    #[tokio::test]
    async fn multiple_txt_records_and_dns_failures_are_refused() {
        let dns = Dns::new(
            vec![("node.example.test.", 27017)],
            vec![vec!["authSource=a"], vec!["replicaSet=b"]],
            ResponseCode::NoError,
        )
        .await;
        assert!(dns.resolve(&mut params()).await.is_err());
        for code in [ResponseCode::ServFail, ResponseCode::Refused] {
            let dns = Dns::with_codes(
                vec![("node.example.test.", 27017)],
                vec![],
                ResponseCode::NoError,
                code,
            )
            .await;
            assert!(
                dns.resolve(&mut params()).await.is_err(),
                "TXT {code:?} must not be treated as absent"
            );
        }
        let dns = Dns::with_codes(
            vec![("node.example.test.", 27017)],
            vec![],
            ResponseCode::NoError,
            ResponseCode::NXDomain,
        )
        .await;
        dns.resolve(&mut params())
            .await
            .expect("an absent TXT is allowed");
        let dns = Dns::new(vec![], vec![], ResponseCode::ServFail).await;
        assert!(dns.resolve(&mut params()).await.is_err());
    }

    #[test]
    fn malformed_seed_urls_are_refused_without_echoing_credentials() {
        for host in [
            "mongodb+srv://user:secret@cluster.example.test",
            "mongodb+srv://cluster.example.test:27017",
            "mongodb+srv://a.example.test,b.example.test",
            "mongodb+srv://cluster.example.test/database",
            "mongodb+srv://cluster.example.test/?tls=false",
            "mongodb+srv://localhost",
            "mongodb+srv://127.0.0.1",
            "mongodb+srv://-bad.example.test",
            "mongodb+srv://cluster..test",
        ] {
            let mut params = params();
            params.host = host.into();
            assert!(seed(Engine::Mongodb, &params).is_err(), "{host}");
            assert!(
                !seed(Engine::Mongodb, &params)
                    .unwrap_err()
                    .to_string()
                    .contains("secret")
            );
        }
        assert!(seed(Engine::Postgres, &params()).is_err());
        let mut params = params();
        params.port = 12345;
        assert!(seed(Engine::Mongodb, &params).is_err());
    }

    #[tokio::test]
    async fn ordinary_hosts_are_unchanged_and_need_no_dns_resolver() {
        let mut params = params();
        params.host = "localhost".into();
        params.tls = TlsMode::Disable;
        resolve_host(Engine::Mongodb, &mut params).await.unwrap();
        assert_eq!(params.host, "localhost");
        assert_eq!(params.tls, TlsMode::Disable);
        assert!(params.options.is_empty());
    }

    #[tokio::test]
    async fn a_stalled_dns_server_is_bounded_by_the_connections_timeout() {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let config = config_at(socket.local_addr().unwrap());
        let resolver = TokioResolver::builder_with_config(config, TokioRuntimeProvider::default())
            .build()
            .unwrap();
        let mut params = params();
        params.connect_timeout = Some(Duration::from_millis(20));
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            resolve_bounded(&resolver, "cluster.example.test", &mut params),
        )
        .await
        .expect("DNS must not hang an open");
        assert!(matches!(result, Err(OpenError::Timeout { .. })));
        assert!(params.host.starts_with(PREFIX));
        assert!(params.options.is_empty());
    }
}
