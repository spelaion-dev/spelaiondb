/** Pretty-print JSON without parsing and rewriting its numbers, keys or string escapes. */
export function formatJson(text: string): string | null {
  try { JSON.parse(text); } catch { return null; }
  const parts: string[] = [];
  let depth = 0, inString = false, escaped = false;
  const indent = () => "  ".repeat(Math.min(depth, 80));
  for (let i = 0; i < text.length; i++) {
    const ch = text[i]!;
    if (inString) {
      parts.push(ch);
      if (escaped) escaped = false;
      else if (ch === "\\") escaped = true;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') { inString = true; parts.push(ch); continue; }
    if (/\s/.test(ch)) continue;
    if (ch === "{" || ch === "[") {
      parts.push(ch); depth++;
      let at = i + 1;
      while (at < text.length && /\s/.test(text[at]!)) at++;
      const next = text[at];
      if (next !== "}" && next !== "]") parts.push("\n", indent());
    } else if (ch === "}" || ch === "]") {
      depth--;
      let at = i - 1;
      while (at >= 0 && /\s/.test(text[at]!)) at--;
      const previous = text[at];
      if (previous !== "{" && previous !== "[") parts.push("\n", indent());
      parts.push(ch);
    } else if (ch === ",") parts.push(",\n", indent());
    else if (ch === ":") parts.push(": ");
    else parts.push(ch);
  }
  return parts.join("");
}
