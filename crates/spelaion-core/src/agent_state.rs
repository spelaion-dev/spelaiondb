//! What an agent is told about a call that did not do what it asked, and about a connection:
//! a stable code with the sentence beside it (ADR-060 point 2, SDB-374).
//!
//! The MCP server is a thin adapter: it hands on what this module decides. Every refusal and
//! failure gets a [`FailureCode`] and whether running the same call again makes sense, decided
//! here from the core's own error types, never from their text. A code carries no data: no
//! secret, no SQL, no path. Nothing is ever run again by the server; `retry` only tells the
//! agent whether it may.
//!
//! The rule for writes: a failed write never says it may be run again unless its transaction is
//! known to be rolled back or it never reached the server. A write whose session was lost may
//! have been committed.

use spelaion_config::ResolveError;
use spelaion_driver_api::DriverError;
use spelaion_ssh::{ProxyFailure, SshError, SsmFailure};

pub use spelaion_store::FailureCode;

use crate::kube::KubeError;
use crate::plan::PlanError;
use crate::{AgentAccess, OpenError, ReadError, ReadOnlyBecause, SetupError, WriteError};

/// Why a call failed, and whether running it again as it is may work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentFailure {
    code: FailureCode,
    retry: bool,
}

impl AgentFailure {
    const fn new(code: FailureCode, retry: bool) -> Self {
        Self { code, retry }
    }

    pub fn code(self) -> FailureCode {
        self.code
    }

    /// Whether running the same call again, unchanged, may work: the cause can pass on its own
    /// (a lost session is replaced at the next call, a network that did not answer may). False
    /// when only the user, or a different call, can change the answer.
    pub fn retry(self) -> bool {
        self.retry
    }

    /// The connection does not exist or is not exposed to agents, or the configuration exposes
    /// nothing. The code is the same in every case, so it never tells an agent that a hidden
    /// connection exists.
    pub const fn not_exposed() -> Self {
        Self::new(FailureCode::NotExposed, false)
    }

    /// A write on a connection agents may only read, whatever the reason.
    pub const fn read_only(_because: ReadOnlyBecause) -> Self {
        Self::new(FailureCode::ReadOnly, false)
    }

    /// The server accepts a read-only transaction without enforcing it (SDB-169).
    pub const fn engine_without_barrier() -> Self {
        Self::new(FailureCode::EngineWithoutBarrier, false)
    }

    /// The call's arguments do not go together.
    pub const fn invalid_arguments() -> Self {
        Self::new(FailureCode::InvalidArguments, false)
    }

    /// Something went wrong inside SpelaionDB (a panic, SDB-163). Running it again would most
    /// likely meet it again.
    pub const fn internal() -> Self {
        Self::new(FailureCode::Internal, false)
    }

    /// A connection that could not be resolved from its definition.
    pub fn from_resolve(error: &ResolveError) -> Self {
        match error {
            ResolveError::Untrusted { .. } | ResolveError::Changed { .. } => {
                Self::new(FailureCode::NotTrusted, false)
            }
            ResolveError::MissingVariables { .. }
            | ResolveError::Config(_)
            | ResolveError::Secret { .. } => Self::new(FailureCode::ConnectionUnavailable, false),
        }
    }

    /// A session that could not be opened. What may answer next time (the network, a tunnel,
    /// a server that did not answer in time) may be retried; what needs the user (a password,
    /// a host key, the configuration) may not. `plugin_unavailable` is
    /// [`crate::Engines::plugin_unavailable`] for the connection's engine: its driver then only
    /// says why its plugin is missing or refused, which is not a connection failure.
    pub fn from_open(error: &OpenError, plugin_unavailable: bool) -> Self {
        let unavailable = |retry| Self::new(FailureCode::ConnectionUnavailable, retry);
        if let OpenError::Driver(_) = error
            && plugin_unavailable
        {
            return Self::new(FailureCode::PluginMissing, false);
        }
        match error {
            OpenError::PasswordRequired { .. } => unavailable(false),
            OpenError::UnknownEngine(_) | OpenError::ExternalEngineMissing { .. } => {
                Self::new(FailureCode::PluginMissing, false)
            }
            OpenError::Tunnel(e) => Self::from_tunnel(e),
            OpenError::Kube(e) => match e {
                KubeError::Spawn { .. } | KubeError::Kubectl(_) => unavailable(false),
                KubeError::Timeout | KubeError::Down(_) => unavailable(true),
            },
            OpenError::Driver(e) => match e {
                DriverError::Connection(_) => unavailable(true),
                DriverError::Cancelled => Self::new(FailureCode::Cancelled, true),
                DriverError::Database { .. }
                | DriverError::Decode { .. }
                | DriverError::Other(_) => unavailable(false),
            },
            OpenError::Timeout { .. } => unavailable(true),
            OpenError::Setup(e) => match e {
                SetupError::Lost(_) | SetupError::Timeout { .. } => unavailable(true),
                SetupError::Refused { .. } => Self::new(FailureCode::RefusedByGuard, false),
                SetupError::ReadOnly { .. } => Self::engine_without_barrier(),
                SetupError::Failed { .. }
                | SetupError::Isolation { .. }
                | SetupError::Schema { .. }
                | SetupError::TransactionLeftOpen => unavailable(false),
            },
            OpenError::NoReadOnlyBarrier(_) | OpenError::FileFolder(_) => {
                Self::engine_without_barrier()
            }
            OpenError::Cleartext(_) => Self::new(FailureCode::RefusedByGuard, false),
        }
    }

    fn from_tunnel(error: &SshError) -> Self {
        let unavailable = |retry| Self::new(FailureCode::ConnectionUnavailable, retry);
        match error {
            SshError::HostKeyChanged { .. } | SshError::HostKeyRejected { .. } => {
                Self::new(FailureCode::NotTrusted, false)
            }
            SshError::Auth { .. } | SshError::Other(_) => unavailable(false),
            SshError::Cancelled => Self::new(FailureCode::Cancelled, true),
            SshError::Connect { .. }
            | SshError::Timeout { .. }
            | SshError::Target { .. }
            | SshError::Hop { .. } => unavailable(true),
            SshError::Ssm { failure, .. } => match failure {
                SsmFailure::Timeout => unavailable(true),
                SsmFailure::MissingCli
                | SsmFailure::MissingPlugin
                | SsmFailure::Invalid
                | SsmFailure::Exited => unavailable(false),
            },
            SshError::Proxy { failure, .. } => match failure {
                ProxyFailure::Unreachable | ProxyFailure::Target | ProxyFailure::Timeout => {
                    unavailable(true)
                }
                ProxyFailure::Rejected | ProxyFailure::Protocol => unavailable(false),
            },
        }
    }

    /// A read (`run_query`, `explain_query`) that failed. `lost` says whether its session was
    /// lost during it: the next call opens a new one, so the same read may work.
    pub fn from_read(error: &ReadError, lost: bool) -> Self {
        match error {
            ReadError::Rejected(_) => Self::new(FailureCode::RefusedByGuard, false),
            ReadError::Unsupported(_) => Self::engine_without_barrier(),
            ReadError::Timeout(_) | ReadError::Database(_) if lost => {
                Self::new(FailureCode::SessionLost, true)
            }
            // The same statement would meet the same limit: it has to be narrowed first.
            ReadError::Timeout(_) => Self::new(FailureCode::TimedOut, false),
            ReadError::Database(e) => Self::from_driver(e, lost),
        }
    }

    /// A failure of the driver during a call that only reads (a read, the schema tools).
    pub fn from_driver(error: &DriverError, lost: bool) -> Self {
        if lost {
            return Self::new(FailureCode::SessionLost, true);
        }
        match error {
            DriverError::Connection(_) => Self::new(FailureCode::ConnectionUnavailable, true),
            DriverError::Cancelled => Self::new(FailureCode::Cancelled, true),
            DriverError::Database { .. } | DriverError::Decode { .. } | DriverError::Other(_) => {
                Self::new(FailureCode::StatementFailed, false)
            }
        }
    }

    /// A write (`execute_write`) that failed. `lost` says whether its session was lost during
    /// it: then the commit may or may not have reached the server, and it is never said to be
    /// safe to run again. Otherwise its transaction was rolled back (`Session::run_write`).
    pub fn from_write(error: &WriteError, lost: bool) -> Self {
        match error {
            WriteError::Rejected(_) => Self::new(FailureCode::RefusedByGuard, false),
            WriteError::Unsupported(_) => Self::engine_without_barrier(),
            WriteError::Timeout(_) | WriteError::Database(_) if lost => {
                Self::new(FailureCode::SessionLost, false)
            }
            WriteError::Timeout(_) => Self::new(FailureCode::TimedOut, false),
            WriteError::Database(e) => match e {
                // A connection error the session has not noticed yet: whether the commit
                // arrived is not known either.
                DriverError::Connection(_) => Self::new(FailureCode::ConnectionUnavailable, false),
                // Cancelled before it committed, and rolled back.
                DriverError::Cancelled => Self::new(FailureCode::Cancelled, true),
                DriverError::Database { .. }
                | DriverError::Decode { .. }
                | DriverError::Other(_) => Self::new(FailureCode::StatementFailed, false),
            },
        }
    }

    /// A structured plan (`explain_query` with `structured`) that could not be made or read.
    /// `lost` says whether its session was lost while it ran: the next call opens a new one.
    pub fn from_plan(error: &PlanError, lost: bool) -> Self {
        if lost {
            return Self::new(FailureCode::SessionLost, true);
        }
        match error {
            // Refused before anything ran: a statement that cannot be explained, a limit.
            PlanError::Invalid(_) | PlanError::Limit(_) => {
                Self::new(FailureCode::RefusedByGuard, false)
            }
            // Not on this engine, or a plan no longer cached: a different call is needed.
            PlanError::Unsupported(_) | PlanError::Missing => Self::invalid_arguments(),
            PlanError::Execution(_) => Self::new(FailureCode::StatementFailed, false),
            PlanError::Io(_) => Self::internal(),
        }
    }
}

/// What agents may do on a visible connection, as stable words: `("read_write", None)` or
/// `("read_only", Some(why))`.
pub fn access_code(access: AgentAccess) -> (&'static str, Option<&'static str>) {
    match access {
        AgentAccess::ReadWrite => ("read_write", None),
        AgentAccess::ReadOnly(because) => (
            "read_only",
            Some(match because {
                ReadOnlyBecause::AllAgents => "all_agents",
                ReadOnlyBecause::ConnectionOverride => "connection_override",
                ReadOnlyBecause::Connection => "connection",
                ReadOnlyBecause::WritesDenied => "writes_denied",
                ReadOnlyBecause::ConfirmUnsupported => "confirm_unsupported",
                ReadOnlyBecause::EngineWithoutWrites => "engine_without_writes",
            }),
        ),
    }
}

/// Whether every agent is read-only (ADR-033): `spelaiondb mcp --read-only` (`flag`), or the
/// user's setting in the store. A setting that cannot be read makes every agent read-only. The
/// one place it is asked: by the MCP server on every call, and by the app's MCP page (which
/// never has a server's flag) so that both show the same answer.
pub fn all_agents_read_only<E>(flag: bool, setting: Result<bool, E>) -> bool {
    flag || setting.unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn driver_database() -> DriverError {
        DriverError::Database {
            code: Some("42P01".into()),
            message: "relation does not exist".into(),
            detail: None,
            hint: None,
            position: None,
        }
    }

    /// One case per code an agent can get, with whether it may run the call again.
    #[test]
    fn every_code_and_whether_it_may_be_run_again() {
        use FailureCode as C;
        let cases: Vec<(AgentFailure, FailureCode, bool)> = vec![
            (
                AgentFailure::from_read(&ReadError::Database(driver_database()), true),
                C::SessionLost,
                true,
            ),
            (
                AgentFailure::from_open(
                    &OpenError::Timeout {
                        after: Duration::from_secs(5),
                    },
                    false,
                ),
                C::ConnectionUnavailable,
                true,
            ),
            (AgentFailure::not_exposed(), C::NotExposed, false),
            (
                AgentFailure::from_open(
                    &OpenError::Tunnel(SshError::HostKeyRejected {
                        hop: 0,
                        host: "bastion".into(),
                    }),
                    false,
                ),
                C::NotTrusted,
                false,
            ),
            (
                AgentFailure::read_only(ReadOnlyBecause::AllAgents),
                C::ReadOnly,
                false,
            ),
            (
                AgentFailure::from_read(&ReadError::Rejected("a write".into()), false),
                C::RefusedByGuard,
                false,
            ),
            (
                AgentFailure::from_open(&OpenError::NoReadOnlyBarrier("RisingWave".into()), false),
                C::EngineWithoutBarrier,
                false,
            ),
            (
                AgentFailure::from_open(&OpenError::UnknownEngine("oracle".into()), false),
                C::PluginMissing,
                false,
            ),
            (
                AgentFailure::from_read(&ReadError::Database(DriverError::Cancelled), false),
                C::Cancelled,
                true,
            ),
            (
                AgentFailure::from_read(&ReadError::Timeout(Duration::from_secs(30)), false),
                C::TimedOut,
                false,
            ),
            (
                AgentFailure::from_read(&ReadError::Database(driver_database()), false),
                C::StatementFailed,
                false,
            ),
            (
                AgentFailure::invalid_arguments(),
                C::InvalidArguments,
                false,
            ),
            (AgentFailure::internal(), C::Internal, false),
        ];
        for (failure, code, retry) in &cases {
            assert_eq!(failure.code(), *code, "{failure:?}");
            assert_eq!(failure.retry(), *retry, "{failure:?}");
        }
        // Every written code has its case.
        for code in FailureCode::WRITTEN {
            assert!(
                cases.iter().any(|(_, c, _)| *c == code),
                "no case for {code:?}"
            );
        }
    }

    /// A write is never said to be safe to run again unless it is known to be rolled back.
    #[test]
    fn a_write_whose_session_was_lost_is_never_retried() {
        for error in [
            WriteError::Database(driver_database()),
            WriteError::Database(DriverError::Connection("reset".into())),
            WriteError::Database(DriverError::Cancelled),
            WriteError::Timeout(Duration::from_secs(30)),
        ] {
            let failure = AgentFailure::from_write(&error, true);
            assert_eq!(failure.code(), FailureCode::SessionLost, "{error:?}");
            assert!(!failure.retry(), "{error:?}");
        }
        // A connection error the session has not noticed: not known to be rolled back.
        let unnoticed = AgentFailure::from_write(
            &WriteError::Database(DriverError::Connection("x".into())),
            false,
        );
        assert!(!unnoticed.retry());
        // Cancelled before the commit: rolled back, so it may.
        let cancelled =
            AgentFailure::from_write(&WriteError::Database(DriverError::Cancelled), false);
        assert_eq!(cancelled.code(), FailureCode::Cancelled);
        assert!(cancelled.retry());
        let rejected = AgentFailure::from_write(&WriteError::Rejected("DROP".into()), false);
        assert_eq!(rejected.code(), FailureCode::RefusedByGuard);
        assert!(!rejected.retry());
    }

    /// What needs the user is never retried; what may answer next time is.
    #[test]
    fn opening_failures_that_need_the_user_are_not_retried() {
        let password =
            AgentFailure::from_open(&OpenError::PasswordRequired { save_to: None }, false);
        assert_eq!(password.code(), FailureCode::ConnectionUnavailable);
        assert!(!password.retry());
        let changed = AgentFailure::from_open(
            &OpenError::Tunnel(SshError::HostKeyChanged {
                hop: 0,
                host: "bastion".into(),
                file: "known_hosts".into(),
                line: 3,
            }),
            false,
        );
        assert_eq!(changed.code(), FailureCode::NotTrusted);
        assert!(!changed.retry());
        let refused = || OpenError::Driver(DriverError::Connection("refused".into()));
        let unreachable = AgentFailure::from_open(&refused(), false);
        assert_eq!(unreachable.code(), FailureCode::ConnectionUnavailable);
        assert!(unreachable.retry());
        // The same driver error from an engine whose plugin is missing or refused: its driver
        // only says why, and no retry brings the plugin back.
        let no_plugin = AgentFailure::from_open(&refused(), true);
        assert_eq!(no_plugin.code(), FailureCode::PluginMissing);
        assert!(!no_plugin.retry());
        let untrusted = AgentFailure::from_resolve(&ResolveError::Untrusted {
            root: "/repo".into(),
        });
        assert_eq!(untrusted.code(), FailureCode::NotTrusted);
    }

    #[test]
    fn every_agent_is_read_only_with_the_flag_or_without_a_readable_setting() {
        assert!(all_agents_read_only::<()>(true, Ok(false)));
        assert!(all_agents_read_only::<()>(false, Ok(true)));
        assert!(all_agents_read_only(false, Err("unreadable")));
        assert!(!all_agents_read_only::<()>(false, Ok(false)));
    }
}
