import { useEffect } from "react";

const DIALOG = '[role="dialog"][aria-modal="true"]';
const FOCUSABLE = 'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])';

/** The topmost visible modal, including a dialog opened over another wizard. */
export function topDialog(): HTMLElement | null {
  const dialogs = [...document.querySelectorAll<HTMLElement>(DIALOG)].filter((dialog) => dialog.getClientRects().length > 0);
  let top: HTMLElement | null = null;
  let highest = -Infinity;
  for (const dialog of dialogs) {
    const backdrop = dialog.closest<HTMLElement>(".modal-backdrop");
    const parsed = backdrop ? Number.parseInt(getComputedStyle(backdrop).zIndex, 10) : 0;
    const layer = Number.isNaN(parsed) ? 0 : parsed;
    if (layer >= highest) { highest = layer; top = dialog; }
  }
  return top;
}

function focusable(dialog: HTMLElement): HTMLElement[] {
  return [...dialog.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((item) => item.getClientRects().length > 0 && !item.closest("[hidden], [inert]"));
}

/** Keep focus in the topmost modal and restore it when that modal closes. */
export function useModalFocus() {
  useEffect(() => {
    let active: HTMLElement | null = null;
    let lastFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const previous = new WeakMap<HTMLElement, HTMLElement>();

    function update() {
      const next = topDialog();
      if (next === active) return;
      const old = active;
      const restore = old && !old.isConnected ? previous.get(old) : undefined;
      if (next && !previous.has(next) && lastFocus && !next.contains(lastFocus)) previous.set(next, lastFocus);
      active = next;
      if (restore?.isConnected && (!next || next.contains(restore))) {
        // Give focus back once the event that closed the dialog is over. Restoring it inside
        // that event hands the rest of the key to the restored control: an Enter that submitted
        // a dialog would press the button that opened it (opening it again) or type a line
        // break into the editor. Nothing is restored if something else took the focus first.
        const target = restore;
        setTimeout(() => {
          const current = document.activeElement;
          if (target.isConnected && (current === null || current === document.body)) target.focus({ preventScroll: true });
        }, 0);
      } else if (next && !next.contains(document.activeElement)) {
        (focusable(next)[0] ?? next).focus({ preventScroll: true });
      }
    }

    const onFocus = (event: FocusEvent) => {
      update();
      const target = event.target instanceof HTMLElement ? event.target : null;
      if (active && target && !active.contains(target)) {
        (focusable(active)[0] ?? active).focus({ preventScroll: true });
      } else if (target) lastFocus = target;
    };
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Tab") return;
      update();
      if (!active) return;
      const items = focusable(active);
      const first = items[0];
      const last = items[items.length - 1];
      if (!first || !last) { event.preventDefault(); return; }
      if (event.shiftKey && (document.activeElement === first || !active.contains(document.activeElement))) {
        event.preventDefault(); last.focus();
      } else if (!event.shiftKey && (document.activeElement === last || !active.contains(document.activeElement))) {
        event.preventDefault(); first.focus();
      }
    };
    const observer = new MutationObserver(update);
    observer.observe(document.body, { childList: true, subtree: true });
    document.addEventListener("focusin", onFocus);
    document.addEventListener("keydown", onKey, true);
    update();
    return () => {
      observer.disconnect();
      document.removeEventListener("focusin", onFocus);
      document.removeEventListener("keydown", onKey, true);
    };
  }, []);
}
