// Opening ER diagram tabs (ADR-046) from the explorer, the palette and a diagram itself, and
// saved diagrams (ADR-084) from the palette.

import { call, commands } from "./api";
import type { DiagramScopeItem_Deserialize, SavedDiagramItem_Serialize as SavedDiagramItem } from "./bindings";
import { explorerSession } from "./windowContext";
import { connectionKey, holdsConnection } from "./connectionKey";
import { connectTabFromExplorer, explorerDatabase, openSession } from "./connect";
import { useApp } from "./store";
import { CONNECTING, DISCONNECTED, connectFailed, connectedWith } from "./tabState";

/** Fired on `window` with a diagram tab's id: Ctrl+S (or the palette's Save) saves it. */
export const SAVE_DIAGRAM_EVENT = "spelaion:save-diagram";

/** The schema a diagram reads first, for the engines whose explorer calls databases schemas. */
function scopeSchema(scope: DiagramScopeItem_Deserialize): string | undefined {
  return scope.kind === "tables" ? scope.tables[0]?.schema : scope.schema;
}

/** Opens a diagram of `scope` on `connectionId` (or shows the tab already open on it), its
 *  session cloned from `source`: the explorer's by default, or a tab's. */
export function openDiagram(connectionId: string, scope: DiagramScopeItem_Deserialize, source = explorerSession(connectionKey(connectionId))) {
  // An empty table filter is no filter: the same diagram, the same tab.
  if (scope.kind === "schema" && scope.filter && scope.filter.include.length === 0 && scope.filter.exclude.length === 0) {
    scope = { ...scope, filter: null };
  }
  const [tab, created] = useApp.getState().addDiagramTab(connectionId, scope);
  if (!created) return;
  const schema = scopeSchema(scope);
  connectTabFromExplorer(tab, source, schema === undefined ? undefined : explorerDatabase(connectionKey(connectionId), schema));
}

/** Opens a saved diagram in a tab of its own (or shows the one already open): its scope comes
 *  from the core, checked; the tab reads the catalog again on a session of its own, cloned
 *  from the explorer's when the explorer is connected, else opened as a new one. */
export async function openSavedDiagram(item: SavedDiagramItem): Promise<void> {
  const detail = await call(commands.openSavedDiagram(item.id));
  // A tab keeps its connection's id; the diagram names its connection by folder and id.
  const key = detail.saved.connection;
  const connectionId = key.id;
  const scope: DiagramScopeItem_Deserialize = detail.scope;
  const [tab, created] = useApp.getState().addDiagramTab(connectionId, scope, { id: detail.saved.id, name: detail.saved.name });
  if (!created) return;
  const schema = scopeSchema(scope);
  const database = schema === undefined ? undefined : explorerDatabase(key, schema);
  if (holdsConnection(useApp.getState().explorerConnected, key)) {
    connectTabFromExplorer(tab, explorerSession(key), database);
    return;
  }
  useApp.getState().setConnection(tab, CONNECTING);
  try {
    const session = await openSession(tab, key);
    if (session === null) {
      useApp.getState().setConnection(tab, DISCONNECTED);
      return;
    }
    if (database) await call(commands.selectDatabase(tab, database)).catch(() => undefined);
    useApp.getState().setConnection(tab, connectedWith(session));
  } catch (e) {
    // The tab is open: it says why it could not connect, and offers to reconnect.
    useApp.getState().setConnection(tab, connectFailed(e instanceof Error ? e.message : String(e)));
  }
}
