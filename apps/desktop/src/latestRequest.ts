// Requests of which only the newest one's answer counts.
//
// Two header clicks send two requests to the core, and their answers may come back in either
// order. Whatever the order, the view must end as the last click asked: an answer to an older
// request is dropped when it arrives, never applied on top of a newer one.

/** What became of a request: its answer, or nothing because a newer request was made. */
export type Latest<T> = { latest: true; value: T } | { latest: false };

/**
 * Runs requests so that only the newest one made is answered. A request that fails while it is
 * still the newest rejects as usual; one that fails after a newer request was made is dropped
 * like an answer would be (its error is about a sort nobody wants any more).
 */
export function latestOnly(): <T>(request: () => Promise<T>) => Promise<Latest<T>> {
  let made = 0;
  return async <T>(request: () => Promise<T>): Promise<Latest<T>> => {
    const mine = ++made;
    try {
      const value = await request();
      return mine === made ? { latest: true, value } : { latest: false };
    } catch (e) {
      if (mine === made) throw e;
      return { latest: false };
    }
  };
}
