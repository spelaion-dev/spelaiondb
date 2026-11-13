//! The file dialogs on Linux. tauri-plugin-dialog opens GTK's file chooser through rfd, which
//! never gives it a parent window there: a tiling Wayland compositor then shows the dialog as
//! one more ordinary window beside the app instead of floating it over the app. GTK also leaves
//! the file's name alone when the user picks another file type, and does not add the type's
//! extension to a name typed without one. This command opens the same `GtkFileChooserNative`
//! the plugin does, transient for (and modal over) the window that asked, starting in the
//! folder the app names, and makes the saved name agree with the chosen type when the user
//! saves (`spelaion_core::renamed_for_type`).
//!
//! Windows and macOS keep the plugin's dialogs, which already have their parent: the app only
//! calls this on Linux (`src/dialogs.ts`), and anywhere else it answers with an error.

use serde::Deserialize;
use specta::Type;

use crate::CmdResult;

/// What a file dialog picks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub enum PathDialogKind {
    /// An existing file.
    Open,
    /// Where to save a file.
    Save,
    /// A folder.
    Folder,
}

/// A file type a dialog offers: `*` stands for every file.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct PathDialogFilter {
    pub name: String,
    pub extensions: Vec<String>,
}

/// A file dialog the app asks for.
#[derive(Debug, Clone, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct PathDialogRequest {
    pub kind: PathDialogKind,
    pub title: Option<String>,
    /// Folders the dialog may start in: the first that exists wins (without one, GTK's own
    /// choice: "Recent").
    pub folders: Vec<String>,
    /// The name a save dialog proposes.
    pub file_name: Option<String>,
    /// The file types, the first one chosen unless `file_name` has another's extension.
    pub filters: Vec<PathDialogFilter>,
}

/// Shows a file dialog over the window that asked and answers the path picked, or `None` when
/// the user cancelled. Only on Linux.
#[tauri::command]
#[specta::specta]
pub(crate) async fn choose_path(
    window: tauri::WebviewWindow,
    request: PathDialogRequest,
) -> CmdResult<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        linux::choose(window, request).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (window, request);
        Err("this file dialog is only used on Linux".to_string())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;

    use gtk::prelude::*;
    use gtk::{FileChooserAction, FileChooserNative, FileFilter, ResponseType};
    use spelaion_core::{renamed_for_type, type_extension, type_pattern};
    use tokio::sync::oneshot;

    use super::{PathDialogKind, PathDialogRequest};
    use crate::CmdResult;

    /// The dialog's file types, each with the extension it stands for (none for every file).
    type Types = Rc<Vec<(FileFilter, Option<String>)>>;

    pub(super) async fn choose(
        window: tauri::WebviewWindow,
        request: PathDialogRequest,
    ) -> CmdResult<Option<String>> {
        let (answer, answered) = oneshot::channel();
        let parent = window.clone();
        window
            .run_on_main_thread(move || show(parent.gtk_window().ok(), request, answer))
            .map_err(|e| e.to_string())?;
        // A dialog that went away without answering counts as cancelled.
        let Ok(Some(path)) = answered.await else {
            return Ok(None);
        };
        path.into_os_string()
            .into_string()
            .map(Some)
            .map_err(|path| format!("{}: not a UTF-8 path", path.to_string_lossy()))
    }

    /// Builds and shows the dialog (on GTK's thread); `answer` gets the path once it closes.
    fn show(
        parent: Option<gtk::ApplicationWindow>,
        request: PathDialogRequest,
        answer: oneshot::Sender<Option<PathBuf>>,
    ) {
        // rfd's titles when the app gives none, and GTK's own (translated) button labels.
        let (action, default_title) = match request.kind {
            PathDialogKind::Open => (FileChooserAction::Open, "Open File"),
            PathDialogKind::Save => (FileChooserAction::Save, "Save File"),
            PathDialogKind::Folder => (FileChooserAction::SelectFolder, "Select Folder"),
        };
        let title = request.title.as_deref().unwrap_or(default_title);
        let dialog = FileChooserNative::new(Some(title), parent.as_ref(), action, None, None);
        // A dialog with a parent is what makes a compositor float it over the app.
        dialog.set_modal(true);
        if let Some(folder) = request.folders.iter().find(|f| Path::new(f).is_dir()) {
            dialog.set_current_folder(folder);
        }
        let types: Types = Rc::new(
            request
                .filters
                .iter()
                .map(|filter| {
                    let gtk_filter = FileFilter::new();
                    gtk_filter.set_name(Some(&filter.name));
                    for extension in &filter.extensions {
                        gtk_filter.add_pattern(&type_pattern(extension));
                    }
                    dialog.add_filter(gtk_filter.clone());
                    (
                        gtk_filter,
                        type_extension(&filter.extensions).map(str::to_owned),
                    )
                })
                .collect(),
        );
        let save = request.kind == PathDialogKind::Save;
        if save {
            dialog.set_do_overwrite_confirmation(true);
            if let Some(name) = &request.file_name {
                dialog.set_current_name(name);
                let has = |extension: &str| {
                    Path::new(name)
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case(extension))
                };
                if let Some((filter, _)) = types
                    .iter()
                    .find(|(_, extension)| extension.as_deref().is_some_and(has))
                {
                    dialog.set_filter(filter);
                }
            }
        }

        // GTK does not keep a native dialog alive: this reference does until it answers.
        let kept = RefCell::new(Some(dialog.clone()));
        let answer = RefCell::new(Some(answer));
        dialog.connect_response(move |dialog, response| {
            let mut path = None;
            if response == ResponseType::Accept {
                path = dialog.filename();
                if save
                    && let Some(picked) = &path
                    && let Some(typed) = saved_name(dialog, picked, &types)
                {
                    // GTK asked about replacing the name as typed, not this one: it shows the
                    // full name again instead, and asks with the next Save if it still exists.
                    if typed.symlink_metadata().is_ok()
                        && let (Some(folder), Some(name)) =
                            (typed.parent(), typed.file_name().and_then(|n| n.to_str()))
                    {
                        dialog.set_current_folder(folder);
                        dialog.set_current_name(name);
                        dialog.show();
                        return;
                    }
                    path = Some(typed);
                }
            }
            if let Some(answer) = answer.borrow_mut().take() {
                let _ = answer.send(path);
            }
            drop(kept.borrow_mut().take());
        });
        dialog.show();
    }

    /// The path a save dialog answered, renamed to agree with the chosen type when it does not
    /// (`None` when it does): an extension of another of the dialog's types becomes the chosen
    /// one's, and a name without one gets it. Only now: a native dialog does not tell when its
    /// type changes while it is up (neither GTK's own nor a portal's).
    fn saved_name(dialog: &FileChooserNative, picked: &Path, types: &Types) -> Option<PathBuf> {
        let extension = chosen_extension(dialog, types)?;
        let name = picked.file_name()?.to_str()?;
        renamed_for_type(name, &extension, &known(types)).map(|name| picked.with_file_name(name))
    }

    /// The extension of the type chosen in the dialog, if it stands for one.
    fn chosen_extension(dialog: &FileChooserNative, types: &Types) -> Option<String> {
        let chosen = dialog.filter()?;
        types
            .iter()
            .find(|(filter, _)| *filter == chosen)
            .and_then(|(_, extension)| extension.clone())
    }

    /// The extensions the dialog's types stand for.
    fn known(types: &Types) -> Vec<&str> {
        types
            .iter()
            .filter_map(|(_, extension)| extension.as_deref())
            .collect()
    }
}
