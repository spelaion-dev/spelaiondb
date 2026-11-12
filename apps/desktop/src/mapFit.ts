// When the cell panel's map fits its shapes (SDB-280). It fits new shapes and its first size;
// while its view is still that automatic fit, it fits again when its size changes (the panel
// resized, a line above it appeared), so the fit always matches the map's real size and 0
// gives the same view. Once the user pans or zooms, a size change keeps their view.

/** The last automatic fit: the shapes and the size it was for, and whether the view is still it. */
export interface FitMemory {
  box: unknown;
  size: string;
  following: boolean;
}

export const sizeKey = (width: number, height: number) => `${width}x${height}`;

/** Whether the map must fit its shapes now, for `box` (compared by identity) at this size. */
export function shouldFit(last: FitMemory | null, box: unknown, width: number, height: number): boolean {
  if (width <= 0 || height <= 0) return false;
  if (!last || last.box !== box) return true;
  return last.following && last.size !== sizeKey(width, height);
}

/** The memory after a fit at this size. */
export function fitted(box: unknown, width: number, height: number): FitMemory {
  return { box, size: sizeKey(width, height), following: true };
}

/** The memory after the user moved the view: a size change no longer fits it again. */
export function moved(last: FitMemory | null): FitMemory | null {
  return last ? { ...last, following: false } : last;
}
