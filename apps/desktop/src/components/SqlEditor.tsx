// SQL editor with CodeMirror 6: engine dialect, table autocompletion, Ctrl/Cmd+Enter to run
// the statement under the cursor (split by the core) or the selection, Ctrl/Cmd+Shift+Enter
// to run everything, Shift+Alt+F to format, and underlining of the error at its position.

import { useEffect, useImperativeHandle, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Compartment, countColumn, EditorSelection, EditorState, findColumn, Prec, StateEffect, StateField } from "@codemirror/state";
import { Decoration, type DecorationSet, EditorView, drawSelection, dropCursor, highlightActiveLine, highlightActiveLineGutter, highlightSpecialChars, keymap, lineNumbers } from "@codemirror/view";
import { MariaSQL, MSSQL, MySQL, PostgreSQL, SQLite, type SQLDialect, sql } from "@codemirror/lang-sql";
import { bracketMatching, defaultHighlightStyle, foldGutter, foldKeymap, HighlightStyle, indentOnInput, syntaxHighlighting, foldCode, unfoldCode, indentUnit } from "@codemirror/language";
import { defaultKeymap, history, historyKeymap, toggleLineComment, toggleBlockComment, cursorMatchingBracket, indentMore, indentLess } from "@codemirror/commands";
import { tags } from "@lezer/highlight";
import { closeLintPanel, lintKeymap, openLintPanel, setDiagnostics } from "@codemirror/lint";
import { closeSearchPanel, gotoLine, highlightSelectionMatches, openSearchPanel, SearchCursor, searchKeymap, searchPanelOpen } from "@codemirror/search";
import { autocompletion, closeBrackets, closeBracketsKeymap, closeCompletion, completionKeymap, completionStatus, startCompletion, snippet, nextSnippetField, prevSnippetField, acceptCompletion } from "@codemirror/autocomplete";
import { EDITOR_ACTIONS, EDITOR_SNIPPETS as snippets, quotedLines, type EditorActionId } from "../editorActions";
import { FORMAT_LANGUAGES, leadingCommas } from "../editorFormat";
import { usePreferences } from "../preferences";
import type { EditorPreferencesItem } from "../bindings";
import { EditorMenu } from "./EditorMenu";
import { call, commands } from "../api";
import { byteToIndex, indexToByte, wordEnd } from "../text";

export interface RunRequest {
  /** Text to run: a statement, the selection or everything. */
  text: string;
  /** Index (JS) in the document where `text` starts. */
  from: number;
}

/** What the toolbar can ask the editor to do. */
export interface SqlEditorHandle {
  action(id: EditorActionId): void;
  format(): void;
  runCurrent(): void;
  runScript(): void;
}

interface Props {
  handle?: React.Ref<SqlEditorHandle>;
  /** Formatting failed (the formatter could not read the SQL). */
  onFormatError(message: string): void;
  onActionError(message: string): void;
  value: string;
  /** Changes when `value` was replaced from outside the editor, which then shows it. */
  revision?: number;
  onChange(value: string): void;
  engine: string | null;
  /** Known tables → columns (may be empty), for autocompletion. */
  tables: Record<string, string[]>;
  onRun(request: RunRequest): void;
  /** Error to underline, with its JS index in the document. */
  error: { index: number; message: string; source: object } | null;
}

const theme = EditorView.theme({
  "&": { height: "100%", fontSize: "var(--sql-font-size, var(--text-md))", backgroundColor: "var(--editor-bg)", color: "var(--fg)" },
  ".cm-scroller": { fontFamily: "var(--sql-font, var(--font-code))", lineHeight: "var(--sql-line-height, var(--leading-md))" },
  ".cm-gutters": { backgroundColor: "var(--editor-bg)", color: "var(--muted)", border: "none" },
  ".cm-selectionMatch": { backgroundColor: "var(--accent-soft)" },
  ".cm-selectionMatch-main": { outline: "1px solid var(--accent)" },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "var(--editor-active)" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--accent-soft) !important",
  },
  ".cm-ran": { backgroundColor: "var(--accent-soft)", transition: "background-color 0.3s" },
  ".cm-tooltip": { backgroundColor: "var(--panel)", color: "var(--fg)", border: "1px solid var(--line)" },
});

// Retain the previous modified click so two Shift+Alt clicks define a rectangle.
// A normal click (or moving/typing with the keyboard) starts a new gesture.
const rectangleAnchors = new WeakMap<EditorView, { pos: number; doc: EditorState["doc"] }>();
const rectangleGesture = [
  EditorView.domEventObservers({
    mousedown: (event, view) => {
      if (!(event.button === 0 && event.shiftKey && event.altKey && !event.ctrlKey && !event.metaKey)) rectangleAnchors.delete(view);
    },
    keydown: (event, view) => {
      if (event.key !== "Alt" && event.key !== "Shift") rectangleAnchors.delete(view);
    },
  }),
  EditorView.mouseSelectionStyle.of((view, event) => {
    if (event.button !== 0 || !event.shiftKey || !event.altKey || event.ctrlKey || event.metaKey) return null;
    const clicked = view.posAtCoords({ x: event.clientX, y: event.clientY });
    if (clicked === null) return null;
    const previous = rectangleAnchors.get(view);
    let clickAnchor = previous?.doc === view.state.doc ? previous.pos : view.state.selection.main.head;
    let dragAnchor = clicked;
    rectangleAnchors.set(view, { pos: clicked, doc: view.state.doc });
    return {
      get(current) {
        const dragging = Math.abs(current.clientX - event.clientX) > 2 || Math.abs(current.clientY - event.clientY) > 2;
        if (dragging) rectangleAnchors.delete(view);
        const end = view.posAtCoords({ x: current.clientX, y: current.clientY }, false);
        if (end === null) return view.state.selection;
        const state = view.state;
        const start = dragging ? dragAnchor : clickAnchor;
        const first = state.doc.lineAt(start);
        const last = state.doc.lineAt(end);
        const startColumn = countColumn(first.text, state.tabSize, start - first.from);
        const endColumn = countColumn(last.text, state.tabSize, end - last.from);
        const ranges = [];
        const low = Math.min(first.number, last.number);
        const high = Math.max(first.number, last.number);
        for (let number = low; number <= high; number++) {
          const line = state.doc.line(number);
          const from = line.from + findColumn(line.text, Math.min(startColumn, endColumn), state.tabSize);
          const to = line.from + findColumn(line.text, Math.max(startColumn, endColumn), state.tabSize);
          ranges.push(startColumn <= endColumn ? EditorSelection.range(from, to) : EditorSelection.range(to, from));
        }
        return EditorSelection.create(ranges, last.number - low);
      },
      update(update) {
        if (update.docChanged) {
          clickAnchor = update.changes.mapPos(clickAnchor);
          dragAnchor = update.changes.mapPos(dragAnchor);
          rectangleAnchors.delete(view);
        }
      },
    };
  }),
];

// The same extensions as CodeMirror's basic setup, with VS Code's mouse gestures.
const editorSetup = [
  highlightActiveLineGutter(), highlightSpecialChars(), history(), foldGutter(),
  drawSelection(), dropCursor(), EditorState.allowMultipleSelections.of(true), indentOnInput(),
  syntaxHighlighting(defaultHighlightStyle, { fallback: true }), bracketMatching(), closeBrackets(),
  autocompletion(),
  rectangleGesture,
  EditorView.clickAddsSelectionRange.of((event) => (event.altKey && !event.shiftKey) || event.ctrlKey || event.metaKey),
  highlightActiveLine(), highlightSelectionMatches({ highlightWordAroundCursor: true }),
  keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...foldKeymap, ...completionKeymap, ...lintKeymap]),
];

/** Select the current word, or the selected text, everywhere in the document. */
function selectAllOccurrences(view: EditorView) {
  const { state } = view;
  const selected = state.selection.main;
  const word = selected.empty ? state.wordAt(selected.head) : null;
  const match = word ?? (selected.empty ? null : selected);
  if (!match) return false;
  const text = state.sliceDoc(match.from, match.to);
  const ranges = [];
  let main = 0;
  for (const cursor = new SearchCursor(state.doc, text); !cursor.next().done;) {
    const { from, to } = cursor.value;
    if (word) {
      const found = state.wordAt(from);
      if (!found || found.from !== from || found.to !== to) continue;
    }
    if (from === match.from && to === match.to) main = ranges.length;
    ranges.push(EditorSelection.range(from, to));
  }
  if (!ranges.length) return false;
  view.dispatch({ selection: EditorSelection.create(ranges, main), userEvent: "select.search.matches" });
  return true;
}

// Keep CodeMirror's existing colors and attach a stable class to keywords so the dark
// theme can override just their color, including when the theme changes at runtime.
const sqlKeywords = HighlightStyle.define([{ tag: tags.keyword, class: "sql-keyword" }]);

/** Marks the text that was just sent to run, for a moment. */
const flashRun = StateEffect.define<{ from: number; to: number } | null>();
const ranMark = Decoration.mark({ class: "cm-ran" });
const ranField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(marks, tr) {
    let next = marks.map(tr.changes);
    for (const e of tr.effects) {
      if (e.is(flashRun)) next = e.value && e.value.to > e.value.from ? Decoration.set([ranMark.range(e.value.from, e.value.to)]) : Decoration.none;
    }
    return next;
  },
  provide: (f) => EditorView.decorations.from(f),
});

function flash(view: EditorView, from: number, to: number) {
  view.dispatch({ effects: flashRun.of({ from, to }) });
  setTimeout(() => {
    if (view.dom.isConnected) view.dispatch({ effects: flashRun.of(null) });
  }, 1000);
}

/** Formats the selection, or everything, with the engine's dialect, in one undo step. The
 *  formatter is loaded on first use (it is large); `null` when it worked. */
async function formatIn(view: EditorView, engine: string | null, preferences: EditorPreferencesItem): Promise<string | null> {
  const sel = view.state.selection.main;
  const from = sel.empty ? 0 : sel.from;
  const to = sel.empty ? view.state.doc.length : sel.to;
  const before = view.state.doc;
  const text = view.state.sliceDoc(from, to);
  if (!text.trim()) return null;
  let formatted: string;
  try {
    const { format: formatSql } = await import("sql-formatter");
    formatted = formatSql(text, {
      language: (engine && FORMAT_LANGUAGES[engine]) || "sql",
      keywordCase: preferences.keywordCase as "preserve" | "upper" | "lower",
      tabWidth: preferences.indent,
      linesBetweenQueries: 1,
    });
    if (preferences.commaPosition === "leading") formatted = leadingCommas(formatted, engine);
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
  // Typing while the formatter loaded: the positions are stale, so leave the text alone.
  if (formatted !== text && view.state.doc === before) {
    view.dispatch({
      changes: { from, to, insert: formatted },
      selection: sel.empty ? undefined : { anchor: from, head: from + formatted.length },
      userEvent: "input.format",
      scrollIntoView: true,
    });
  }
  return null;
}

function language(engine: string | null, tables: Record<string, string[]>) {
  // DuckDB's SQL comes from PostgreSQL's: the default dialect fits it.
  const dialects: Record<string, SQLDialect> = { sqlite: SQLite, mysql: MySQL, mariadb: MariaSQL, sqlserver: MSSQL };
  return sql({ dialect: (engine && dialects[engine]) || PostgreSQL, schema: tables, upperCaseKeywords: false });
}

function editorPreferences(p: EditorPreferencesItem) {
  return [
    EditorState.tabSize.of(p.tabSize), indentUnit.of(" ".repeat(p.tabSize)),
    p.lineNumbers ? lineNumbers() : [], p.wordWrap ? EditorView.lineWrapping : [],
    EditorView.theme({ "&": { "--sql-font-size": `${p.fontSize}px`, "--sql-line-height": `${p.lineHeight}px`, "--sql-font": p.font === "system" ? "var(--font-code)" : `"${p.font}", monospace` } }),
  ];
}

export function SqlEditor({ handle, onFormatError, onActionError, value, revision, onChange, engine, tables, onRun, error }: Props) {
  const { t } = useTranslation("editor");
  const preferences = usePreferences((s) => s.editor);
  const settings = useRef(new Compartment());
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const locale = useRef(new Compartment());
  const callbacks = useRef({ onChange, onRun, engine, onFormatError, onActionError });
  callbacks.current = { onChange, onRun, engine, onFormatError, onActionError };
  const format = (v: EditorView) => {
    void formatIn(v, callbacks.current.engine, usePreferences.getState().editor).then((failure) => {
      if (failure) callbacks.current.onFormatError(failure);
    });
    return true;
  };
  const runAll = (v: EditorView) => {
    callbacks.current.onRun({ text: v.state.doc.toString(), from: 0 });
    return true;
  };
  // Use the same selection and dialect splitter for the toolbar and keyboard shortcut.
  const run = (v: EditorView) => {
    const sel = v.state.selection.main;
    if (!sel.empty) {
      callbacks.current.onRun({ text: v.state.sliceDoc(sel.from, sel.to), from: sel.from });
      return true;
    }
    const engine = callbacks.current.engine;
    if (!engine) return runAll(v);
    const doc = v.state.doc.toString();
    call(commands.statementAt(engine, doc, indexToByte(doc, sel.head)))
      .then((span) => {
        if (!span || v.state.doc.toString() !== doc) return;
        const from = byteToIndex(doc, span.start);
        const to = byteToIndex(doc, span.end);
        flash(v, from, to);
        callbacks.current.onRun({ text: doc.slice(from, to), from });
      })
      .catch((e: unknown) => console.error("statement_at", e));
    return true;
  };
  const insertSnippet = (v: EditorView, id: keyof typeof snippets, from = v.state.selection.main.from) => {
    const to = v.state.selection.main.to;
    const content = snippets[id];
    const lower = usePreferences.getState().editor.keywordCase === "lower";
    snippet(lower ? content.replace(/SELECT|FROM|INSERT INTO|VALUES|UPDATE|SET|WHERE/g, (word) => word.toLowerCase()) : content)(v, null, from, to);
    return true;
  };
  const currentStatement = (v: EditorView, copy: boolean) => {
    const doc = v.state.doc.toString();
    const selection = v.state.selection;
    void call(commands.statementAt(callbacks.current.engine ?? "postgres", doc, indexToByte(doc, selection.main.head))).then(async (span) => {
      if (!span || !v.dom.isConnected || v.state.doc.toString() !== doc || !v.state.selection.eq(selection)) return;
      const from = byteToIndex(doc, span.start), to = byteToIndex(doc, span.end);
      if (copy) await navigator.clipboard.writeText(doc.slice(from, to));
      else v.dispatch({ selection: { anchor: from, head: to }, scrollIntoView: true });
    }).catch((e: unknown) => callbacks.current.onActionError(e instanceof Error ? e.message : String(e)));
    return true;
  };
  const transform = (v: EditorView, change: (text: string) => string, wholeLine = false) => {
    v.dispatch(v.state.changeByRange((range) => {
      const target = range.empty ? (wholeLine ? v.state.doc.lineAt(range.head) : v.state.wordAt(range.head)) ?? range : range;
      const text = change(v.state.sliceDoc(target.from, target.to));
      return { changes: { from: target.from, to: target.to, insert: text }, range: EditorSelection.range(target.from, target.from + text.length) };
    }), { userEvent: "input.transform" });
    return true;
  };
  const action = (v: EditorView, id: EditorActionId): boolean => {
    switch (id) {
      case "lineComment": return toggleLineComment(v);
      case "blockComment": return toggleBlockComment(v);
      case "wordWrap": {
        const { editor, setEditor } = usePreferences.getState();
        void setEditor({ ...editor, wordWrap: !editor.wordWrap }).then((saved) => {
          const error = usePreferences.getState().error;
          if (!saved && error?.setting === "editor") callbacks.current.onActionError(error.detail);
        }); return true;
      }
      case "fold": return foldCode(v);
      case "unfold": return unfoldCode(v);
      case "matchingBracket": return cursorMatchingBracket(v);
      case "upperCase": return transform(v, (text) => text.toUpperCase());
      case "lowerCase": return transform(v, (text) => text.toLowerCase());
      case "quotedList": return transform(v, (text) => quotedLines(text, callbacks.current.engine), true);
      case "selectStatement": return currentStatement(v, false);
      case "copyStatement": return currentStatement(v, true);
      case "selectOccurrences": return selectAllOccurrences(v);
      case "insertSelect": case "insertInsert": case "insertUpdate": return insertSnippet(v, id);
    }
  };
  const tab = (v: EditorView) => {
    if (nextSnippetField(v)) return true;
    const range = v.state.selection.main;
    if (range.empty) {
      const prefix = /\b(sel|ins|upd)$/.exec(v.state.doc.lineAt(range.head).text.slice(0, range.head - v.state.doc.lineAt(range.head).from));
      if (prefix) return insertSnippet(v, prefix[1] === "sel" ? "insertSelect" : prefix[1] === "ins" ? "insertInsert" : "insertUpdate", range.head - prefix[1]!.length);
    }
    return acceptCompletion(v) || indentMore(v);
  };
  useImperativeHandle(handle, () => ({
    action: (id) => { if (view.current) { action(view.current, id); view.current.focus(); } },
    format: () => {
      if (view.current) {
        format(view.current);
        view.current.focus();
      }
    },
    runCurrent: () => { if (view.current) run(view.current); },
    runScript: () => { if (view.current) runAll(view.current); },
  }));

  useEffect(() => {
    if (!host.current) return;
    const editor = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        extensions: [
          Prec.highest(
            keymap.of([
              { key: "Mod-Shift-Enter", run: runAll },
              { key: "Mod-Enter", run },
              { key: "Shift-Alt-f", run: format },
              ...EDITOR_ACTIONS.map(({ id, key }) => ({ key, run: (v: EditorView) => action(v, id) })),
              { key: "Tab", run: tab },
              { key: "Shift-Tab", run: (v) => prevSnippetField(v) || indentLess(v) },
            ]),
          ),
          settings.current.of(editorPreferences(preferences)),
          ranField,
          editorSetup,
          syntaxHighlighting(defaultHighlightStyle),
          syntaxHighlighting(sqlKeywords),
          lang.current.of(language(engine, tables)),
          locale.current.of([
            EditorState.phrases.of(t("codeMirror", { returnObjects: true }) as Record<string, string>),
            EditorView.contentAttributes.of({ "aria-label": t("sqlEditor") }),
          ]),
          theme,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) callbacks.current.onChange(u.state.doc.toString());
            // Completions arrive asynchronously: one that lands after the editor lost focus
            // (a dialog opened meanwhile) would stay open on top of it.
            if (!u.view.hasFocus && completionStatus(u.state) !== null) {
              setTimeout(() => {
                if (!u.view.hasFocus) closeCompletion(u.view);
              });
            }
          }),
          EditorView.domEventHandlers({
            contextmenu: (event) => {
              event.preventDefault(); setMenu({ x: event.clientX, y: event.clientY }); return true;
            },
            blur: (_event, view) => {
              setTimeout(() => {
                if (!view.hasFocus) closeCompletion(view);
              });
            },
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
    view.current?.dispatch({ effects: settings.current.reconfigure(editorPreferences(preferences)) });
  }, [preferences]);

  // Text replaced from outside the editor (a file reloaded from disk, ADR-036): a new
  // `revision` makes the editor show `value`. Only then: a render that arrives late while the
  // user types must never put back older text.
  useEffect(() => {
    const editor = view.current;
    if (!editor || revision === undefined) return;
    const current = editor.state.doc.toString();
    if (current !== value) editor.dispatch({ changes: { from: 0, to: current.length, insert: value } });
    // Only a new revision replaces the text.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [revision]);

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

  return <>
    <div ref={host} className="sql-editor" onKeyDown={(event) => {
      if (event.key === "ContextMenu" || (event.shiftKey && event.key === "F10")) {
        event.preventDefault();
        const rect = host.current!.getBoundingClientRect(); setMenu({ x: rect.left, y: rect.top });
      }
    }} />
    {menu && <EditorMenu {...menu} onClose={() => { setMenu(null); view.current?.focus(); }} />}
  </>;
}
