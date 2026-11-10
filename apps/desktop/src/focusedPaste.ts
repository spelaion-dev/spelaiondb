// Ctrl+V for an area that is not a text field (the explorer, the import dialog), decided by the
// focused element. WebKitGTK sends the `paste` event to the node holding the page's selection,
// not to the focused element (WebView2 sends it to the focused one): after a click in the SQL
// editor and then on an explorer row, the event's target is still the editor's line, and an area
// that decided by `e.target` let the editor paste the text. A capture listener on `window` runs
// before the event reaches any target, so the area can take it before CodeMirror sees it.
import { useEffect, useRef } from "react";

/** Whether an element is a text field, which keeps its own paste. */
export function isTextField(element: Element | null): boolean {
  return (
    element instanceof HTMLInputElement ||
    element instanceof HTMLTextAreaElement ||
    (element instanceof HTMLElement && element.isContentEditable)
  );
}

/**
 * Offers every paste in the window to `take`, with the focused element, before the event reaches
 * its target. When `take` returns true the event is the area's: it goes no further and the
 * browser does not paste. The text stays untrusted: `take` hands it to the core as it is.
 */
export function useFocusedPaste(take: (e: ClipboardEvent, focused: Element | null) => boolean) {
  const latest = useRef(take);
  latest.current = take;
  useEffect(() => {
    const listener = (e: ClipboardEvent) => {
      if (!latest.current(e, document.activeElement)) return;
      e.preventDefault();
      e.stopPropagation();
    };
    window.addEventListener("paste", listener, true);
    return () => window.removeEventListener("paste", listener, true);
  }, []);
}
