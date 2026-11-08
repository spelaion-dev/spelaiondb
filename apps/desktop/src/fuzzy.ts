// The command palette's fuzzy search (SDB-18): the letters of each word typed must appear in the
// text in order, not necessarily together ("clt" finds "Close tab"), and the words in any order
// ("tab close" finds it too). Matches at the start of a word, and letters that follow each other,
// count for more; shorter texts win ties. Pure, so `scripts/fuzzy.test.mjs` runs it.

export interface FuzzyMatch {
  score: number;
  /** Indexes (in code points) of the matched characters, for highlighting. */
  positions: number[];
}

const SEPARATORS = new Set([" ", "_", "-", ".", "/", "\\", ":", "(", ")", ",", "·"]);
const START_BONUS = 8;
const WORD_BONUS = 7;
const CAMEL_BONUS = 6;
const NEXT_BONUS = 5;
const GAP_PENALTY = 1;
const LEAD_PENALTY = 0.2;

function lower(char: string): string {
  const folded = char.toLowerCase();
  // A character whose lower case is longer (İ) keeps one code point.
  return [...folded].length === 1 ? folded : char;
}

function bonusAt(chars: string[], index: number): number {
  if (index === 0) return START_BONUS;
  const previous = chars[index - 1] ?? "";
  const current = chars[index] ?? "";
  if (SEPARATORS.has(previous)) return WORD_BONUS;
  const previousLower = previous === previous.toLowerCase() && previous !== previous.toUpperCase();
  const currentUpper = current === current.toUpperCase() && current !== current.toLowerCase();
  if (previousLower && currentUpper) return CAMEL_BONUS;
  return 0;
}

/** The best way to find one word in the text, or null if its letters are not all there. */
function matchWord(word: string[], chars: string[], folded: string[], bonuses: number[]): FuzzyMatch | null {
  const n = chars.length;
  const m = word.length;
  if (m === 0) return { score: 0, positions: [] };
  if (m > n) return null;
  // best[j][i]: the best score with word[j] matched at text[i]; from[j][i]: where word[j - 1] was.
  const best: Float64Array[] = [];
  const from: Int32Array[] = [];
  for (let j = 0; j < m; j++) {
    const row = new Float64Array(n).fill(-Infinity);
    const back = new Int32Array(n).fill(-1);
    const letter = word[j];
    if (j === 0) {
      for (let i = 0; i < n; i++) {
        if (folded[i] === letter) row[i] = 1 + (bonuses[i] ?? 0) - i * LEAD_PENALTY;
      }
    } else {
      const previous = best[j - 1] as Float64Array;
      // The best previous match before i - 1, with its gap penalty counted from i: running
      // maximum of previous[k] + k * GAP_PENALTY.
      let runningBest = -Infinity;
      let runningAt = -1;
      for (let i = 0; i < n; i++) {
        if (i >= 2) {
          const k = i - 2;
          const candidate = (previous[k] ?? -Infinity) + k * GAP_PENALTY;
          if (candidate > runningBest) {
            runningBest = candidate;
            runningAt = k;
          }
        }
        if (folded[i] !== letter) continue;
        const own = 1 + (bonuses[i] ?? 0);
        const adjacent = i >= 1 ? (previous[i - 1] ?? -Infinity) + NEXT_BONUS : -Infinity;
        const apart = runningAt >= 0 ? runningBest - (i - 1) * GAP_PENALTY : -Infinity;
        if (adjacent === -Infinity && apart === -Infinity) continue;
        if (adjacent >= apart) {
          row[i] = own + adjacent;
          back[i] = i - 1;
        } else {
          row[i] = own + apart;
          back[i] = runningAt;
        }
      }
    }
    best.push(row);
    from.push(back);
  }
  const last = best[m - 1] as Float64Array;
  let end = -1;
  let endScore = -Infinity;
  for (let i = 0; i < n; i++) {
    const score = last[i] ?? -Infinity;
    if (score > endScore) {
      endScore = score;
      end = i;
    }
  }
  if (end < 0) return null;
  const positions: number[] = [];
  let at = end;
  for (let j = m - 1; j >= 0 && at >= 0; j--) {
    positions.push(at);
    at = (from[j] as Int32Array)[at] ?? -1;
  }
  positions.reverse();
  return { score: endScore, positions };
}

/** How well the query matches the text, or null if it does not. An empty query matches all. */
export function fuzzyMatch(query: string, text: string): FuzzyMatch | null {
  const words = query.trim().split(/\s+/).filter((word) => word !== "");
  if (words.length === 0) return { score: 0, positions: [] };
  const chars = [...text];
  const folded = chars.map(lower);
  const bonuses = chars.map((_, i) => bonusAt(chars, i));
  let score = 0;
  const positions = new Set<number>();
  for (const word of words) {
    const match = matchWord([...word].map(lower), chars, folded, bonuses);
    if (!match) return null;
    score += match.score;
    for (const position of match.positions) positions.add(position);
  }
  const whole = query.trim().toLowerCase();
  if (whole === text.toLowerCase()) score += 20;
  else if (text.toLowerCase().startsWith(whole)) score += 10;
  score -= chars.length * 0.05;
  return { score, positions: [...positions].sort((a, b) => a - b) };
}
