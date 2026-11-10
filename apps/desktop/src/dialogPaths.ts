// Where the app's own file dialog (Linux, `src-tauri/src/file_dialog.rs`) starts: pure path
// work, run by `npm run build` with `node --test` (scripts/dialog-paths.test.mjs). Linux paths
// only: the other systems keep the dialog plugin, which remembers its own folder.

/** The folder that holds `path`; `null` for a bare name. */
export function folderOf(path: string): string | null {
  const slash = path.lastIndexOf("/");
  if (slash < 0) return null;
  return slash === 0 ? "/" : path.slice(0, slash);
}

/** The folders an open dialog may start in, the first that exists winning: the folder of the
 *  file it is about (`near`), else the open workspace. */
export function startFolders(near: string | null | undefined, workspace: string | null): string[] {
  const folders = [near ? folderOf(near) : null, workspace];
  return folders.filter((folder, i): folder is string => !!folder && folders.indexOf(folder) === i);
}

/** Where a save dialog starts and the name it proposes, from the app's `defaultPath`: a full
 *  path starts in its own folder (then the workspace), a bare name in the workspace. */
export function savePlace(defaultPath: string | null | undefined, workspace: string | null): { folders: string[]; fileName: string | null } {
  if (!defaultPath) return { folders: startFolders(null, workspace), fileName: null };
  return { folders: startFolders(defaultPath, workspace), fileName: defaultPath.slice(defaultPath.lastIndexOf("/") + 1) || null };
}
