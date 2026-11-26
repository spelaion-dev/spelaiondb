// How the page names open folders and connections when several folders are open (ADR-075
// point 8): a folder by its name, with its parent's when two have the same name; a connection
// by its name, with its folder's (or "Personal") where another listed connection has the same
// name. Pure (no store, no runtime imports), so its tests can use it.

/** What naming needs of an open folder. */
export interface NamedFolder {
  path: string;
  root: string;
}

/** What naming needs of a listed connection. */
export interface NamedConnection {
  id: string;
  name: string;
  folder?: string | null;
}

/** The last part of a path: a folder's name. */
function baseName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/** Each open folder's name, by its root: the folder's name, with its parent's when another
 *  open folder has the same name, so the two can be told apart. */
export function folderNames(folders: NamedFolder[]): Map<string, string> {
  const names = new Map<string, string>();
  for (const folder of folders) {
    const name = baseName(folder.path);
    const same = folders.filter((other) => baseName(other.path).toLowerCase() === name.toLowerCase()).length > 1;
    const parts = folder.path.split(/[\\/]/).filter(Boolean);
    names.set(folder.root, same && parts.length > 1 ? `${name} (${parts[parts.length - 2]})` : name);
  }
  return names;
}

/** A connection's name as pickers and dialogs show it: its own, followed by its folder's name
 *  (or `personal`) when another listed connection has the same name. */
export function connectionName(connection: NamedConnection, connections: NamedConnection[], names: Map<string, string>, personal: string): string {
  const folder = connection.folder ?? null;
  const ambiguous = connections.some((other) =>
    other.name.toLowerCase() === connection.name.toLowerCase() && (other.id !== connection.id || (other.folder ?? null) !== folder));
  if (!ambiguous) return connection.name;
  return `${connection.name} · ${folder === null ? personal : names.get(folder) ?? baseName(folder)}`;
}
