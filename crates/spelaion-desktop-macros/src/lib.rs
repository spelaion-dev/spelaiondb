//! `#[command]`: the desktop app's Tauri commands (SDB-163).
//!
//! Tauri runs an async command in a task of its own and answers the page when it ends. If the
//! command panics, the task dies without answering and the page waits for ever: a tab stuck on
//! "Running…". `#[command]` is `#[tauri::command]` and `#[specta::specta]` with the body of an
//! async command run through `crate::panics::guard`, which turns a panic into the command's
//! error. A sync command runs inside the app's invoke handler, which catches its panic itself.
//!
//! An async command must return a `Result` (the app's `CmdResult`): a panic needs an error to
//! become.

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, ReturnType, Type, parse_macro_input, parse_quote};

#[proc_macro_attribute]
pub fn command(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[command] takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let mut function = parse_macro_input!(item as ItemFn);
    if function.sig.asyncness.is_some() {
        if !returns_result(&function.sig.output) {
            return syn::Error::new_spanned(
                &function.sig,
                "an async #[command] returns CmdResult, so that a panic can become its error",
            )
            .to_compile_error()
            .into();
        }
        let name = function.sig.ident.to_string();
        let body = &function.block;
        // The body's future goes on the heap. Tauri builds a command's future on the main
        // thread, whose stack is small (1 MB on Windows), and moves it a few times before it
        // spawns it: with the body inline, the future of a big command grew past that stack
        // ("thread 'main' has overflowed its stack" in the smoke test). Boxed, what the main
        // thread holds is the arguments and a pointer; the body starts on a runtime thread.
        function.block = parse_quote!({
            crate::panics::guard(#name, ::std::boxed::Box::pin(async move #body)).await
        });
    }
    quote!(
        #[tauri::command]
        #[specta::specta]
        #function
    )
    .into()
}

/// Whether the return type's last path segment is `CmdResult` or `Result`.
fn returns_result(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };
    let Type::Path(path) = ty.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "CmdResult" || s.ident == "Result")
}
