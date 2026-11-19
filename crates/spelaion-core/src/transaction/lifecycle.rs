//! An editor session's transaction as one state (ADR-031, ADR-060): every combination the
//! type can hold is one the editor handles, and it only changes through the methods here. The
//! state still always comes from the driver, read after every statement ([`observe`]).
//!
//! [`observe`]: TransactionLifecycle::observe

use spelaion_config::Safeguards;
use spelaion_driver_api::TransactionState;

/// An editor session's transaction: the one place its state is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransactionLifecycle {
    /// The session does not follow its transaction: not an editor session, or an engine that
    /// cannot tell whether one is open. It only runs in auto-commit, as before ADR-031.
    #[default]
    NotFollowed,
    /// An editor session that follows its transaction.
    Followed {
        commit: CommitMode,
        /// What the driver last said.
        phase: TransactionPhase,
    },
}

/// How statements are committed in a session that follows its transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitMode {
    /// Each statement commits as it runs. With the connection type's `smart_commit`, the first
    /// statement that is not a read switches the session to manual commit.
    Auto { smart_commit: bool },
    /// Changes wait for Commit, as the user or the connection type chose. `smart_commit` is
    /// the connection type's, for when auto-commit is turned on again.
    Manual { smart_commit: bool },
    /// Smart commit turned manual commit on: when the transaction ends, the session returns to
    /// auto-commit with smart commit.
    SmartSwitched,
}

/// Whether a transaction is open, as the driver last said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransactionPhase {
    Idle,
    /// Open with work a Commit would keep. `statements`: the ones run in it, without
    /// transaction control and internal metadata reads.
    Open {
        statements: u32,
    },
    /// Open, but it can only be rolled back.
    Failed {
        statements: u32,
    },
}

impl TransactionPhase {
    fn of(state: TransactionState, statements: u32) -> Self {
        match state {
            TransactionState::Idle => Self::Idle,
            TransactionState::Open => Self::Open { statements },
            TransactionState::Failed => Self::Failed { statements },
        }
    }

    fn state(self) -> TransactionState {
        match self {
            Self::Idle => TransactionState::Idle,
            Self::Open { .. } => TransactionState::Open,
            Self::Failed { .. } => TransactionState::Failed,
        }
    }

    fn statements(self) -> u32 {
        match self {
            Self::Idle => 0,
            Self::Open { statements } | Self::Failed { statements } => statements,
        }
    }
}

impl CommitMode {
    fn auto_commit(self) -> bool {
        match self {
            Self::Auto { .. } => true,
            Self::Manual { .. } | Self::SmartSwitched => false,
        }
    }

    fn smart_commit(self) -> bool {
        match self {
            Self::Auto { smart_commit } | Self::Manual { smart_commit } => smart_commit,
            Self::SmartSwitched => true,
        }
    }
}

impl TransactionLifecycle {
    /// The starting state of an editor session under its connection type's safeguards.
    pub(crate) fn follow(safeguards: &Safeguards, state: TransactionState) -> Self {
        let smart_commit = safeguards.smart_commit;
        Self::Followed {
            commit: if safeguards.manual_commit {
                CommitMode::Manual { smart_commit }
            } else {
                CommitMode::Auto { smart_commit }
            },
            phase: TransactionPhase::of(state, 0),
        }
    }

    /// The session follows its transaction.
    pub fn is_followed(&self) -> bool {
        match self {
            Self::NotFollowed => false,
            Self::Followed { .. } => true,
        }
    }

    /// What the driver last said; `None` while it is not followed.
    pub fn state(&self) -> Option<TransactionState> {
        match self {
            Self::NotFollowed => None,
            Self::Followed { phase, .. } => Some(phase.state()),
        }
    }

    /// Statements run in the open transaction (0 when none is open).
    pub fn statements(&self) -> u32 {
        match self {
            Self::NotFollowed => 0,
            Self::Followed { phase, .. } => phase.statements(),
        }
    }

    /// Each statement commits as it runs (always so while it is not followed).
    pub fn auto_commit(&self) -> bool {
        match self {
            Self::NotFollowed => true,
            Self::Followed { commit, .. } => commit.auto_commit(),
        }
    }

    /// The connection type's smart commit.
    pub fn smart_commit(&self) -> bool {
        match self {
            Self::NotFollowed => false,
            Self::Followed { commit, .. } => commit.smart_commit(),
        }
    }

    /// Manual commit came from smart commit: it ends with the transaction.
    pub fn smart_switched(&self) -> bool {
        matches!(
            self,
            Self::Followed {
                commit: CommitMode::SmartSwitched,
                ..
            }
        )
    }

    /// A transaction is open with work that a Commit would keep.
    pub fn is_open(&self) -> bool {
        matches!(
            self,
            Self::Followed {
                phase: TransactionPhase::Open { .. },
                ..
            }
        )
    }

    /// A transaction is open, also one that can only be rolled back: Commit and Rollback
    /// apply to it.
    pub fn in_transaction(&self) -> bool {
        match self {
            Self::NotFollowed => false,
            Self::Followed { phase, .. } => match phase {
                TransactionPhase::Idle => false,
                TransactionPhase::Open { .. } | TransactionPhase::Failed { .. } => true,
            },
        }
    }

    /// An open transaction already holds statements the user ran.
    pub(crate) fn has_work(&self) -> bool {
        matches!(
            self,
            Self::Followed {
                phase: TransactionPhase::Open { statements: 1.. },
                ..
            }
        )
    }

    /// Auto-commit cannot be turned on now: manual commit with a transaction open, which the
    /// user commits or rolls back first.
    pub fn auto_commit_locked(&self) -> bool {
        !self.auto_commit() && self.in_transaction()
    }

    /// Commit and Rollback are offered: in manual commit, or while a transaction is open.
    pub fn end_actions_shown(&self) -> bool {
        self.is_followed() && (!self.auto_commit() || self.in_transaction())
    }

    /// The one rule for whether closing the session (closing its tab, switching its
    /// connection, disconnecting, quitting) must ask first, from this state: a transaction
    /// with work a Commit would keep is open. `Session::close_needs_confirmation` asks the
    /// driver first; the quit question reads the last state of the sessions still alive.
    pub fn closing_asks(&self) -> bool {
        self.is_open()
    }

    /// Takes the driver's answer: when no transaction is open any more, the count restarts and
    /// a session that smart commit switched returns to auto-commit.
    pub(crate) fn observe(&mut self, state: TransactionState) {
        let Self::Followed { commit, phase } = self else {
            return;
        };
        *phase = TransactionPhase::of(state, phase.statements());
        if *phase == TransactionPhase::Idle && *commit == CommitMode::SmartSwitched {
            *commit = CommitMode::Auto { smart_commit: true };
        }
    }

    /// Turns auto-commit on or off, as the user asked: a smart switch is forgotten, the
    /// connection type's smart commit is kept.
    pub(crate) fn set_auto_commit(&mut self, on: bool) {
        let Self::Followed { commit, .. } = self else {
            return;
        };
        let smart_commit = commit.smart_commit();
        *commit = if on {
            CommitMode::Auto { smart_commit }
        } else {
            CommitMode::Manual { smart_commit }
        };
    }

    /// Smart commit switches the session to manual right before a write. `false`, and nothing
    /// changes, unless it is in auto-commit with smart commit.
    pub(crate) fn switch_by_smart_commit(&mut self) -> bool {
        match self {
            Self::Followed {
                commit: commit @ CommitMode::Auto { smart_commit: true },
                ..
            } => {
                *commit = CommitMode::SmartSwitched;
                true
            }
            Self::NotFollowed | Self::Followed { .. } => false,
        }
    }

    /// The core's own `BEGIN` opened a transaction: open, with nothing run in it yet.
    pub(crate) fn opened_by_core(&mut self) {
        if let Self::Followed { phase, .. } = self {
            *phase = TransactionPhase::Open { statements: 0 };
        }
    }

    /// The open transaction starts counting again (MySQL's `BEGIN` committed the previous one).
    pub(crate) fn restart_count(&mut self) {
        if let Self::Followed {
            phase: TransactionPhase::Open { statements },
            ..
        } = self
        {
            *statements = 0;
        }
    }

    /// One more statement the user ran in the open transaction.
    pub(crate) fn count_statement(&mut self) {
        if let Self::Followed {
            phase: TransactionPhase::Open { statements },
            ..
        } = self
        {
            *statements += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransactionStatus;

    fn followed(commit: CommitMode, phase: TransactionPhase) -> TransactionLifecycle {
        TransactionLifecycle::Followed { commit, phase }
    }

    fn manual(phase: TransactionPhase) -> TransactionLifecycle {
        followed(
            CommitMode::Manual {
                smart_commit: false,
            },
            phase,
        )
    }

    fn auto(phase: TransactionPhase) -> TransactionLifecycle {
        followed(
            CommitMode::Auto {
                smart_commit: false,
            },
            phase,
        )
    }

    #[test]
    fn a_smart_switch_ends_with_its_transaction() {
        let mut status = followed(
            CommitMode::SmartSwitched,
            TransactionPhase::Open { statements: 2 },
        );
        status.observe(TransactionState::Open);
        assert!(!status.auto_commit() && status.statements() == 2);
        status.observe(TransactionState::Idle);
        assert!(status.auto_commit() && !status.smart_switched() && status.statements() == 0);
        assert!(
            status.smart_commit(),
            "smart commit stays on for the next write"
        );
        // A manual mode the user chose stays manual.
        let mut chosen = followed(
            CommitMode::Manual { smart_commit: true },
            TransactionPhase::Open { statements: 2 },
        );
        chosen.observe(TransactionState::Idle);
        assert!(!chosen.auto_commit());
    }

    /// What each state shows, field by field, as the struct of six fields showed it before
    /// ADR-060: the view reads exactly as it did.
    #[test]
    fn the_view_shows_each_state_as_the_six_fields_did() {
        let view = |l: TransactionLifecycle| {
            let s = TransactionStatus::from(l);
            (
                s.supported,
                s.auto_commit,
                s.smart_commit,
                s.smart_switched,
                s.state,
                s.statements,
            )
        };
        assert_eq!(
            view(TransactionLifecycle::NotFollowed),
            (false, true, false, false, None, 0)
        );
        assert_eq!(
            TransactionStatus::default(),
            TransactionLifecycle::NotFollowed.into()
        );
        assert_eq!(
            view(followed(
                CommitMode::Auto { smart_commit: true },
                TransactionPhase::Idle
            )),
            (true, true, true, false, Some(TransactionState::Idle), 0)
        );
        assert_eq!(
            view(manual(TransactionPhase::Open { statements: 3 })),
            (true, false, false, false, Some(TransactionState::Open), 3)
        );
        assert_eq!(
            view(followed(
                CommitMode::SmartSwitched,
                TransactionPhase::Failed { statements: 1 }
            )),
            (true, false, true, true, Some(TransactionState::Failed), 1)
        );
    }

    #[test]
    fn the_safeguards_choose_the_starting_mode() {
        let mut safeguards = Safeguards::default();
        assert_eq!(
            TransactionLifecycle::follow(&safeguards, TransactionState::Idle),
            auto(TransactionPhase::Idle)
        );
        safeguards.manual_commit = true;
        safeguards.smart_commit = true;
        assert_eq!(
            TransactionLifecycle::follow(&safeguards, TransactionState::Open),
            followed(
                CommitMode::Manual { smart_commit: true },
                TransactionPhase::Open { statements: 0 }
            )
        );
    }

    #[test]
    fn smart_commit_switches_only_an_auto_commit_session_that_asks_for_it() {
        let mut plain = auto(TransactionPhase::Idle);
        assert!(!plain.switch_by_smart_commit());
        let mut chosen = followed(
            CommitMode::Manual { smart_commit: true },
            TransactionPhase::Idle,
        );
        assert!(!chosen.switch_by_smart_commit());
        let mut not_followed = TransactionLifecycle::NotFollowed;
        assert!(!not_followed.switch_by_smart_commit());
        let mut smart = followed(
            CommitMode::Auto { smart_commit: true },
            TransactionPhase::Idle,
        );
        assert!(smart.switch_by_smart_commit());
        assert!(smart.smart_switched() && !smart.auto_commit());
        // The user's switch forgets the smart switch and keeps the type's smart commit.
        smart.set_auto_commit(false);
        assert_eq!(
            smart,
            followed(
                CommitMode::Manual { smart_commit: true },
                TransactionPhase::Idle
            )
        );
        smart.set_auto_commit(true);
        assert_eq!(
            smart,
            followed(
                CommitMode::Auto { smart_commit: true },
                TransactionPhase::Idle
            )
        );
        not_followed.set_auto_commit(false);
        assert_eq!(not_followed, TransactionLifecycle::NotFollowed);
    }

    #[test]
    fn statements_are_counted_only_in_an_open_transaction() {
        let mut status = manual(TransactionPhase::Idle);
        status.count_statement();
        assert_eq!(status.statements(), 0, "nothing is open: nothing to count");
        status.opened_by_core();
        assert!(status.is_open() && !status.has_work());
        status.count_statement();
        status.count_statement();
        assert!(status.has_work());
        assert_eq!(status.statements(), 2);
        // A failed transaction keeps its count; it can only be rolled back.
        status.observe(TransactionState::Failed);
        assert_eq!(status.statements(), 2);
        assert!(!status.is_open() && status.in_transaction() && !status.has_work());
        status.count_statement();
        assert_eq!(status.statements(), 2);
        status.observe(TransactionState::Open);
        status.restart_count();
        assert_eq!(status.statements(), 0);
        status.observe(TransactionState::Idle);
        assert_eq!(status.state(), Some(TransactionState::Idle));
        // A session that does not follow its transaction takes nothing.
        let mut not_followed = TransactionLifecycle::NotFollowed;
        not_followed.observe(TransactionState::Open);
        not_followed.opened_by_core();
        assert_eq!(not_followed, TransactionLifecycle::NotFollowed);
    }

    #[test]
    fn closing_asks_only_about_an_open_transaction_with_work_to_keep() {
        assert!(manual(TransactionPhase::Open { statements: 0 }).closing_asks());
        assert!(manual(TransactionPhase::Open { statements: 4 }).closing_asks());
        assert!(auto(TransactionPhase::Open { statements: 1 }).closing_asks());
        assert!(!manual(TransactionPhase::Failed { statements: 4 }).closing_asks());
        assert!(!manual(TransactionPhase::Idle).closing_asks());
        assert!(!TransactionLifecycle::NotFollowed.closing_asks());
    }

    #[test]
    fn the_controls_read_what_the_core_answers() {
        let idle = auto(TransactionPhase::Idle);
        assert!(!idle.end_actions_shown() && !idle.auto_commit_locked());
        // A BEGIN typed in auto-commit: Commit and Rollback apply, the switch is not locked.
        let typed = auto(TransactionPhase::Open { statements: 1 });
        assert!(typed.end_actions_shown() && !typed.auto_commit_locked());
        let manual_idle = manual(TransactionPhase::Idle);
        assert!(manual_idle.end_actions_shown() && !manual_idle.auto_commit_locked());
        let manual_failed = manual(TransactionPhase::Failed { statements: 1 });
        assert!(manual_failed.end_actions_shown() && manual_failed.auto_commit_locked());
        assert!(!TransactionLifecycle::NotFollowed.end_actions_shown());
        assert!(!TransactionLifecycle::NotFollowed.auto_commit_locked());
    }
}
