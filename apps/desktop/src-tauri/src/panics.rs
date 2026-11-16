//! A command that panics answers the page with an error (SDB-163): a panic is a bug, written to
//! the app log by the core's panic hook (`spelaion_core::install_panic_log`), and the tab that
//! waited is told the operation failed and can be used again. Async commands go through
//! [`guard`] (`#[command]`, `spelaion-desktop-macros`); sync ones run inside the invoke handler,
//! which catches them with [`catch_sync`].

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::OnceLock;

use spelaion_core::AppLog;

use crate::CmdResult;

/// Where the app log is, to tell the user.
static LOG: OnceLock<AppLog> = OnceLock::new();

/// Sends every panic's place to `log` from now on.
pub(crate) fn install(log: AppLog) {
    spelaion_core::install_panic_log(Some(log.clone()));
    let _ = LOG.set(log);
}

/// The error a command answers when it panicked.
pub(crate) fn internal_error(command: &str) -> String {
    let log = LOG
        .get()
        .map(|log| format!(" ({})", log.path().display()))
        .unwrap_or_default();
    format!(
        "{}{log}. The command was {command}.",
        spelaion_core::INTERNAL_ERROR
    )
}

/// Runs an async command's body: a panic becomes its error.
pub(crate) async fn guard<T>(
    command: &'static str,
    body: impl Future<Output = CmdResult<T>>,
) -> CmdResult<T> {
    spelaion_core::catch_panic(body)
        .await
        .unwrap_or_else(|_| Err(internal_error(command)))
}

/// Runs the invoke handler for one call: a sync command that panics is answered with an error
/// (`reject`) instead of unwinding into the webview, which would end the app.
pub(crate) fn catch_sync<R: tauri::Runtime>(
    invoke: tauri::ipc::Invoke<R>,
    handle: impl FnOnce(tauri::ipc::Invoke<R>) -> bool,
) -> bool {
    let resolver = invoke.resolver.clone();
    let command = invoke.message.command().to_string();
    match std::panic::catch_unwind(AssertUnwindSafe(|| handle(invoke))) {
        Ok(handled) => handled,
        Err(_) => {
            // Answering cannot itself be allowed to unwind into the webview.
            let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
                resolver.reject(internal_error(&command))
            }));
            true
        }
    }
}

/// Commands that panic, through Tauri's own invoke path (its mock runtime). Before SDB-163 an
/// async command that panicked never answered (the page's promise waited for ever) and a sync
/// one unwound into the webview's callback, which ends the app.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use spelaion_desktop_macros::command;
    use tauri::ipc::{CallbackFn, InvokeBody, InvokeResponseBody};
    use tauri::test::{INVOKE_KEY, MockRuntime, get_ipc_response, mock_builder, mock_context};
    use tauri::webview::InvokeRequest;

    use crate::CmdResult;

    #[command]
    async fn async_panics() -> CmdResult<u32> {
        if std::hint::black_box(true) {
            panic!("an async command panicked");
        }
        Ok(1)
    }

    #[command]
    fn sync_panics() -> CmdResult<u32> {
        if std::hint::black_box(true) {
            panic!("a sync command panicked");
        }
        Ok(2)
    }

    #[command]
    async fn answers() -> CmdResult<u32> {
        Ok(7)
    }

    fn webview() -> tauri::WebviewWindow<MockRuntime> {
        let app = mock_builder()
            .invoke_handler(|invoke| {
                super::catch_sync(
                    invoke,
                    tauri::generate_handler![async_panics, sync_panics, answers],
                )
            })
            .build(mock_context(tauri::test::noop_assets()))
            .expect("the mock app");
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("the mock window");
        // The app must outlive its window for the test.
        std::mem::forget(app);
        webview
    }

    /// Invokes `command` and waits for its answer, at most 10 seconds.
    fn invoke(
        webview: &tauri::WebviewWindow<MockRuntime>,
        command: &str,
    ) -> Result<InvokeResponseBody, serde_json::Value> {
        let webview = webview.clone();
        let request = InvokeRequest {
            cmd: command.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body: InvokeBody::default(),
            headers: Default::default(),
            invoke_key: INVOKE_KEY.to_string(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(get_ipc_response(&webview, request));
        });
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("{command} never answered"))
    }

    fn assert_internal_error(answer: Result<InvokeResponseBody, serde_json::Value>, command: &str) {
        let error = answer.expect_err("a panic is an error");
        let text = error.as_str().unwrap_or_default();
        assert!(text.contains("internal error"), "{error}");
        assert!(text.contains(command), "{error}");
        assert!(
            !text.contains("panicked"),
            "the message is never shown: {error}"
        );
    }

    #[test]
    fn commands_that_panic_answer_with_an_error_and_the_app_keeps_working() {
        let webview = webview();
        assert_internal_error(invoke(&webview, "async_panics"), "async_panics");
        assert_internal_error(invoke(&webview, "sync_panics"), "sync_panics");
        // The app goes on answering.
        match invoke(&webview, "answers") {
            Ok(InvokeResponseBody::Json(json)) => assert_eq!(json, "7"),
            other => panic!("{other:?}"),
        }
    }

    /// A command whose body keeps a lot across an await.
    #[command]
    async fn big_body() -> CmdResult<u8> {
        let held = std::hint::black_box([7u8; 256 * 1024]);
        std::future::ready(()).await;
        Ok(held[held.len() - 1])
    }

    /// Tauri builds a command's future on the main thread, whose stack is 1 MB on Windows, and
    /// moves it before spawning it: `#[command]` keeps the body on the heap, so that future
    /// stays small however big the body is. Inline, the smoke test's app overflowed the main
    /// thread's stack.
    #[test]
    fn a_commands_future_stays_small_however_big_its_body() {
        let future = big_body();
        assert!(
            std::mem::size_of_val(&future) < 1024,
            "{} bytes",
            std::mem::size_of_val(&future)
        );
    }
}
