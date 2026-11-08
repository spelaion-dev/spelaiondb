// The command palette (Ctrl+Shift+P, SDB-18): one fuzzy box for every command of the registry
// (with its shortcut, so it teaches them), the connections, the objects the explorer has
// already read (catalog.ts: nothing is asked of a database here) and the recent files. Choosing
// an object lists its actions (open data, structure, new console, copy name…); Escape goes
// back, then closes.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { RecentFileItem } from "../api";
import { useCatalog } from "../catalog";
import { commandSpec, paletteCommands, type CommandId } from "../commandRegistry";
import {
  activeContext,
  commandEnabled,
  commandLabel,
  isCommandId,
  objectActions,
  runCommand,
  type PaletteObject,
  type PaletteObjectAction,
} from "../commands";
import { fuzzyMatch } from "../fuzzy";
import { openFilePath, recentFiles } from "../sqlFiles";
import { useApp } from "../store";

/** At most this many matches are listed: typing more narrows them. */
const MAX_RESULTS = 60;

type Entry =
  | { kind: "command"; key: string; id: CommandId; text: string; prefix: number; shortcut?: string }
  | { kind: "object"; key: string; item: PaletteObject; text: string; detail: string }
  | { kind: "file"; key: string; path: string; text: string; detail: string }
  | { kind: "action"; key: string; action: PaletteObjectAction; text: string };

interface Shown {
  entry: Entry;
  positions: number[];
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The text with the matched characters in bold; the first `muted` characters dimmed. */
function Highlighted({ text, positions, muted = 0 }: { text: string; positions: number[]; muted?: number }) {
  const chars = [...text];
  const marked = new Set(positions);
  const parts: { text: string; mark: boolean; muted: boolean }[] = [];
  chars.forEach((char, i) => {
    const mark = marked.has(i);
    const dim = i < muted;
    const last = parts[parts.length - 1];
    if (last && last.mark === mark && last.muted === dim) last.text += char;
    else parts.push({ text: char, mark, muted: dim });
  });
  return (
    <span className="palette-label">
      {parts.map((part, i) =>
        part.mark ? (
          <mark key={i} className={part.muted ? "muted" : undefined}>{part.text}</mark>
        ) : (
          <span key={i} className={part.muted ? "muted" : undefined}>{part.text}</span>
        ),
      )}
    </span>
  );
}

export function CommandPalette({ onClose }: { onClose(): void }) {
  const { t } = useTranslation(["app", "shell"]);
  const [query, setQuery] = useState("");
  const [chosen, setChosen] = useState<{ entry: Extract<Entry, { kind: "object" }>; query: string } | null>(null);
  const [active, setActive] = useState(0);
  const [error, setError] = useState<{ action: string; detail: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [recent, setRecent] = useState<RecentFileItem[]>([]);
  const list = useRef<HTMLUListElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const connections = useApp((s) => s.connections);
  const connected = useApp((s) => s.explorerConnected);
  const catalog = useCatalog((s) => s.schemas);
  // The palette is modal: the active tab cannot change while it is open.
  const context = useMemo(() => activeContext("palette"), []);

  useEffect(() => {
    let alive = true;
    void recentFiles().then((files) => alive && setRecent(files.filter((file) => file.exists)));
    return () => {
      alive = false;
    };
  }, []);

  const commandEntries = useMemo<Entry[]>(
    () =>
      paletteCommands()
        .filter((spec) => isCommandId(spec.id) && commandEnabled(spec.id, context))
        .map((spec) => {
          const id = spec.id as CommandId;
          const category = t(`commands.categories.${spec.category}`);
          return {
            kind: "command",
            key: `command:${id}`,
            id,
            text: `${category}: ${t(commandLabel(id, context))}`,
            prefix: category.length + 2,
            shortcut: commandSpec(id).shortcut,
          };
        }),
    [context, t],
  );

  const otherEntries = useMemo<Entry[]>(() => {
    const names = new Map(connections.map((c) => [c.id, c.name]));
    const entries: Entry[] = connections.map((connection) => ({
      kind: "object",
      key: `connection:${connection.id}`,
      item: { kind: "connection", connection },
      text: connection.name,
      detail: t("palette.connection"),
    }));
    for (const schema of Object.values(catalog)) {
      const connectionName = names.get(schema.connectionId);
      // Only what the explorer shows now: a connection still connected there.
      if (connectionName === undefined || !connected.includes(schema.connectionId)) continue;
      for (const object of schema.relations) {
        entries.push({
          kind: "object",
          key: `relation:${schema.connectionId}:${object.schema}:${object.name}`,
          item: { kind: "relation", connectionId: schema.connectionId, object },
          text: object.name,
          detail: `${t(`shell:kinds.${object.kind}`)} · ${object.schema} · ${connectionName}`,
        });
      }
      schema.others.forEach((object, index) => {
        entries.push({
          kind: "object",
          key: `other:${schema.connectionId}:${object.schema}:${object.kind}:${object.name}:${object.detail ?? ""}:${index}`,
          item: { kind: "schemaObject", connectionId: schema.connectionId, object },
          text: object.detail ? `${object.name}(${object.detail})` : object.name,
          detail: `${t(`shell:kinds.${object.kind}`)} · ${object.schema} · ${connectionName}`,
        });
      });
    }
    for (const file of recent) {
      entries.push({ kind: "file", key: `file:${file.path}`, path: file.path, text: file.name, detail: file.path });
    }
    return entries;
  }, [connections, connected, catalog, recent, t]);

  const actionEntries = useMemo<Entry[]>(
    () =>
      chosen
        ? objectActions(chosen.entry.item).map((action) => ({
            kind: "action",
            key: `action:${action.id}`,
            action,
            text: t(action.label),
          }))
        : [],
    [chosen, t],
  );

  const shown = useMemo<Shown[]>(() => {
    const pool = chosen ? actionEntries : commandEntries;
    if (query.trim() === "") return pool.map((entry) => ({ entry, positions: [] }));
    const scored: (Shown & { score: number; order: number })[] = [];
    const candidates = chosen ? pool : [...pool, ...otherEntries];
    candidates.forEach((entry, order) => {
      const match = fuzzyMatch(query, entry.text);
      if (match) scored.push({ entry, positions: match.positions, score: match.score, order });
    });
    // Best first; on a tie, commands before objects, then the registry's or catalog's order.
    scored.sort((a, b) => b.score - a.score || a.order - b.order);
    return scored.slice(0, MAX_RESULTS);
  }, [query, chosen, actionEntries, commandEntries, otherEntries]);

  useEffect(() => setActive(0), [query, chosen]);
  useEffect(() => {
    list.current?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: "nearest" });
  }, [active, shown]);

  function back() {
    if (!chosen) return;
    setQuery(chosen.query);
    setChosen(null);
    setError(null);
    input.current?.focus();
  }

  async function choose(item: Shown | undefined) {
    if (!item || busy) return;
    const { entry } = item;
    setError(null);
    switch (entry.kind) {
      case "command":
        // Run once the palette is gone: a command that focuses something (the explorer's
        // filter, a tab's name) or opens a dialog must not have the palette in its way.
        onClose();
        window.setTimeout(() => runCommand(entry.id, context), 0);
        break;
      case "file":
        onClose();
        window.setTimeout(() => void openFilePath(entry.path), 0);
        break;
      case "object":
        setChosen({ entry, query });
        setQuery("");
        break;
      case "action":
        // Stays open with the error if it fails (a clipboard refused, a definition not read).
        setBusy(true);
        try {
          await entry.action.run();
          onClose();
        } catch (e) {
          setError({ action: entry.text, detail: errorText(e) });
        } finally {
          setBusy(false);
        }
        break;
    }
  }

  const optionId = (index: number) => `palette-option-${index}`;
  const placeholder = chosen ? t("palette.actionPlaceholder", { name: chosen.entry.text }) : t("palette.placeholder");
  return (
    <div className="modal-backdrop palette-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div
        className="modal command-palette"
        role="dialog"
        aria-modal="true"
        aria-label={t("palette.title")}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            if (busy) return;
            if (chosen) back();
            else onClose();
          } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            if (shown.length === 0) return;
            const step = e.key === "ArrowDown" ? 1 : shown.length - 1;
            setActive((i) => (i + step) % shown.length);
          } else if (e.key === "PageDown" || e.key === "PageUp") {
            e.preventDefault();
            setActive((i) => Math.max(0, Math.min(shown.length - 1, i + (e.key === "PageDown" ? 8 : -8))));
          } else if (e.key === "Enter") {
            e.preventDefault();
            void choose(shown[active]);
          } else if (e.key === "Backspace" && chosen && query === "") {
            e.preventDefault();
            back();
          }
        }}
      >
        {chosen && (
          <div className="palette-context">
            <span className="palette-context-name">{chosen.entry.text}</span>
            <span className="muted small">{chosen.entry.detail}</span>
          </div>
        )}
        <input
          ref={input}
          className="palette-input"
          type="text"
          role="combobox"
          autoFocus
          spellCheck={false}
          autoComplete="off"
          aria-label={placeholder}
          aria-expanded="true"
          aria-controls="palette-results"
          aria-autocomplete="list"
          aria-activedescendant={shown[active] ? optionId(active) : undefined}
          placeholder={placeholder}
          value={query}
          readOnly={busy}
          onChange={(e) => setQuery(e.target.value)}
        />
        {error && (
          <p className="error small palette-error" role="alert">
            {t("palette.failed", { action: error.action, detail: error.detail })}
          </p>
        )}
        <ul id="palette-results" className="palette-results" role="listbox" ref={list} aria-label={t("palette.results")}>
          {shown.map((item, index) => {
            const { entry } = item;
            return (
              <li
                key={entry.key}
                id={optionId(index)}
                role="option"
                aria-selected={index === active}
                className={`palette-item${entry.kind === "action" && entry.action.danger ? " palette-danger" : ""}`}
                data-command={entry.kind === "command" ? entry.id : undefined}
                onMouseMove={() => setActive(index)}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => void choose(item)}
              >
                <Highlighted text={entry.text} positions={item.positions} muted={entry.kind === "command" ? entry.prefix : 0} />
                {entry.kind === "command" && entry.shortcut && <span className="menu-shortcut">{entry.shortcut}</span>}
                {(entry.kind === "object" || entry.kind === "file") && (
                  <span className="palette-detail muted small">
                    {entry.kind === "file" ? `${t("palette.recentFile")} · ${entry.detail}` : entry.detail}
                  </span>
                )}
              </li>
            );
          })}
        </ul>
        <p className="palette-hint muted small" role="status">
          {busy
            ? t("palette.running")
            : shown.length === 0
              ? chosen
                ? t("palette.noActions")
                : t("palette.nothing")
              : chosen
                ? t("palette.actionHint")
                : t("palette.hint")}
        </p>
      </div>
    </div>
  );
}
