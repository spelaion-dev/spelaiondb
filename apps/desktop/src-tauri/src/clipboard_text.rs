//! Reading the clipboard's text on Linux. WebKitGTK's `navigator.clipboard.readText()` asks the
//! clipboard's owner only for `text/plain;charset=utf-8` (and decodes plain `text/plain` as
//! Latin-1): on X11, text from a program that offers only `UTF8_STRING` or `STRING` (xterm,
//! urxvt, xclip's and xsel's default) reads as "". GTK's own `gtk_clipboard_request_text`, on
//! the same display connection as the window, asks for every text target (`UTF8_STRING`,
//! `text/plain;charset=utf-8`, `STRING`, `TEXT`, `COMPOUND_TEXT`, …) and converts it to UTF-8,
//! under X11 and Wayland alike. GTK is already how the app shows its window and its file
//! dialogs (`file_dialog.rs`); a separate clipboard crate (arboard) would open its own X11 or
//! Wayland connection, and needs a compositor with wlr-data-control on Wayland.
//!
//! The app calls this only on Linux (`src/clipboardText.ts`) and only while handling a click or
//! a key ("Paste from clipboard", Ctrl+Alt+V, "Paste connection", the grid's Paste), as it read
//! `navigator.clipboard` before. The text stays untrusted input: it goes back to the page as it
//! is and reaches the core only through the commands that check it (ADR-030, ADR-038). Windows
//! and macOS keep `navigator.clipboard.readText()`; anywhere else this answers with an error.

use crate::CmdResult;

/// The clipboard's text, `None` when it holds none (empty, an image, no owner).
#[tauri::command]
#[specta::specta]
pub(crate) async fn read_clipboard_text(window: tauri::WebviewWindow) -> CmdResult<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        linux::read(window).await
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = window;
        Err("the clipboard is read through the webview here".to_string())
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use tokio::sync::oneshot;

    use crate::CmdResult;

    pub(super) async fn read(window: tauri::WebviewWindow) -> CmdResult<Option<String>> {
        let (answer, answered) = oneshot::channel();
        // GTK's clipboard belongs to its main thread. `request_text` answers from GTK's main
        // loop once the owner has sent the text, without blocking that loop meanwhile.
        window
            .run_on_main_thread(move || {
                let clipboard = gtk::Clipboard::get(&gtk::gdk::SELECTION_CLIPBOARD);
                clipboard.request_text(move |_, text| {
                    let _ = answer.send(text.map(str::to_owned));
                });
            })
            .map_err(|e| e.to_string())?;
        answered
            .await
            .map_err(|_| "the clipboard did not answer".to_string())
    }
}
