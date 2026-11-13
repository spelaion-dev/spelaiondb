// Reading the clipboard's text, only while handling a click or a key (a menu's Paste, "Paste
// from clipboard", Ctrl+Alt+V). Ctrl+V does not come here: it takes the text from its `paste`
// event.
//
// On Linux the app reads it through GTK (`read_clipboard_text`, src-tauri/src/clipboard_text.rs):
// WebKitGTK's `navigator.clipboard.readText()` asks only for `text/plain;charset=utf-8`, so on X11
// text from xterm, urxvt, xclip or xsel (offered only as `UTF8_STRING` or `STRING`) read as "".
// Windows and macOS keep the webview's own read. Either way the text is untrusted: callers hand
// it to the core as it is (ADR-030, ADR-038).

import { call, commands } from "./api";

const gtkClipboard = /Linux/.test(navigator.userAgent) && !/Android/.test(navigator.userAgent);

/** The clipboard's text ("" when it holds none). Rejects when the clipboard cannot be read. */
export async function readClipboardText(): Promise<string> {
  if (gtkClipboard) return (await call(commands.readClipboardText())) ?? "";
  return navigator.clipboard.readText();
}
