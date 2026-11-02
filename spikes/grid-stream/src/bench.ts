export interface ScrollBenchResult {
  durationMs: number;
  frames: number;
  avgFps: number;
  p50FrameMs: number;
  p95FrameMs: number;
  p99FrameMs: number;
  maxFrameMs: number;
  /** Percentage of frames over 33 ms (below 30 fps): they show as stutter. */
  slowFramePct: number;
  /** Percentage of cells painted as "loading" during scrolling. */
  blankCellPct: number;
  fetchP50Ms: number;
  fetchP95Ms: number;
}

export const percentile = (xs: number[], p: number) => {
  if (xs.length === 0) return 0;
  const sorted = [...xs].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];
};

const round = (x: number) => Math.round(x * 10) / 10;

/**
 * Programmatic scrolling over the grid's native container: first continuous and fast
 * (like dragging the scrollbar), then random jumps (like going to a specific row).
 */
export async function runScrollBench(opts: {
  scroller: HTMLElement;
  durationMs: number;
  cellCounters: { requested: number; blank: number };
  fetchLatenciesMs: number[];
}): Promise<ScrollBenchResult> {
  const { scroller, durationMs, cellCounters } = opts;
  cellCounters.requested = 0;
  cellCounters.blank = 0;
  const fetchStart = opts.fetchLatenciesMs.length;
  const frameTimes: number[] = [];
  const maxTop = scroller.scrollHeight - scroller.clientHeight;
  const pxPerFrame = 2400; // ~70 rows per frame: very fast scrolling

  await new Promise<void>((resolve) => {
    let start = 0;
    let last = 0;
    let frame = 0;
    const tick = (t: number) => {
      if (start === 0) {
        start = t;
        last = t;
      } else {
        frameTimes.push(t - last);
        last = t;
      }
      const elapsed = t - start;
      if (elapsed < durationMs / 2) {
        scroller.scrollTop = Math.min(maxTop, scroller.scrollTop + pxPerFrame);
      } else if (frame % 15 === 0) {
        scroller.scrollTop = Math.random() * maxTop;
      }
      frame++;
      if (elapsed < durationMs) requestAnimationFrame(tick);
      else resolve();
    };
    requestAnimationFrame(tick);
  });

  const total = frameTimes.reduce((a, b) => a + b, 0);
  const fetches = opts.fetchLatenciesMs.slice(fetchStart);
  return {
    durationMs: round(total),
    frames: frameTimes.length,
    avgFps: round((frameTimes.length / total) * 1000),
    p50FrameMs: round(percentile(frameTimes, 50)),
    p95FrameMs: round(percentile(frameTimes, 95)),
    p99FrameMs: round(percentile(frameTimes, 99)),
    maxFrameMs: round(Math.max(...frameTimes)),
    slowFramePct: round((frameTimes.filter((f) => f > 33.4).length / frameTimes.length) * 100),
    blankCellPct: round((cellCounters.blank / Math.max(1, cellCounters.requested)) * 100),
    fetchP50Ms: round(percentile(fetches, 50)),
    fetchP95Ms: round(percentile(fetches, 95)),
  };
}
