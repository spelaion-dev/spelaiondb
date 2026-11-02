// SQL editor with CodeMirror 6: engine dialect, table autocompletion, Ctrl/Cmd+Enter to run
// and underlining of the error at its position.

import { useEffect, useRef } from "react";
import { useTranslation } from "react-i18next";
import { basicSetup } from "codemirror";
import { Compartment, EditorState, Prec } from "@codemirror/state";
import { EditorView, keymap } from "@codemirror/view";
import { PostgreSQL, SQLite, sql } from "@codemirror/lang-sql";
import { closeLintPanel, openLintPanel, setDiagnostics } from "@codemirror/lint";
import { closeSearchPanel, gotoLine, openSearchPanel, searchPanelOpen } from "@codemirror/search";
import { closeCompletion, completionStatus, startCompletion } from "@codemirror/autocomplete";
import { wordEnd } from "../text";

export interface RunRequest {
  /** Text to run: the selection, or everything if there is no selection. */
  text: string;
  /** Index (JS) in the document where `text` starts. */
  from: number;
}

interface Props {
  value: string;
  onChange(value: string): void;
  engine: string | null;
  /** Known tables → columns (may be empty), for autocompletion. */
  tables: Record<string, string[]>;
  onRun(request: RunRequest): void;
  /** Error to underline, with its JS index in the document. */
  error: { index: number; message: string; source: object } | null;
}

const theme = EditorView.theme({
  "&": { height: "100%", fontSize: "13.5px", backgroundColor: "var(--editor-bg)", color: "var(--fg)" },
  ".cm-scroller": { fontFamily: 'ui-monospace, "Cascadia Mono", Consolas, monospace', lineHeight: "1.55" },
  ".cm-gutters": { backgroundColor: "var(--editor-bg)", color: "var(--muted)", border: "none" },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "var(--editor-active)" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--accent-soft) !important",
  },
  ".cm-tooltip": { backgroundColor: "var(--panel)", color: "var(--fg)", border: "1px solid var(--line)" },
});

function language(engine: string | null, tables: Record<string, string[]>) {
  return sql({ dialect: engine === "sqlite" ? SQLite : PostgreSQL, schema: tables, upperCaseKeywords: false });
}

export function SqlEditor({ value, onChange, engine, tables, onRun, error }: Props) {
  const { t } = useTranslation("editor");
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const locale = useRef(new Compartment());
  const callbacks = useRef({ onChange, onRun });
  callbacks.current = { onChange, onRun };

  useEffect(() => {
    if (!host.current) return;
    const run = (v: EditorView) => {
      const sel = v.state.selection.main;
      const text = sel.empty ? v.state.doc.toString() : v.state.sliceDoc(sel.from, sel.to);
      callbacks.current.onRun({ text, from: sel.empty ? 0 : sel.from });
      return true;
    };
    const editor = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          Prec.highest(keymap.of([{ key: "Mod-Enter", run }])),
          basicSetup,
          lang.current.of(language(engine, tables)),
          locale.current.of([
            EditorState.phrases.of(t("codeMirror", { returnObjects: true }) as Record<string, string>),
            EditorView.contentAttributes.of({ "aria-label": t("sqlEditor") }),
          ]),
          theme,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) callbacks.current.onChange(u.state.doc.toString());
          }),
        ],
      }),
    });
    view.current = editor;
    editor.focus();
    return () => editor.destroy();
    // The editor is created once; dialect and schema are reconfigured below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    view.current?.dispatch({ effects: lang.current.reconfigure(language(engine, tables)) });
  }, [engine, tables]);

  useEffect(() => {
    const editor = view.current;
    if (!editor) return;
    const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const searchOpen = searchPanelOpen(editor.state);
    const lintOpen = editor.dom.querySelector(".cm-panel-lint") !== null;
    const completing = completionStatus(editor.state) !== null;
    const oldFold = editor.state.phrase("Fold line");
    const oldUnfold = editor.state.phrase("Unfold line");
    const gotoInput = editor.dom.querySelector<HTMLInputElement>('.cm-goto-line input[name="line"]');
    const gotoState = gotoInput ? { value: gotoInput.value, start: gotoInput.selectionStart, end: gotoInput.selectionEnd } : null;
    const field = active instanceof HTMLInputElement && active.closest(".cm-search")
      ? { name: active.name, start: active.selectionStart, end: active.selectionEnd }
      : null;
    // CodeMirror creates panel labels once. Reopen just the panels to refresh their
    // labels; the document, undo history, selection and search query stay in state.
    if (searchOpen) closeSearchPanel(editor);
    if (lintOpen) closeLintPanel(editor);
    if (completing) closeCompletion(editor);
    editor.dispatch({ effects: locale.current.reconfigure([
      EditorState.phrases.of(t("codeMirror", { returnObjects: true }) as Record<string, string>),
      EditorView.contentAttributes.of({ "aria-label": t("sqlEditor") }),
    ]) });
    // Existing gutter markers and go-to-line dialogs keep their DOM when phrases
    // change. Refresh only their labels, leaving inputs, folding and focus intact.
    const goto = editor.dom.querySelector(".cm-goto-line");
    const gotoLabel = goto?.querySelector("label");
    if (gotoLabel?.firstChild?.nodeType === Node.TEXT_NODE) {
      gotoLabel.firstChild.textContent = `${editor.state.phrase("Go to line")}: `;
    }
    const gotoSubmit = goto?.querySelector('button[type="submit"]');
    if (gotoSubmit) gotoSubmit.textContent = editor.state.phrase("go");
    goto?.querySelector(".cm-dialog-close")?.setAttribute("aria-label", editor.state.phrase("close"));
    for (const marker of editor.dom.querySelectorAll<HTMLElement>(".cm-foldGutter span[title]")) {
      if (marker.title === oldFold) marker.title = editor.state.phrase("Fold line");
      else if (marker.title === oldUnfold) marker.title = editor.state.phrase("Unfold line");
    }
    for (const placeholder of editor.dom.querySelectorAll<HTMLElement>(".cm-foldPlaceholder")) {
      placeholder.title = editor.state.phrase("unfold");
      placeholder.setAttribute("aria-label", editor.state.phrase("folded code"));
    }
    if (lintOpen) openLintPanel(editor);
    if (searchOpen) openSearchPanel(editor);
    if (completing) startCompletion(editor);
    if (field) {
      const input = editor.dom.querySelector<HTMLInputElement>(`.cm-search input[name="${field.name}"]`);
      input?.focus({ preventScroll: true });
      if (input && field.start !== null && field.end !== null) input.setSelectionRange(field.start, field.end);
    } else if (active?.isConnected && document.activeElement !== active) {
      active.focus({ preventScroll: true });
    }
    // Changing phrases rebuilds CodeMirror's view plugins. The old showDialog
    // promise then closes its reconstructed panel in a microtask. Restore it with
    // a new dialog constructor after that cleanup has finished.
    let cancelled = false;
    const cancelRestore = (event: Event) => {
      const target = event.target;
      if (target instanceof Element && target.closest(".cm-goto-line") &&
          (event.type === "click" && target.closest(".cm-dialog-close") ||
           event instanceof KeyboardEvent && event.key === "Escape")) cancelled = true;
    };
    let timer: number | undefined;
    if (gotoState) {
      editor.dom.addEventListener("click", cancelRestore, true);
      editor.dom.addEventListener("keydown", cancelRestore, true);
      timer = window.setTimeout(() => {
        if (cancelled || view.current !== editor || !editor.dom.isConnected) return;
        const focused = document.activeElement instanceof HTMLElement ? document.activeElement : null;
        gotoLine(editor);
        const input = editor.dom.querySelector<HTMLInputElement>('.cm-goto-line input[name="line"]');
        if (input) {
          input.value = gotoState.value;
          if (gotoState.start !== null && gotoState.end !== null) input.setSelectionRange(gotoState.start, gotoState.end);
        }
        if (focused?.isConnected && focused !== document.body && !focused.closest(".cm-goto-line")) focused.focus({ preventScroll: true });
      }, 0);
    }
    return () => {
      cancelled = true;
      window.clearTimeout(timer);
      editor.dom.removeEventListener("click", cancelRestore, true);
      editor.dom.removeEventListener("keydown", cancelRestore, true);
    };
  }, [t]);

  useEffect(() => {
    const editor = view.current;
    if (!editor) return;
    const doc = editor.state.doc.toString();
    const diagnostics =
      error && error.index <= doc.length
        ? [{ from: error.index, to: Math.min(doc.length, wordEnd(doc, error.index)), severity: "error" as const, message: error.message }]
        : [];
    editor.dispatch(setDiagnostics(editor.state, diagnostics));
  }, [error]);

  useEffect(() => {
    const editor = view.current;
    if (!editor) return;
    const doc = editor.state.doc.toString();
    if (error && error.index <= doc.length) {
      editor.dispatch({ selection: { anchor: error.index }, scrollIntoView: true });
    }
    // Translation changes the displayed diagnostic, not the identity of the error.
    // Navigate only when an execution produces a new error or position.
  }, [error?.source, error?.index]);

  return <div ref={host} className="sql-editor" />;
}
