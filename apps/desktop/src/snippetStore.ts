// The user's editor snippets and the template of new scripts as the page holds them
// (SDB-384). The core stores and checks them; this is what it last answered, for the editor
// (Tab and completion), the settings page and new tabs.

import { create } from "zustand";
import { call, commands } from "./api";
import type { EditorSnippetItem, EditorSnippetLimits } from "./bindings";
import { setNewScriptTemplate } from "./store";

interface SnippetStore {
  snippets: EditorSnippetItem[];
  /** The text a new script starts with; empty for none. */
  template: string;
  /** The core's limits, for the settings page; `null` until read. */
  limits: EditorSnippetLimits | null;
  /** Why they could not be read, if they could not. */
  loadError: string | null;
  load(): Promise<void>;
  /** Saves a snippet (the core checks it); rejects with the core's reason. */
  save(snippet: EditorSnippetItem): Promise<void>;
  remove(name: string): Promise<void>;
  /** Saves the template (the core checks it); rejects with the core's reason. */
  saveTemplate(template: string): Promise<void>;
}

export const useSnippets = create<SnippetStore>((set, get) => ({
  snippets: [],
  template: "",
  limits: null,
  loadError: null,
  async load() {
    try {
      const [snippets, template, limits] = await Promise.all([
        call(commands.editorSnippets()),
        call(commands.newScriptTemplate()),
        call(commands.editorSnippetLimits()),
      ]);
      setNewScriptTemplate(template);
      set({ snippets, template, limits, loadError: null });
    } catch (e) {
      set({ loadError: e instanceof Error ? e.message : String(e) });
    }
  },
  async save(snippet) {
    await call(commands.saveEditorSnippet(snippet));
    await get().load();
  },
  async remove(name) {
    await call(commands.deleteEditorSnippet(name));
    await get().load();
  },
  async saveTemplate(template) {
    await call(commands.setNewScriptTemplate(template));
    await get().load();
  },
}));
