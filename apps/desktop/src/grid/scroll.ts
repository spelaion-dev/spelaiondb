// The result grid's vertical scroll past the browser's height cap (SDB-109).
//
// The grid scrolls a container whose spacer is as tall as the result. Chromium (WebView2) and
// WebKit cap an element's height at 33,554,428 px (their layout unit's limit), so a spacer for
// more than about 1.4 million 24 px rows stops growing and the rows after it cannot be reached.
// Past `MAX_SPACER_H` the spacer stays at that height and a scroll position is scaled to a
// position in the content: the top of the scroll is the first row, its end is the last one.
// Below it nothing is scaled and a scroll position is a content position, as before.

/** The tallest spacer the grid makes, safely under both engines' cap (and above a million rows). */
export const MAX_SPACER_H = 30_000_000;

/** How a scroll position maps to the content for one content height and viewport. */
export interface ScrollMap {
  /** The content's full height: the header and every row. */
  contentH: number;
  /** The spacer's height: the content's, or `MAX_SPACER_H` past it. */
  spacerH: number;
  /** The scroll container's visible height. */
  viewportH: number;
}

export function scrollMap(contentH: number, viewportH: number, maxH: number = MAX_SPACER_H): ScrollMap {
  return { contentH, spacerH: Math.min(contentH, maxH), viewportH };
}

/** Whether scroll positions are scaled: the content is taller than the spacer can be. */
export function isScaled(map: ScrollMap): boolean {
  return map.contentH > map.spacerH && map.spacerH > map.viewportH;
}

/** The furthest the content can be scrolled: its height less the viewport's. */
export function maxContentTop(map: ScrollMap): number {
  return Math.max(0, map.contentH - map.viewportH);
}

/** The furthest the container can be scrolled. */
export function maxScrollTop(map: ScrollMap): number {
  return Math.max(0, map.spacerH - map.viewportH);
}

/** The content position shown at a container scroll position. */
export function toContent(map: ScrollMap, scrollTop: number): number {
  const top = Math.min(Math.max(0, scrollTop), maxScrollTop(map));
  if (!isScaled(map)) return top;
  return (top * maxContentTop(map)) / maxScrollTop(map);
}

/** The container scroll position that shows a content position. */
export function toScroll(map: ScrollMap, contentTop: number): number {
  const top = Math.min(Math.max(0, contentTop), maxContentTop(map));
  if (!isScaled(map)) return top;
  return (top * maxScrollTop(map)) / maxContentTop(map);
}

/** A wheel event's movement in pixels, from its delta and its unit (pixels, lines or pages). */
export function wheelPixels(delta: number, deltaMode: number, lineH: number, pageH: number): number {
  switch (deltaMode) {
    case 1:
      return delta * lineH;
    case 2:
      return delta * pageH;
    default:
      return delta;
  }
}
