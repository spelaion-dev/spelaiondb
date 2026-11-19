//! The app's own lifecycles (SDB-377, ADR-060): quitting and switching folder, each one type
//! with its transitions here. Both are read from the window's event loop or from any command,
//! so they are atomics behind these types: nothing here ever waits (ADR-031).

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// Where quitting stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitPhase {
    /// Nobody asked to close the window, or the UI answered the last request by going back.
    Idle,
    /// `QuitRequested` was sent and the UI has not said it got it.
    Sent,
    /// The UI got the request and is checking its tabs or asking the user.
    Asking,
    /// The app is quitting: the user confirmed, or there was nothing to ask.
    Quitting,
}

impl QuitPhase {
    fn code(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Sent => 1,
            Self::Asking => 2,
            Self::Quitting => 3,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            1 => Self::Sent,
            2 => Self::Asking,
            3 => Self::Quitting,
            // Only `code` writes it.
            _ => Self::Idle,
        }
    }
}

/// What the window does with a close request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Let it close: the app quits.
    Close,
    /// Keep it open and send `QuitRequested`: the UI decides.
    AskTheUi,
}

/// The app's quitting, one [`QuitPhase`] changed only by the methods here.
#[derive(Debug)]
pub struct Quit(AtomicU8);

impl Default for Quit {
    fn default() -> Self {
        Self(AtomicU8::new(QuitPhase::Idle.code()))
    }
}

impl Quit {
    pub fn phase(&self) -> QuitPhase {
        QuitPhase::from_code(self.0.load(Ordering::SeqCst))
    }

    /// The user confirmed, or the UI found nothing to ask.
    pub fn quit(&self) {
        self.0.store(QuitPhase::Quitting.code(), Ordering::SeqCst);
    }

    /// The UI got `QuitRequested` and is deciding: a later close request asks it again.
    pub fn asking(&self) {
        self.unless_quitting(QuitPhase::Asking);
    }

    /// The user went back from the quit question: the next close request asks again.
    pub fn cancelled(&self) {
        self.unless_quitting(QuitPhase::Idle);
    }

    /// The window was asked to close. With no sessions nothing can be lost; a second request
    /// while the first was never acknowledged (the UI is frozen) quits too.
    pub fn close_requested(&self, no_sessions: bool) -> CloseDecision {
        let mut current = self.phase();
        loop {
            let (next, decision) = match current {
                QuitPhase::Quitting => return CloseDecision::Close,
                QuitPhase::Idle | QuitPhase::Sent | QuitPhase::Asking if no_sessions => {
                    (QuitPhase::Quitting, CloseDecision::Close)
                }
                QuitPhase::Sent => (QuitPhase::Quitting, CloseDecision::Close),
                QuitPhase::Idle | QuitPhase::Asking => (QuitPhase::Sent, CloseDecision::AskTheUi),
            };
            match self.0.compare_exchange(
                current.code(),
                next.code(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return decision,
                Err(seen) => current = QuitPhase::from_code(seen),
            }
        }
    }

    fn unless_quitting(&self, next: QuitPhase) {
        let mut current = self.phase();
        loop {
            match current {
                QuitPhase::Quitting => return,
                QuitPhase::Idle | QuitPhase::Sent | QuitPhase::Asking => {}
            }
            match self.0.compare_exchange(
                current.code(),
                next.code(),
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(seen) => current = QuitPhase::from_code(seen),
            }
        }
    }
}

/// A moment of the open folder: work started in it (a reload, a connection) compares it again
/// before it lands, so nothing started in the previous folder overwrites the new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderMoment(u64);

/// Why a folder switch could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderChanging;

impl std::fmt::Display for FolderChanging {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the folder is already changing")
    }
}

/// The open folder's generation. Settled or changing; while a switch runs it is changing, and
/// the switch's [`FolderSwitch`] guard settles it when dropped, also when the switch failed or
/// panicked, so the app can always change folder again.
#[derive(Debug, Default)]
pub struct Folder {
    /// Even: settled; odd: a switch is running. Only this type reads or writes it.
    generation: AtomicU64,
}

impl Folder {
    /// The current moment, settled or not.
    pub fn moment(&self) -> FolderMoment {
        FolderMoment(self.generation.load(Ordering::Acquire))
    }

    /// The current moment while no switch runs; `None` while the folder is changing.
    pub fn settled(&self) -> Option<FolderMoment> {
        let moment = self.moment();
        moment.0.is_multiple_of(2).then_some(moment)
    }

    /// Nothing changed since `moment`.
    pub fn still(&self, moment: FolderMoment) -> bool {
        self.moment() == moment
    }

    /// Starts a switch; refused while another one runs.
    pub fn begin_switch(&self) -> Result<FolderSwitch<'_>, FolderChanging> {
        let settled = self.settled().ok_or(FolderChanging)?;
        self.generation
            .compare_exchange(
                settled.0,
                settled.0 + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| FolderChanging)?;
        Ok(FolderSwitch { folder: self })
    }
}

/// A folder switch that is running. Dropping it settles the folder.
#[derive(Debug)]
pub struct FolderSwitch<'a> {
    folder: &'a Folder,
}

impl FolderSwitch<'_> {
    /// The path changed: work that saw the old path is stale. The folder stays changing.
    pub fn path_changed(&self) {
        self.folder.generation.fetch_add(2, Ordering::AcqRel);
    }
}

impl Drop for FolderSwitch<'_> {
    fn drop(&mut self) {
        self.folder.generation.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_close_request_quits_at_once_without_sessions_and_asks_the_ui_otherwise() {
        let quit = Quit::default();
        assert_eq!(quit.close_requested(true), CloseDecision::Close);
        assert_eq!(quit.phase(), QuitPhase::Quitting);
        // Once quitting, every request closes, whatever the UI says after.
        quit.cancelled();
        quit.asking();
        assert_eq!(quit.close_requested(false), CloseDecision::Close);

        let quit = Quit::default();
        assert_eq!(quit.close_requested(false), CloseDecision::AskTheUi);
        assert_eq!(quit.phase(), QuitPhase::Sent);
    }

    #[test]
    fn a_frozen_ui_does_not_hold_quitting_but_one_that_answered_is_asked_again() {
        // The UI never said it got the request: the second one quits.
        let quit = Quit::default();
        assert_eq!(quit.close_requested(false), CloseDecision::AskTheUi);
        assert_eq!(quit.close_requested(false), CloseDecision::Close);
        assert_eq!(quit.phase(), QuitPhase::Quitting);
        // The UI is asking the user: a later request asks it again.
        let quit = Quit::default();
        quit.close_requested(false);
        quit.asking();
        assert_eq!(quit.close_requested(false), CloseDecision::AskTheUi);
        // The user went back: the next request asks again too.
        quit.asking();
        quit.cancelled();
        assert_eq!(quit.phase(), QuitPhase::Idle);
        assert_eq!(quit.close_requested(false), CloseDecision::AskTheUi);
        quit.quit();
        assert_eq!(quit.phase(), QuitPhase::Quitting);
    }

    #[test]
    fn work_from_the_previous_folder_is_stale() {
        let folder = Folder::default();
        let before = folder.settled().unwrap();
        let switch = folder.begin_switch().unwrap();
        assert_eq!(folder.settled(), None, "changing");
        assert_eq!(folder.begin_switch().unwrap_err(), FolderChanging);
        let seen_old_path = folder.moment();
        switch.path_changed();
        assert!(!folder.still(seen_old_path));
        drop(switch);
        let after = folder.settled().unwrap();
        assert!(!folder.still(before));
        assert!(folder.still(after));
    }

    /// The reload in the middle of a switch panics: before, the folder stayed "changing" until
    /// the app restarted. The guard settles it, and the next switch starts (SDB-377).
    #[test]
    fn a_switch_that_panics_leaves_the_folder_settled_and_switchable() {
        let folder = Folder::default();
        let failed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let switch = folder.begin_switch().unwrap();
            switch.path_changed();
            panic!("the reload failed");
        }));
        assert!(failed.is_err());
        assert!(folder.settled().is_some(), "settled again after the panic");
        let again = folder.begin_switch().expect("the next switch starts");
        again.path_changed();
        drop(again);
        assert!(folder.settled().is_some());
    }

    /// A switch that ends with an error (an early return) settles the folder as well.
    #[test]
    fn a_switch_that_fails_leaves_the_folder_settled() {
        fn switch(folder: &Folder) -> Result<(), String> {
            let _switch = folder.begin_switch().map_err(|e| e.to_string())?;
            Err("could not read the folder".into())
        }
        let folder = Folder::default();
        assert!(switch(&folder).is_err());
        assert!(folder.settled().is_some());
        assert!(switch(&folder).is_err(), "it failed again, but it started");
    }
}
