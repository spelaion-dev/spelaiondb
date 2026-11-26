// How the page names folders where several are open (ADR-075 point 8): by their last part, and
// with their parent too when two have the same last part. Pure, so `node --test` covers it.

/** A path's parts, without Windows' `\\?\` prefix (canonical roots carry it) or empty parts. */
function parts(path: string): string[] {
  return path.replace(/^\\\\\?\\/, "").split(/[\\/]+/).filter((part) => part !== "");
}

/** The name of each of `paths`, in the same order: its last part, or `name (parent)` when
 *  another of them has the same last part. */
export function folderLabels(paths: readonly string[]): string[] {
  const split = paths.map(parts);
  const last = split.map((p) => p.at(-1) ?? "");
  return split.map((p, i): string => {
    const name = last[i] || (paths[i] ?? "");
    const shared = last.some((other, j) => j !== i && other === last[i]);
    const parent = p.at(-2);
    return shared && parent ? `${name} (${parent})` : name;
  });
}
