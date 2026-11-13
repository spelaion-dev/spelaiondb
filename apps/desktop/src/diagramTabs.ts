// Opening ER diagram tabs (ADR-046) from the explorer, the palette and a diagram itself.

import type { DiagramScopeItem_Deserialize } from "./bindings";
import { connectTabFromExplorer, explorerDatabase } from "./connect";
import { useApp } from "./store";

/** Opens a diagram of `scope` on `connectionId` (or shows the tab already open on it), its
 *  session cloned from `source`: the explorer's by default, or a tab's. */
export function openDiagram(connectionId: string, scope: DiagramScopeItem_Deserialize, source = `meta:${connectionId}`) {
  // An empty table filter is no filter: the same diagram, the same tab.
  if (scope.kind === "schema" && scope.filter && scope.filter.include.length === 0 && scope.filter.exclude.length === 0) {
    scope = { ...scope, filter: null };
  }
  const [tab, created] = useApp.getState().addDiagramTab(connectionId, scope);
  if (!created) return;
  const schema = scope.kind === "tables" ? scope.tables[0]?.schema : scope.schema;
  connectTabFromExplorer(tab, source, schema === undefined ? undefined : explorerDatabase(connectionId, schema));
}
