// HTTP console syntax and suggestions only. Parsing and every execution safeguard remain
// in spelaion-dialect's HTTP language, never in this editor extension.
import { StreamLanguage } from "@codemirror/language";
import { autocompletion } from "@codemirror/autocomplete";
import type { CompletionCatalog } from "./sqlCompletion";

const engines = new Set(["elasticsearch", "opensearch", "solr", "meilisearch"]);
export const isHttpLanguage = (engine: string | null) => engine !== null && engines.has(engine);

const http = StreamLanguage.define({
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.sol() && stream.match(/(?:GET|HEAD|POST|PUT|PATCH|DELETE)\b/)) return "keyword";
    if (stream.peek() === "#" && stream.sol()) { stream.skipToEnd(); return "comment"; }
    if (stream.match(/\/(?:[^\s]*)/)) return "string";
    if (stream.match(/"(?:[^"\\]|\\.)*"/)) return "string";
    if (stream.match(/\b(?:true|false|null)\b/)) return "atom";
    if (stream.match(/-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/)) return "number";
    stream.next();
    return null;
  },
  languageData: { commentTokens: { line: "#" } },
});

export function httpLanguage(engine: string, catalog: CompletionCatalog) {
  return [http, autocompletion({ override: [(context) => {
    const line = context.state.doc.lineAt(context.pos);
    const before = line.text.slice(0, context.pos - line.from);
    if (/^\s*\w*$/.test(before)) {
      return { from: line.from, options: ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"].map((label) => ({ label, type: "keyword" })) };
    }
    const path = before.match(/^\s*(?:GET|HEAD|POST|PUT|PATCH|DELETE)\s+(\/[^\s]*)$/);
    if (!path) return null;
    const options = catalog.tables.map(({ name }) => ({
      label: engine === "solr" ? `/solr/${name}/select?q=*%3A*&rows=200&wt=json`
        : engine === "meilisearch" ? `/indexes/${name}/documents?limit=200` : `/${name}/_search`,
      type: "variable",
    }));
    return { from: context.pos - path[1]!.length, options };
  }] })];
}

/** Format a single selected HTTP request. A script can be formatted one request at a time. */
export function formatHttp(text: string, indent: number): string {
  const newline = text.indexOf("\n");
  if (newline < 0) return text.trim();
  const header = text.slice(0, newline).trim();
  const body = text.slice(newline + 1).trim();
  return body ? `${header}\n${JSON.stringify(JSON.parse(body), null, indent)}` : header;
}
