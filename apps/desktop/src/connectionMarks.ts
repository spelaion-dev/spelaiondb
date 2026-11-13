// What follows a connection's name in the explorer (`.connection-marks`): the type's dot, then
// the type's name, the SSH mark and a few tags. The name keeps its width; the marks take what is
// left. The dot always stays, since the type is what tells environments apart (ADR-029); every
// other mark is shown whole or not at all, in order, and one that does not fit never hides a
// smaller one after it.
import { useCallback, useLayoutEffect, useRef } from "react";

/** A mark's width and the space before it when something is shown before it. */
export type Mark = { width: number; gap: number; required?: boolean };

/**
 * Which marks are shown in `available` pixels: the required ones (they come first) always, then
 * each other one, in order, if it fits whole after those already shown.
 */
export function fitMarks(available: number, marks: readonly Mark[]): boolean[] {
  let used = 0;
  let shownAny = false;
  return marks.map((mark) => {
    const needed = used + (shownAny ? mark.gap : 0) + mark.width;
    // Half a pixel for rounding: a mark that fits exactly is not dropped.
    const shown = mark.required === true || needed <= available + 0.5;
    if (shown) {
      used = needed;
      shownAny = true;
    }
    return shown;
  });
}

/** The class a mark that does not fit gets: out of the layout and out of sight, but measurable. */
export const MARK_OUT = "mark-out";

const width = (element: Element) => element.getBoundingClientRect().width;
const columnGap = (element: Element) => parseFloat(getComputedStyle(element).columnGap) || 0;

/** Shows or hides each mark of one `.connection-marks` element for its current width. */
export function fitConnectionMarks(container: HTMLElement): void {
  const gap = columnGap(container);
  const elements: Element[] = [];
  const marks: Mark[] = [];
  for (const child of container.children) {
    const text = child.classList.contains("type-badge") ? child.querySelector(".type-badge-text") : null;
    if (text) {
      // The type's badge is two marks: its dot, which always stays, and its name.
      const textGap = columnGap(child);
      const textWidth = width(text);
      const shownText = textWidth > 0 && !text.classList.contains(MARK_OUT) ? textGap + textWidth : 0;
      elements.push(child, text);
      marks.push({ width: width(child) - shownText, gap, required: true }, { width: textWidth, gap: textGap });
    } else {
      elements.push(child);
      marks.push({ width: width(child), gap });
    }
  }
  const shown = fitMarks(container.clientWidth, marks);
  // Only what changes is written, so rows that already fit do not touch the layout.
  elements.forEach((element, i) => {
    if (element.classList.contains(MARK_OUT) === shown[i]) element.classList.toggle(MARK_OUT, !shown[i]);
  });
}

const resizes =
  typeof ResizeObserver === "undefined"
    ? null
    : new ResizeObserver((entries) => {
        for (const entry of entries) fitConnectionMarks(entry.target as HTMLElement);
      });

/**
 * A ref for a `.connection-marks` element: its marks are fitted when its width changes (the
 * panel, the name, the font) and after every render (marks added, removed or renamed).
 */
export function useFittingMarks(): (element: HTMLElement | null) => void {
  const current = useRef<HTMLElement | null>(null);
  useLayoutEffect(() => {
    if (current.current) fitConnectionMarks(current.current);
  });
  return useCallback((element: HTMLElement | null) => {
    if (current.current) resizes?.unobserve(current.current);
    current.current = element;
    if (element) resizes?.observe(element);
  }, []);
}
