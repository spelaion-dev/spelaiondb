// SQL editor with CodeMirror 6: engine dialect, table autocompletion, Ctrl/Cmd+Enter to run
// the statement under the cursor (split by the core) or the selection (several statements:
// the picker asks which), Ctrl/Cmd+Shift+Enter to run everything, Ctrl/Cmd+Alt+Enter to run
// from the cursor to the end, Shift+Alt+F to format, and underlining of the error at its
// position. What Run will send is the core's answer, asked again as the cursor moves.

import { useEffect, useImperativeHandle, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { Compartment, countColumn, EditorSelection, EditorState, findColumn, Prec, StateEffect, StateField } from "@codemirror/state";
import { Decoration, type DecorationSet, EditorView, drawSelection, dropCursor, highlightActiveLine, highlightActiveLineGutter, highlightSpecialChars, hoverTooltip, keymap, lineNumbers } from "@codemirror/view";
import { sql } from "@codemirror/lang-sql";
import { bracketMatching, defaultHighlightStyle, foldGutter, foldKeymap, HighlightStyle, indentOnInput, syntaxHighlighting, foldCode, unfoldCode, indentUnit } from "@codemirror/language";
import { defaultKeymap, history, historyKeymap, toggleLineComment, toggleBlockComment, cursorMatchingBracket, indentMore, indentLess } from "@codemirror/commands";
import { tags } from "@lezer/highlight";
import { closeLintPanel, lintKeymap, openLintPanel, setDiagnostics } from "@codemirror/lint";
import { closeSearchPanel, gotoLine, highlightSelectionMatches, openSearchPanel, searchKeymap, searchPanelOpen } from "@codemirror/search";
import { autocompletion, closeBrackets, closeBracketsKeymap, closeCompletion, completionKeymap, completionStatus, startCompletion, snippet, snippetCompletion, nextSnippetField, prevSnippetField, acceptCompletion, type CompletionContext, type CompletionResult } from "@codemirror/autocomplete";
import { occurrenceOrigin, selectAllOccurrences, selectNextOccurrence } from "../editorOccurrences";
import { EDITOR_ACTIONS, SNIPPET_COMMANDS, quotedLines, type EditorActionId } from "../editorActions";
import { allSnippets, lowerKeywords, snippetBefore, snippetsStarting, type EditorSnippet } from "../editorSnippets";
import { useSnippets } from "../snippetStore";
import { FORMAT_LANGUAGES, leadingCommas } from "../editorFormat";
import { resolveName, sqlCompletionSource, sqlDialect, type CompletionCatalog, type KeywordCase, type ResolvedName } from "../sqlCompletion";
import { formatHttp, httpLanguage, isHttpLanguage } from "../httpLanguage";
import { usePreferences, zoomed } from "../preferences";
import type { EditorPreferencesItem, RunKindItem } from "../bindings";
import { useDialogs } from "../dialogStore";
import { EditorMenu } from "./EditorMenu";
import { call, commands } from "../api";
import { byteToIndex, indexToByte, wordEnd } from "../text";

export interface RunRequest {
  /** Text to run: a statement, the selection or everything. */
  text: string;
  /** Index (JS) in the document where `text` starts. */
  from: number;
}

/** What Run will send, as the core answered for the cursor and selection. */
export interface RunLabel {
  kind: RunKindItem;
  /** Statements in it: a selection with several opens the picker. */
  statements: number;
}

/** How long the cursor rests before the core is asked what Run will send. */
const RUN_LABEL_DELAY_MS = 120;

/** What the toolbar can ask the editor to do. */
export interface SqlEditorHandle {
  action(id: EditorActionId): void;
  format(): void;
  runCurrent(): void;
  runScript(): void;
  currentRequest(): Promise<RunRequest | null>;
  /** Gives the editor the focus, its cursor where it was. */
  focus(): void;
  /** Puts the cursor at `index` (JS) of the text, shows it and takes the focus. */
  showAt(index: number): void;
}

/** What to do with the table a name stands for: open its details, or show it in the explorer. */
export type NameAction = "open" | "reveal";

/** Most columns a table's hover lists. */
const HOVER_COLUMNS = 24;

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
  /** Catalog metadata already read in the background, never fetched while typing. */
  catalog: CompletionCatalog;
  /** `newTab`: the results go next to the ones already there (SDB-378). */
  onRun(request: RunRequest, newTab?: boolean): void;
  /** What Run will send changed (the cursor, the selection, the text or the engine). */
  onRunLabel(label: RunLabel): void;
  /** Ctrl+click on a table's name, or "Go to table" / "Reveal in explorer" at the cursor. */
  onName(name: ResolvedName, action: NameAction): void;
  /** Shows or hides the outline of the script. */
  onOutline(): void;
  /** Error to underline, with its JS index in the document. */
  error: { index: number; message: string; source: object } | null;
  /** The editor opens with its cursor after the text instead of before it. */
  cursorAtEnd?: boolean;
}

const theme = EditorView.theme({
  "&": { height: "100%", fontSize: "var(--sql-font-size, var(--text-md))", backgroundColor: "var(--editor-bg)", color: "var(--fg)" },
  ".cm-scroller": { fontFamily: "var(--sql-font, var(--font-code))", lineHeight: "var(--sql-line-height, var(--leading-md))" },
  ".cm-gutters": { backgroundColor: "var(--editor-bg)", color: "var(--muted)", border: "none" },
  ".cm-selectionMatch": { backgroundColor: "var(--accent-soft)" },
  ".cm-selectionMatch-main": { backgroundColor: "transparent" },
  ".cm-activeLine, .cm-activeLineGutter": { backgroundColor: "var(--editor-active)" },
  "&.cm-focused .cm-cursor": { borderLeftColor: "var(--accent)" },
  "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
    backgroundColor: "var(--accent-soft) !important",
  },
  ".cm-ran": { backgroundColor: "var(--accent-soft)", transition: "background-color 0.3s" },
  ".cm-tooltip": { backgroundColor: "var(--panel)", color: "var(--fg)", border: "1px solid var(--line)" },
  ".cm-tooltip-autocomplete > ul > li[aria-selected]": { backgroundColor: "var(--accent-soft)", color: "var(--fg)" },
  ".cm-completionDetail": { color: "var(--muted)" },
  ".cm-tooltip-hover": { boxShadow: "var(--shadow-popover)", borderRadius: "var(--radius-md)" },
  ".cm-tooltip-autocomplete > ul > li[aria-selected] .cm-completionDetail": { color: "var(--fg)" },
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
  rectangleGesture, occurrenceOrigin,
  EditorView.clickAddsSelectionRange.of((event) => (event.altKey && !event.shiftKey) || event.ctrlKey || event.metaKey),
  highlightActiveLine(), highlightSelectionMatches({ highlightWordAroundCursor: true }),
  keymap.of([...closeBracketsKeymap, ...defaultKeymap, ...searchKeymap, ...historyKeymap, ...foldKeymap, ...completionKeymap, ...lintKeymap]),
];

// Keep CodeMirror's light colors and add stable classes for the dark theme. CSS switches
// them when the theme changes, without rebuilding the editor or its selection.
const sqlColors = HighlightStyle.define([
  { tag: tags.keyword, class: "sql-keyword" },
  { tag: tags.string, class: "sql-string" },
  { tag: [tags.literal, tags.typeName, tags.namespace], class: "sql-literal" },
]);

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
    if (isHttpLanguage(engine)) {
      formatted = formatHttp(text, preferences.indent);
    } else {
      const { format: formatSql } = await import("sql-formatter");
      formatted = formatSql(text, {
        language: (engine && FORMAT_LANGUAGES[engine]) || "sql",
        keywordCase: preferences.keywordCase as "preserve" | "upper" | "lower",
        tabWidth: preferences.indent,
        linesBetweenQueries: 1,
      });
      if (preferences.commaPosition === "leading") formatted = leadingCommas(formatted, engine);
    }
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

/** The hover of a name: a table with its columns and types, or a column with its type, whether
 *  it may be NULL and its comment. Built as DOM: CodeMirror shows it outside React. */
function nameTooltip(name: ResolvedName, t: (key: string, values?: Record<string, unknown>) => string): HTMLElement {
  const el = (tag: string, className: string, text?: string) => {
    const node = document.createElement(tag);
    node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  };
  const root = el("div", "sql-hover");
  root.setAttribute("role", "tooltip");
  const table = name.table;
  const qualified = `${table.schema}.${table.name}`;
  if (name.kind === "column") {
    const column = table.details?.find((c) => c.name === name.column);
    const head = el("div", "sql-hover-head");
    head.append(el("span", "sql-hover-name", name.column), el("span", "sql-hover-type", column?.type ?? ""));
    root.append(head);
    if (column) root.append(el("div", "sql-hover-note", t(column.nullable ? "names.nullable" : "names.notNull")));
    if (column?.comment) root.append(el("div", "sql-hover-comment", column.comment));
    root.append(el("div", "sql-hover-note", t("names.columnOf", { table: qualified })));
    return root;
  }
  const head = el("div", "sql-hover-head");
  head.append(el("span", "sql-hover-name", qualified));
  if (table.kind) head.append(el("span", "sql-hover-type", table.kind.replaceAll("_", " ")));
  root.append(head);
  if (!table.details) {
    root.append(el("div", "sql-hover-note", t("names.columnsLoading")));
    return root;
  }
  const list = el("div", "sql-hover-columns");
  for (const column of table.details.slice(0, HOVER_COLUMNS)) {
    list.append(el("span", "sql-hover-column", column.name), el("span", "sql-hover-type", column.type));
  }
  root.append(list);
  const more = table.details.length - HOVER_COLUMNS;
  if (more > 0) root.append(el("div", "sql-hover-note", t("names.moreColumns", { count: more })));
  root.append(el("div", "sql-hover-note", t("names.openHint")));
  return root;
}

/** A snippet's text as it is inserted: a built-in one follows the keyword case preference. */
function snippetText(chosen: EditorSnippet): string {
  return chosen.builtIn && usePreferences.getState().editor.keywordCase === "lower" ? lowerKeywords(chosen.body) : chosen.body;
}

/** Offers the snippets whose name starts with the word being typed, next to the SQL
 *  completions: the built-in ones and the user's own. Read when asked, so a snippet saved in
 *  Settings is offered at once. */
function snippetCompletionSource(describe: (snippet: EditorSnippet) => string) {
  return (context: CompletionContext): CompletionResult | null => {
    const word = context.matchBefore(/[A-Za-z_][A-Za-z0-9_]*/);
    if (!word || word.from === word.to) return null;
    // Not after a dot (a column of an alias) or inside a longer word.
    const before = context.state.sliceDoc(Math.max(0, word.from - 1), word.from);
    if (/[A-Za-z0-9_.]/.test(before)) return null;
    const offered = snippetsStarting(word.text, allSnippets(useSnippets.getState().snippets));
    if (!offered.length) return null;
    return {
      from: word.from,
      options: offered.map((chosen) => snippetCompletion(snippetText(chosen), { label: chosen.name, detail: describe(chosen), type: "text", boost: -1 })),
      validFor: /^[A-Za-z0-9_]*$/,
    };
  };
}

function language(engine: string | null, catalog: CompletionCatalog, keywordCase: KeywordCase, describe: (snippet: EditorSnippet) => string) {
  if (engine && isHttpLanguage(engine)) return httpLanguage(engine, catalog);
  return [sql({ dialect: sqlDialect(engine) }), autocompletion({ override: [sqlCompletionSource(catalog, engine, keywordCase), snippetCompletionSource(describe)] })];
}

function editorPreferences(p: EditorPreferencesItem) {
  return [
    EditorState.tabSize.of(p.tabSize), indentUnit.of(" ".repeat(p.tabSize)),
    p.lineNumbers ? lineNumbers() : [], p.wordWrap ? EditorView.lineWrapping : [],
    // The editor's zoom (SDB-389) multiplies both sizes; the interface size scales the result.
    EditorView.theme({ "&": { "--sql-font-size": `${zoomed(p.fontSize, p.zoom)}px`, "--sql-line-height": `${zoomed(p.lineHeight, p.zoom)}px`, "--sql-font": p.font === "system" ? "var(--font-code)" : `"${p.font}", monospace` } }),
  ];
}

export function SqlEditor({ handle, onFormatError, onActionError, value, revision, onChange, engine, catalog, onRun, onRunLabel, onName, onOutline, error, cursorAtEnd }: Props) {
  const { t } = useTranslation("editor");
  const preferences = usePreferences((s) => s.editor);
  const settings = useRef(new Compartment());
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const host = useRef<HTMLDivElement>(null);
  const view = useRef<EditorView | null>(null);
  const lang = useRef(new Compartment());
  const locale = useRef(new Compartment());
  const callbacks = useRef({ onChange, onRun, onRunLabel, engine, onFormatError, onActionError, catalog, onName, onOutline, t });
  callbacks.current = { onChange, onRun, onRunLabel, engine, onFormatError, onActionError, catalog, onName, onOutline, t };
  const failed = (e: unknown) => callbacks.current.onActionError(e instanceof Error ? e.message : String(e));
  // What a snippet is for, next to its name in the completion list: a built-in one's is
  // translated, the user's is their own text.
  const translate = useRef(t);
  translate.current = t;
  const describeSnippet = useRef((chosen: EditorSnippet) => (chosen.builtIn ? translate.current(chosen.description) : chosen.description)).current;
  // The Run button's label: the core says what Run will send. Asked once the cursor rests, so
  // typing never waits for it, and an answer to an older question is dropped.
  const label = useRef({ timer: undefined as number | undefined, asked: 0 });
  const askRunLabel = (v: EditorView, delay = RUN_LABEL_DELAY_MS) => {
    window.clearTimeout(label.current.timer);
    const asked = ++label.current.asked;
    label.current.timer = window.setTimeout(() => {
      if (!v.dom.isConnected) return;
      const doc = v.state.doc.toString(), sel = v.state.selection.main;
      call(commands.runTarget(callbacks.current.engine, doc, indexToByte(doc, sel.head), sel.empty ? null : indexToByte(doc, sel.from), sel.empty ? null : indexToByte(doc, sel.to)))
        .then((target) => {
          if (asked === label.current.asked) callbacks.current.onRunLabel({ kind: target.kind, statements: target.statements.length });
        })
        .catch((e: unknown) => console.error("run_target", e));
    }, delay);
  };
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
  // A selection with several statements (the core splits it as the run will): the picker asks
  // which to run, and the core writes the text of the ones chosen.
  const runSelection = async (v: EditorView, engine: string, selected: RunRequest, newTab = false) => {
    const doc = v.state.doc.toString(), selection = v.state.selection;
    const stale = () => !v.dom.isConnected || v.state.doc.toString() !== doc || !v.state.selection.eq(selection);
    const to = selected.from + selected.text.length;
    const target = await call(commands.runTarget(engine, doc, indexToByte(doc, selection.main.head), indexToByte(doc, selected.from), indexToByte(doc, to)));
    if (stale()) return;
    if (target.statements.length < 2) {
      callbacks.current.onRun(selected, newTab);
      return;
    }
    const statements = target.statements.map((span) => doc.slice(byteToIndex(doc, span.start), byteToIndex(doc, span.end)));
    const answer = await useDialogs.getState().askStatements(statements);
    if (answer === null || stale()) return;
    if (answer === "all" || answer.length === statements.length) {
      callbacks.current.onRun(selected, newTab);
      return;
    }
    const picked = await call(commands.pickStatements(engine, selected.text, answer));
    if (stale()) return;
    const from = selected.from + byteToIndex(selected.text, picked.span.start);
    flash(v, from, selected.from + byteToIndex(selected.text, picked.span.end));
    callbacks.current.onRun({ text: picked.text, from }, newTab);
  };
  // From the statement under the cursor to the end of the script: the core says where that
  // statement starts and writes the text. With no connection there is nothing to split with.
  const runFromCursor = (v: EditorView) => {
    const engine = callbacks.current.engine;
    if (!engine) return runAll(v);
    const doc = v.state.doc.toString();
    call(commands.statementsFromCursor(engine, doc, indexToByte(doc, v.state.selection.main.head)))
      .then((picked) => {
        if (!picked || !v.dom.isConnected || v.state.doc.toString() !== doc) return;
        const from = byteToIndex(doc, picked.span.start);
        flash(v, from, byteToIndex(doc, picked.span.end));
        callbacks.current.onRun({ text: picked.text, from });
      })
      .catch(failed);
    return true;
  };
  // Use the same selection and dialect splitter for the toolbar and keyboard shortcut.
  const run = (v: EditorView, newTab = false) => {
    const sel = v.state.selection.main;
    const engine = callbacks.current.engine;
    if (!sel.empty) {
      const selected = { text: v.state.sliceDoc(sel.from, sel.to), from: sel.from };
      if (engine) void runSelection(v, engine, selected, newTab).catch(failed);
      else callbacks.current.onRun(selected, newTab);
      return true;
    }
    if (!engine) {
      callbacks.current.onRun({ text: v.state.doc.toString(), from: 0 }, newTab);
      return true;
    }
    const doc = v.state.doc.toString();
    call(commands.statementAt(engine, doc, indexToByte(doc, sel.head)))
      .then((span) => {
        if (!span || v.state.doc.toString() !== doc) return;
        const from = byteToIndex(doc, span.start);
        const to = byteToIndex(doc, span.end);
        flash(v, from, to);
        callbacks.current.onRun({ text: doc.slice(from, to), from }, newTab);
      })
      .catch((e: unknown) => console.error("statement_at", e));
    return true;
  };
  // Inserts a snippet's text over the selection (or from `from`), its first field selected.
  // A snippet is only ever text put into the editor: nothing runs.
  const insertSnippet = (v: EditorView, chosen: EditorSnippet, from = v.state.selection.main.from) => {
    const to = v.state.selection.main.to;
    snippet(snippetText(chosen))(v, null, from, to);
    return true;
  };
  const insertNamed = (v: EditorView, name: string) => {
    const chosen = allSnippets(useSnippets.getState().snippets).find((candidate) => candidate.name === name);
    return chosen ? insertSnippet(v, chosen) : false;
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
  // What the name at `pos` is, in the catalog the editor already has (the one completion
  // uses): nothing is asked of the database for a click or a hover.
  const nameAt = (v: EditorView, pos: number): ResolvedName | null => {
    const { engine, catalog } = callbacks.current;
    if (engine && isHttpLanguage(engine)) return null;
    return resolveName(v.state.doc.toString(), pos, catalog, engine);
  };
  // "Go to table" and "Reveal in explorer": the table the name at the cursor stands for (a
  // column leads to its table).
  const nameAtCursor = (v: EditorView, how: NameAction) => {
    const name = nameAt(v, v.state.selection.main.head);
    if (name) callbacks.current.onName(name, how);
    else callbacks.current.onActionError(callbacks.current.t("names.unknown"));
    return true;
  };
  // The statement after or before the cursor's, as the core's splitter reads the script.
  const moveStatement = (v: EditorView, forward: boolean) => {
    const doc = v.state.doc.toString();
    call(commands.neighbourStatement(callbacks.current.engine ?? "postgres", doc, indexToByte(doc, v.state.selection.main.head), forward))
      .then((span) => {
        if (!span || !v.dom.isConnected || v.state.doc.toString() !== doc) return;
        v.dispatch({ selection: { anchor: byteToIndex(doc, span.start) }, scrollIntoView: true, userEvent: "select" });
      })
      .catch(failed);
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
  // A preference the editor changed from a key: why it could not be saved shows in the editor.
  const editorSaved = (saved: boolean) => {
    const error = usePreferences.getState().error;
    if (!saved && error?.setting === "editor") callbacks.current.onActionError(error.detail);
  };
  const action = (v: EditorView, id: EditorActionId): boolean => {
    switch (id) {
      case "runNewTab": return run(v, true);
      case "lineComment": return toggleLineComment(v);
      case "blockComment": return toggleBlockComment(v);
      case "wordWrap": {
        const { editor, setEditor } = usePreferences.getState();
        void setEditor({ ...editor, wordWrap: !editor.wordWrap }).then(editorSaved); return true;
      }
      case "zoomIn": void usePreferences.getState().zoomEditor(1).then(editorSaved); return true;
      case "zoomOut": void usePreferences.getState().zoomEditor(-1).then(editorSaved); return true;
      case "zoomReset": void usePreferences.getState().zoomEditor(0).then(editorSaved); return true;
      case "fold": return foldCode(v);
      case "unfold": return unfoldCode(v);
      case "matchingBracket": return cursorMatchingBracket(v);
      case "upperCase": return transform(v, (text) => text.toUpperCase());
      case "lowerCase": return transform(v, (text) => text.toLowerCase());
      case "quotedList": return transform(v, (text) => quotedLines(text, callbacks.current.engine), true);
      case "runFromCursor": return runFromCursor(v);
      case "selectStatement": return currentStatement(v, false);
      case "copyStatement": return currentStatement(v, true);
      case "selectOccurrences": return selectAllOccurrences(v);
      case "openObject": return nameAtCursor(v, "open");
      case "revealInExplorer": return nameAtCursor(v, "reveal");
      case "nextStatement": return moveStatement(v, true);
      case "previousStatement": return moveStatement(v, false);
      case "outline": callbacks.current.onOutline(); return true;
      case "insertSelect": case "insertInsert": case "insertUpdate": return insertNamed(v, SNIPPET_COMMANDS[id]);
    }
  };
  const tab = (v: EditorView) => {
    if (nextSnippetField(v)) return true;
    const range = v.state.selection.main;
    if (range.empty) {
      // The name of a snippet just before the cursor (a built-in one or the user's) expands.
      const line = v.state.doc.lineAt(range.head);
      const named = snippetBefore(line.text.slice(0, range.head - line.from), allSnippets(useSnippets.getState().snippets));
      if (named) return insertSnippet(v, named.snippet, range.head - named.start);
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
    currentRequest: async () => {
      const v = view.current;
      if (!v) return null;
      const doc = v.state.doc.toString(), selection = v.state.selection;
      if (!selection.main.empty) return { text: v.state.sliceDoc(selection.main.from, selection.main.to), from: selection.main.from };
      const span = await call(commands.statementAt(callbacks.current.engine ?? "postgres", doc, indexToByte(doc, selection.main.head)));
      if (!span || !v.dom.isConnected || v.state.doc.toString() !== doc || !v.state.selection.eq(selection)) return null;
      const from = byteToIndex(doc, span.start), to = byteToIndex(doc, span.end);
      flash(v, from, to);
      return { text: doc.slice(from, to), from };
    },
    runScript: () => { if (view.current) runAll(view.current); },
    focus: () => view.current?.focus(),
    showAt: (index) => {
      const v = view.current;
      if (!v) return;
      v.dispatch({ selection: { anchor: Math.min(index, v.state.doc.length) }, scrollIntoView: true, userEvent: "select" });
      v.focus();
    },
  }));

  useEffect(() => {
    if (!host.current) return;
    const editor = new EditorView({
      parent: host.current,
      state: EditorState.create({
        doc: value,
        // A new script that starts with the user's template: typing goes on after it.
        selection: cursorAtEnd ? { anchor: value.length } : undefined,
        extensions: [
          Prec.highest(
            keymap.of([
              { key: "Mod-d", run: selectNextOccurrence },
              { key: "Mod-Shift-Enter", run: runAll },
              { key: "Mod-Enter", run: (v) => run(v) },
              { key: "Shift-Alt-f", run: format },
              ...EDITOR_ACTIONS.map(({ id, key }) => ({ key, run: (v: EditorView) => action(v, id) })),
              // Zoom in also from the + key (its own on some layouts, the keypad's, Shift+=).
              { key: "Mod-+", run: (v) => action(v, "zoomIn") },
              { key: "Enter", run: acceptCompletion },
              { key: "Tab", run: tab },
              { key: "Shift-Tab", run: (v) => prevSnippetField(v) || indentLess(v) },
            ]),
          ),
          settings.current.of(editorPreferences(preferences)),
          ranField,
          editorSetup,
          syntaxHighlighting(defaultHighlightStyle),
          syntaxHighlighting(sqlColors),
          lang.current.of(language(engine, catalog, preferences.keywordCase as KeywordCase, describeSnippet)),
          locale.current.of([
            EditorState.phrases.of(t("codeMirror", { returnObjects: true }) as Record<string, string>),
            EditorView.contentAttributes.of({ "aria-label": t("sqlEditor") }),
          ]),
          theme,
          EditorView.updateListener.of((u) => {
            if (u.docChanged) callbacks.current.onChange(u.state.doc.toString());
            if (u.docChanged || u.selectionSet) askRunLabel(u.view);
            // Completions arrive asynchronously: one that lands after the editor lost focus
            // (a dialog opened meanwhile) would stay open on top of it.
            if (!u.view.hasFocus && completionStatus(u.state) !== null) {
              setTimeout(() => {
                if (!u.view.hasFocus) closeCompletion(u.view);
              });
            }
          }),
          // Hover: a table's columns and types, or a column's type and comment, from the
          // catalog already read. A name that is not there shows nothing.
          hoverTooltip((v, pos) => {
            const name = nameAt(v, pos);
            if (!name) return null;
            return { pos: name.from, end: name.to, above: true, create: () => ({ dom: nameTooltip(name, callbacks.current.t) }) };
          }, { hoverTime: 350 }),
          EditorView.domEventHandlers({
            // Ctrl / Cmd+click on a table's name (written, or through its alias) opens its
            // details. Anywhere else the click adds a cursor, as before.
            mousedown: (event, v) => {
              if (event.button !== 0 || !(event.ctrlKey || event.metaKey) || event.shiftKey || event.altKey) return false;
              const pos = v.posAtCoords({ x: event.clientX, y: event.clientY }, false);
              const name = nameAt(v, pos);
              if (!name) return false;
              event.preventDefault();
              callbacks.current.onName(name, "open");
              return true;
            },
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
    return () => {
      window.clearTimeout(label.current.timer);
      editor.destroy();
    };
    // The editor is created once; dialect and schema are reconfigured below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    view.current?.dispatch({ effects: lang.current.reconfigure(language(engine, catalog, preferences.keywordCase as KeywordCase, describeSnippet)) });
  }, [engine, catalog, preferences.keywordCase]);

  // Asked at once when the editor opens and when its engine changes: another engine reads the
  // same text otherwise, and with none only the whole script can be sent.
  useEffect(() => {
    if (view.current) askRunLabel(view.current, 0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [engine]);

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
