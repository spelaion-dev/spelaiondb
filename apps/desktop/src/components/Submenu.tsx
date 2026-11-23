// A context menu's submenu that stays inside the window, even near its edges, and tolerates a
// pointer going diagonally into it. The result grid's menus and the SQL editor's menu (SDB-438)
// share it.

import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { MENU_MARGIN, placeMenu } from "../menuPlacement";

/** How long the pointer rests on a submenu's item before the submenu opens, and how long an
 *  open submenu stays once the pointer has left it. A pointer going diagonally into an open
 *  submenu crosses other items of the menu on the way: they neither close it nor open theirs. */
const SUBMENU_OPEN_DELAY = 150;
const SUBMENU_CLOSE_DELAY = 400;

/** The submenu open now: opening another one closes it at once. */
let openSubmenu: { close(): void } | null = null;

/** The items of a menu that the arrows move between: its own, not those of a submenu in it. */
export function menuItems(menu: HTMLElement): HTMLButtonElement[] {
  return [...menu.querySelectorAll<HTMLButtonElement>(":scope > button, :scope > .grid-submenu > button")].filter((item) => !item.disabled);
}

/** Up and Down (Home and End) move the focus among a menu's own items; true when it moved. */
export function moveInMenu(menu: HTMLElement, key: string): boolean {
  if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(key)) return false;
  const items = menuItems(menu);
  if (items.length === 0) return true;
  const at = items.indexOf(document.activeElement as HTMLButtonElement);
  const next = key === "Home" ? 0 : key === "End" ? items.length - 1 : at < 0 ? 0 : (at + (key === "ArrowDown" ? 1 : items.length - 1)) % items.length;
  items[next]?.focus();
  return true;
}

/**
 * `keyboard`: the menu's keys, for a menu the arrows move through (the SQL editor's): Right or
 * Enter opens it and its first item takes the focus; Up and Down move inside it; Left or Escape
 * close it and give the focus back to its item. Without it (the grid's menus, which Tab goes
 * through), the submenu opens when its item has the focus.
 */
export function Submenu({ label, children, keyboard = false }: { label: string; children: ReactNode; keyboard?: boolean }) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const submenu = useRef<HTMLDivElement>(null);
  const timer = useRef<number | null>(null);
  /** Opened from the keyboard: its first item takes the focus once it is drawn. */
  const focusFirst = useRef(false);
  const clearTimer = () => {
    if (timer.current !== null) window.clearTimeout(timer.current);
    timer.current = null;
  };
  const self = useRef({ close: () => {} });
  self.current.close = () => {
    clearTimer();
    if (openSubmenu === self.current) openSubmenu = null;
    setOpen(false);
  };
  const show = (focus = false) => {
    clearTimer();
    if (openSubmenu !== self.current) openSubmenu?.close();
    openSubmenu = self.current;
    focusFirst.current = focus;
    setOpen(true);
  };
  useEffect(() => () => self.current.close(), []);
  useLayoutEffect(() => {
    if (!open || !trigger.current || !submenu.current) return;
    const position = () => {
      if (!trigger.current || !submenu.current) return;
      const anchor = trigger.current.getBoundingClientRect();
      const width = submenu.current.getBoundingClientRect().width;
      const right = anchor.right;
      const x = right + width <= window.innerWidth - MENU_MARGIN ? right : anchor.left - width;
      placeMenu(submenu.current, x, anchor.top);
    };
    position();
    if (focusFirst.current) {
      focusFirst.current = false;
      menuItems(submenu.current)[0]?.focus();
    }
    // It follows its item when the menu or the submenu changes size (items added while open).
    const resized = new ResizeObserver(position);
    resized.observe(submenu.current);
    const parent = trigger.current.closest(".grid-menu");
    if (parent) resized.observe(parent);
    window.addEventListener("resize", position);
    window.addEventListener("scroll", position, true);
    return () => {
      resized.disconnect();
      window.removeEventListener("resize", position);
      window.removeEventListener("scroll", position, true);
    };
  }, [open, children]);
  const back = () => {
    self.current.close();
    trigger.current?.focus();
  };
  return (
    <div className="grid-submenu" onMouseEnter={() => {
      clearTimer();
      if (!open) timer.current = window.setTimeout(() => show(), SUBMENU_OPEN_DELAY);
    }} onMouseLeave={(e) => {
      clearTimer();
      if (open && !e.currentTarget.contains(document.activeElement)) timer.current = window.setTimeout(() => self.current.close(), SUBMENU_CLOSE_DELAY);
    }} onFocus={keyboard ? undefined : () => show()} onBlur={(e) => {
      if (!e.currentTarget.contains(e.relatedTarget)) self.current.close();
    }}>
      <button
        ref={trigger}
        role="menuitem"
        aria-haspopup="menu"
        aria-expanded={open}
        // A click (or Enter) opens it, or takes the focus into it when the pointer already did.
        onClick={keyboard ? () => (open ? (submenu.current && menuItems(submenu.current)[0]?.focus()) : show(true)) : undefined}
        onKeyDown={keyboard ? (e) => {
          if (e.key !== "ArrowRight") return;
          e.preventDefault();
          e.stopPropagation();
          show(true);
        } : undefined}
      >
        {label} ▸
      </button>
      {open && (
        <div
          ref={submenu}
          className="grid-menu grid-child-menu"
          role="menu"
          aria-label={label}
          onKeyDown={keyboard ? (e) => {
            if (e.key === "ArrowLeft" || e.key === "Escape") {
              e.preventDefault();
              e.stopPropagation();
              back();
            } else if (submenu.current && moveInMenu(submenu.current, e.key)) {
              e.preventDefault();
              e.stopPropagation();
            }
          } : undefined}
        >
          {children}
        </div>
      )}
    </div>
  );
}
