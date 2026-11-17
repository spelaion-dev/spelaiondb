// An engine's mark (SDB-363). Which file each engine shows, and why an engine has none, is
// `assets/engines/marks.json`, also read by the desktop crate's test that every engine of the
// core's catalogue is in it: a new engine cannot be added without that decision. The files
// come only from licensed sources, listed in `assets/engines/README.md`.
import table from "../assets/engines/marks.json";
import { Icon } from "./Icons";

interface Without {
  /** What stands in: the base engine's mark (a kind of its connection), a folder or a database. */
  shows: "base" | "folder" | "database";
  why: string;
}

const MARKS = table.marks as Record<string, string>;
const COLORED = new Set<string>(table.colored);
const WITHOUT = table.without as Record<string, Without>;

// Every mark's file, as the address Vite gives it.
const FILES = import.meta.glob<string>("../assets/engines/*.svg", { eager: true, query: "?url", import: "default" });
const fileOf = (engine: string): string | undefined => {
  const name = MARKS[engine];
  return name ? FILES[`../assets/engines/${name}`] : undefined;
};

/** Engine marks are decorative: the engine name is already present in the surrounding UI.
 *  `fallback` is the engine whose mark stands in when `engine` has none of its own (a server
 *  reached through PostgreSQL's protocol shows PostgreSQL's). */
export function EngineIcon({ engine, fallback }: { engine: string | null; fallback?: string | null }) {
  const own = engine ? fileOf(engine) : undefined;
  const without = engine ? WITHOUT[engine] : undefined;
  const shown = own ? engine : without?.shows === "folder" || without?.shows === "database" ? null : fallback ?? null;
  const src = own ?? (shown ? fileOf(shown) : undefined);
  if (src && shown) return <img className={`engine-icon ${COLORED.has(shown) ? "colored" : ""}`} src={src} alt="" aria-hidden="true" />;
  // No licensed mark: a deliberate neutral one, in the interface's own icon set.
  return (
    <span className="engine-icon engine-icon-fallback" aria-hidden="true">
      <Icon name={without?.shows === "folder" ? "folderOpen" : "database"} />
    </span>
  );
}
