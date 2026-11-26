//! The native dialog the app shows when it cannot start (SDB-283): its own window may not exist
//! yet, so it is the system's message box. What it says and offers comes from the core
//! (`spelaion_core::StartupFailure`); this only shows it, opens a folder and reports the choice.
//!
//! It is always shown on the main thread: before Tauri starts, or from its `setup`. On Linux it
//! is GTK's own dialog, initialized through gtk-rs on that thread, the one Tauri then uses (rfd
//! would start GTK on a thread of its own, which Tauri's GTK could not share).

use std::path::Path;
use std::time::SystemTime;

use spelaion_core::{AppLog, LogLevel, StartupAction, StartupDialog, StartupFailure};

/// The smoke test (`e2e/smoke.mjs`) answers the dialog with this instead of a click, so its
/// window never appears on the user's screen: `start-over` or `quit`. The failure is still
/// logged. Without it the dialog is always shown.
const CHOICE_SWITCH: &str = "SPELAION_E2E_STARTUP_CHOICE";

/// Writes `failure` to the log and asks the user what to do until they choose something other
/// than opening the folder.
pub(crate) fn ask(failure: &StartupFailure, log: &AppLog) -> StartupAction {
    let entry = failure.log_entry();
    // Also on the console, for a start from a terminal.
    eprintln!("{entry}");
    let written = match log.append(LogLevel::Error, &entry) {
        Ok(()) => Some(log.path()),
        Err(e) => {
            eprintln!("could not write the log {}: {e}", log.path().display());
            None
        }
    };
    let dialog = failure.dialog(written, SystemTime::now());
    if let Some(choice) = std::env::var_os(CHOICE_SWITCH) {
        let action = match choice.to_str() {
            Some("start-over") if dialog.actions.contains(&StartupAction::StartOver) => {
                StartupAction::StartOver
            }
            _ => StartupAction::Quit,
        };
        note(
            log,
            LogLevel::Info,
            &format!(
                "startup dialog answered by {CHOICE_SWITCH}: {}",
                action.label()
            ),
        );
        return action;
    }
    loop {
        match show(&dialog) {
            StartupAction::OpenFolder => {
                if let Some(folder) = &dialog.folder
                    && let Err(e) = open_folder(folder)
                {
                    note(
                        log,
                        LogLevel::Warning,
                        &format!("could not open {}: {e}", folder.display()),
                    );
                }
            }
            action @ (StartupAction::Quit | StartupAction::StartOver) => return action,
        }
    }
}

/// Tells the user that what they chose could not be done (it is logged too), and quits: starting
/// SpelaionDB again shows the first dialog again.
pub(crate) fn fail(log: &AppLog, message: &str) -> ! {
    note(log, LogLevel::Error, message);
    if std::env::var_os(CHOICE_SWITCH).is_none() {
        show(&StartupDialog {
            title: "SpelaionDB could not start".into(),
            body: format!(
                "{message}\n\nThis error was also written to {}.",
                log.path().display()
            ),
            actions: vec![StartupAction::Quit],
            folder: None,
        });
    }
    std::process::exit(1);
}

/// Adds `message` to the log at `level`, and to the console.
pub(crate) fn note(log: &AppLog, level: LogLevel, message: &str) {
    eprintln!("{message}");
    if let Err(e) = log.append(level, message) {
        eprintln!("could not write the log {}: {e}", log.path().display());
    }
}

/// Shows the dialog and returns the action chosen. Closing it, or anything unexpected, is Quit,
/// so a choice that changes something is only ever a click on its own button. Quit is first and
/// has the focus.
#[cfg(target_os = "linux")]
fn show(dialog: &StartupDialog) -> StartupAction {
    use gtk::prelude::*;
    use gtk::{ButtonsType, DialogFlags, MessageDialog, MessageType, ResponseType};
    // Without a display there is no dialog to show: the console and the log have the error.
    if gtk::init().is_err() {
        return StartupAction::Quit;
    }
    let message = MessageDialog::new(
        None::<&gtk::Window>,
        DialogFlags::MODAL,
        MessageType::Error,
        ButtonsType::None,
        &dialog.title,
    );
    message.set_secondary_text(Some(&dialog.body));
    for (i, action) in dialog.actions.iter().enumerate() {
        message.add_button(action.label(), ResponseType::Other(i as u16));
    }
    message.set_default_response(ResponseType::Other(0));
    let response = message.run();
    message.hide();
    while gtk::events_pending() {
        gtk::main_iteration();
    }
    match response {
        ResponseType::Other(i) => dialog
            .actions
            .get(usize::from(i))
            .copied()
            .unwrap_or(StartupAction::Quit),
        _ => StartupAction::Quit,
    }
}

/// Shows the dialog and returns the action chosen. Closing it, or anything unexpected, is Quit,
/// so a choice that changes something is only ever a click on its own button. Quit is first and
/// has the focus.
#[cfg(not(target_os = "linux"))]
fn show(dialog: &StartupDialog) -> StartupAction {
    use rfd::{MessageButtons, MessageDialog, MessageDialogResult, MessageLevel};
    let labels: Vec<String> = dialog
        .actions
        .iter()
        .map(|a| a.label().to_string())
        .collect();
    let buttons = match labels.as_slice() {
        [a, b, c] => MessageButtons::YesNoCancelCustom(a.clone(), b.clone(), c.clone()),
        [a, b] => MessageButtons::OkCancelCustom(a.clone(), b.clone()),
        _ => MessageButtons::OkCustom(StartupAction::Quit.label().into()),
    };
    let result = MessageDialog::new()
        .set_level(MessageLevel::Error)
        .set_title(&dialog.title)
        .set_description(&dialog.body)
        .set_buttons(buttons)
        .show();
    match result {
        MessageDialogResult::Custom(label) => dialog
            .actions
            .iter()
            .copied()
            .find(|a| a.label() == label)
            .unwrap_or(StartupAction::Quit),
        MessageDialogResult::Yes
        | MessageDialogResult::No
        | MessageDialogResult::Ok
        | MessageDialogResult::Cancel => StartupAction::Quit,
    }
}

/// Opens `folder` in the system's file manager, without waiting for it.
fn open_folder(folder: &Path) -> std::io::Result<()> {
    let program = if cfg!(windows) {
        "explorer.exe"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program)
        .arg(folder)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    // The file manager may stay with what it opened: reap it without waiting here.
    std::thread::spawn(move || child.wait());
    Ok(())
}
