// Where a context menu or popover opens: at the point it was asked for (the click) when it fits,
// otherwise moved up or left just enough to stay inside the window.
import { useLayoutEffect, type RefObject } from "react";

/** The space kept between a menu and the window's edges. */
export const MENU_MARGIN = 8;

/**
 * The top-left corner for a box of `width` × `height` wanted at (`x`, `y`) in a window of
 * `viewWidth` × `viewHeight`: the point itself when the box fits, otherwise moved up or left to
 * end `MENU_MARGIN` before the edge, and never past the top or left margin (a box taller or
 * wider than the window starts at the margin).
 */
export function fitInWindow(
  x: number,
  y: number,
  width: number,
  height: number,
  viewWidth: number,
  viewHeight: number,
): { left: number; top: number } {
  return {
    left: Math.max(MENU_MARGIN, Math.min(x, viewWidth - width - MENU_MARGIN)),
    top: Math.max(MENU_MARGIN, Math.min(y, viewHeight - height - MENU_MARGIN)),
  };
}

/** Keeps a fixed-position menu inside the window, wanted at (`x`, `y`). */
export function placeMenu(menu: HTMLElement, x: number, y: number) {
  // Measured at the window's corner first: a fixed box near the right edge is narrowed to the
  // space left there, and that narrower width must not be the one kept.
  menu.style.left = "0px";
  menu.style.top = "0px";
  const rect = menu.getBoundingClientRect();
  const { left, top } = fitInWindow(x, y, rect.width, rect.height, window.innerWidth, window.innerHeight);
  menu.style.left = `${left}px`;
  menu.style.top = `${top}px`;
}

/**
 * Places the menu in `ref` with `placeMenu` before it is painted, and again when the window is
 * resized or the menu changes size (items that appear once something has loaded).
 */
export function useMenuInWindow(ref: RefObject<HTMLElement | null>, x: number, y: number) {
  useLayoutEffect(() => {
    const menu = ref.current;
    if (!menu) return;
    const place = () => placeMenu(menu, x, y);
    place();
    window.addEventListener("resize", place);
    const observer = new ResizeObserver(place);
    observer.observe(menu);
    return () => {
      window.removeEventListener("resize", place);
      observer.disconnect();
    };
  }, [ref, x, y]);
}
