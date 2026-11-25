// "Compare schemas…" (ADR-070): which schema the dialog compares from. The comparison itself is
// the core's: the dialog only holds its report, and asks the core for the script by its id.

import { create } from "zustand";

/** The source of a comparison: a schema of a connection connected in the explorer. */
export interface CompareSource {
  connectionId: string;
  schema: string;
}

export const useSchemaCompare = create<{
  source: CompareSource | null;
  open(source: CompareSource): void;
  close(): void;
}>((set) => ({
  source: null,
  open: (source) => set({ source }),
  close: () => set({ source: null }),
}));

/** Opens the comparison's dialog with `schema` of `connectionId` as its source. */
export function compareSchemas(connectionId: string, schema: string) {
  useSchemaCompare.getState().open({ connectionId, schema });
}
