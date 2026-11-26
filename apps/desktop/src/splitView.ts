// The editor area's layout (SDB-30): one tab, or two tabs side by side or one above the other.
// It exists only in the page, so it is a discriminated union whose transitions are written here
// and nowhere else (ADR-060). Each pane is an ordinary tab: its own session, its own results,
// its own connection row. The focused pane is the active tab; nothing else says which it is.
//
// This module imports nothing: `scripts/split-view.test.mjs` runs it with `node --test`.

/** `right`: the panes side by side, with a vertical separator; `down`: one above the other. */
export type SplitDirection = "right" | "down";

export type SplitLayout =
  | { kind: "single" }
  | {
      kind: "split";
      direction: SplitDirection;
      /** The tab in the left (or top) pane. */
      first: string;
      /** The tab in the right (or bottom) pane; never the same as `first`. */
      second: string;
      /** The first pane's share of the room, between `MIN_SHARE` and `MAX_SHARE`. */
      share: number;
    };

export type Pane = "first" | "second";

export const SINGLE: SplitLayout = { kind: "single" };
export const MIN_SHARE = 0.2;
export const MAX_SHARE = 0.8;
export const DEFAULT_SHARE = 0.5;

export function clampShare(share: number): number {
  if (!Number.isFinite(share)) return DEFAULT_SHARE;
  return Math.min(MAX_SHARE, Math.max(MIN_SHARE, share));
}

/** The pane that shows `tab`, or `null` when it is not shown in a split. */
export function paneOf(layout: SplitLayout, tab: string): Pane | null {
  switch (layout.kind) {
    case "single":
      return null;
    case "split":
      return layout.first === tab ? "first" : layout.second === tab ? "second" : null;
    default:
      return unknownLayout(layout);
  }
}

/** The tab in the other pane than the one showing `tab` (none when not split, or not shown). */
export function otherPaneTab(layout: SplitLayout, tab: string): string | null {
  switch (paneOf(layout, tab)) {
    case "first":
      return layout.kind === "split" ? layout.second : null;
    case "second":
      return layout.kind === "split" ? layout.first : null;
    case null:
      return null;
  }
}

/** The two panes, `focused` (the active tab) in one and `other` in the other. The focused tab
 *  keeps the pane it had; a tab that was not shown goes first. */
export function split(layout: SplitLayout, focused: string, other: string, direction: SplitDirection): SplitLayout {
  if (focused === other) return layout;
  const share = layout.kind === "split" ? layout.share : DEFAULT_SHARE;
  if (layout.kind === "split" && layout.second === focused) {
    return { kind: "split", direction, first: other, second: focused, share };
  }
  return { kind: "split", direction, first: focused, second: other, share };
}

/** The user shows `next` (a click in the strip, a new tab, Ctrl+Tab): a tab already in a pane
 *  only takes the focus; any other takes the place of the focused one, `active`. */
export function show(layout: SplitLayout, active: string | null, next: string): SplitLayout {
  switch (layout.kind) {
    case "single":
      return layout;
    case "split":
      if (layout.first === next || layout.second === next) return layout;
      if (layout.first === active) return { ...layout, first: next };
      return { ...layout, second: next };
    default:
      return unknownLayout(layout);
  }
}

/** After closing tabs: a pane whose tab closed ends the split. */
export function withoutTabs(layout: SplitLayout, closed: ReadonlySet<string>): SplitLayout {
  switch (layout.kind) {
    case "single":
      return layout;
    case "split":
      return closed.has(layout.first) || closed.has(layout.second) ? SINGLE : layout;
    default:
      return unknownLayout(layout);
  }
}

/** The tab that takes the focus once `closed` are gone: the other pane's, when its tab stays. */
export function survivingPane(layout: SplitLayout, closed: ReadonlySet<string>): string | null {
  if (layout.kind !== "split") return null;
  if (closed.has(layout.first) && !closed.has(layout.second)) return layout.second;
  if (closed.has(layout.second) && !closed.has(layout.first)) return layout.first;
  return null;
}

export function resized(layout: SplitLayout, share: number): SplitLayout {
  switch (layout.kind) {
    case "single":
      return layout;
    case "split": {
      const next = clampShare(share);
      return next === layout.share ? layout : { ...layout, share: next };
    }
    default:
      return unknownLayout(layout);
  }
}

function unknownLayout(layout: never): never {
  throw new Error(`unknown split layout: ${JSON.stringify(layout)}`);
}
