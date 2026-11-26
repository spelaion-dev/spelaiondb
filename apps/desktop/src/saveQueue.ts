// Saving preferences one after another (SDB-83). A change made while another preference is
// still saving used to be dropped without a word; now every change is saved, in the order it
// was made, after the save before it. A setting the UI shows at once ("optimistic") goes back
// to its last saved value when its save fails, unless a newer change of it is already on its
// way: that newer change is what it shows, and what is saved next.

/** Runs saves one at a time, in the order they were asked. `onBusy` gets the setting being
 *  saved, and `null` once nothing is left to save. A failed save does not stop the next one. */
export function createSaveQueue<S>(onBusy: (setting: S | null) => void) {
  let tail: Promise<unknown> = Promise.resolve();
  let waiting = 0;
  return function run<T>(setting: S, save: () => Promise<T>): Promise<T> {
    waiting += 1;
    const done = tail.then(async () => {
      onBusy(setting);
      try {
        return await save();
      } finally {
        waiting -= 1;
        if (waiting === 0) onBusy(null);
      }
    });
    tail = done.catch(() => undefined);
    return done;
  };
}

/** What one optimistic setting last saved, and which of its changes is the newest. */
export function savedSetting<T>(initial: T) {
  let saved = initial;
  let newest = 0;
  /** Changes of this window not yet saved or failed. */
  let pending = 0;
  return {
    /** The value the core has: loaded at start, then each successful save. */
    get saved(): T {
      return saved;
    },
    /** A value read from the core. */
    loaded(value: T): void {
      saved = value;
    },
    /** A change starts: its ticket. */
    change(): number {
      newest += 1;
      pending += 1;
      return newest;
    },
    /** Its save succeeded (saves run in order, so this is the newest saved value). */
    succeeded(value: T): void {
      pending -= 1;
      saved = value;
    },
    /** Its save failed: the value to show again, or `undefined` when a newer change is on its
     *  way and what is shown must stay. Called for every change that does not succeed, also one
     *  that was not shown at once, or it stays pending and `changedElsewhere` shows nothing. */
    failed(ticket: number): T | undefined {
      pending -= 1;
      return ticket === newest ? saved : undefined;
    },
    /** The core says it now stores `value`, saved from any window (SDB-533): it is what a failed
     *  change goes back to, and the value to show, or `undefined` while a change of this window
     *  is on its way: that change shows, and is saved after it. */
    changedElsewhere(value: T): T | undefined {
      saved = value;
      return pending === 0 ? value : undefined;
    },
  };
}
