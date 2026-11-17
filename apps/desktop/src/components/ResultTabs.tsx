// The result tabs of an editor tab (SDB-378): one per result the core keeps for it, with its
// name, its pin and its menu. Which results a run replaces, the names and the pins are the
// core's: this draws them and asks through the registry's commands, like the tab strip.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import type { CommandId } from "../commandRegistry";
import { runCommand } from "../commands";
import type { ResultMeta } from "../store";
import { CommandMenuItem } from "./CommandMenuItem";
import { Icon } from "./Icons";

/** The key that renames the focused result tab, as F2 renames a tab of the tab strip. */
export const RESULT_RENAME_KEY = "F2";

export function ResultTabList({
  results,
  activeResult,
  session,
  messages,
  onSelect,
}: {
  results: ResultMeta[];
  activeResult: number | null;
  /** The SQL editor tab whose results these are; `null` where result tabs have no actions
   *  (a table's data shows one page, replaced by every load). */
  session: string | null;
  /** The label of the messages tab. */
  messages: string;
  onSelect(id: number | null): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = new Intl.NumberFormat(i18n.language);
  const [menu, setMenu] = useState<{ result: number; x: number; y: number } | null>(null);
  const label = (result: ResultMeta, index: number) => result.name ?? t("results.result", { number: numbers.format(index + 1) });
  return (
    <div className="result-tab-list">
      {results.map((result, index) => {
        const active = result.id === activeResult;
        const name = label(result, index);
        return (
          <span key={result.id} className={`result-tab-item${active ? " active" : ""}`} data-result={result.id} data-pinned={result.pinned ? "true" : undefined}>
            <button
              className={`result-tab${active ? " active" : ""}`}
              aria-pressed={active}
              title={result.pinned ? t("resultTabs.pinnedTitle", { name }) : name}
              onClick={() => onSelect(result.id)}
              onContextMenu={(e) => {
                if (!session) return;
                e.preventDefault();
                // The menu's commands act on the tab's active result.
                onSelect(result.id);
                setMenu({ result: result.id, x: e.clientX, y: e.clientY });
              }}
              onKeyDown={(e) => {
                if (!session) return;
                if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
                  e.preventDefault();
                  onSelect(result.id);
                  const rect = e.currentTarget.getBoundingClientRect();
                  setMenu({ result: result.id, x: rect.left, y: rect.bottom + 2 });
                } else if (e.key === RESULT_RENAME_KEY && !e.ctrlKey && !e.metaKey && !e.altKey && !e.shiftKey) {
                  e.preventDefault();
                  onSelect(result.id);
                  // After the selection reaches the store: the command renames the active result.
                  setTimeout(() => runCommand("results.rename", { tab: session, source: "key" }), 0);
                }
              }}
            >
              {result.pinned && <Icon name="pin" />}
              <span className="result-tab-name">{name}</span>
              {result.pinned && <span className="sr-only">{t("resultTabs.pinned")}</span>}
            </button>
            {session && active && result.complete && (
              <button
                className="ghost result-tab-close"
                aria-label={t("resultTabs.closeNamed", { name })}
                title={t("resultTabs.close")}
                onClick={() => runCommand("results.close", { tab: session, source: "menu" })}
              >
                <Icon name="x" />
              </button>
            )}
          </span>
        );
      })}
      <button className={activeResult === null || !results.some((r) => r.id === activeResult) ? "active" : ""} onClick={() => onSelect(null)}>
        {messages}
      </button>
      {menu && session && (
        <ResultTabMenu
          key={menu.result}
          session={session}
          x={menu.x}
          y={menu.y}
          name={(() => {
            const at = results.findIndex((r) => r.id === menu.result);
            return at < 0 ? "" : label(results[at]!, at);
          })()}
          renamed={results.find((r) => r.id === menu.result)?.renamed ?? false}
          onClose={(focus) => {
            const result = menu.result;
            setMenu(null);
            if (focus) document.querySelector<HTMLButtonElement>(`.result-tab-item[data-result="${result}"] > .result-tab`)?.focus();
          }}
        />
      )}
    </div>
  );
}

/** A result tab's menu. Every item is a command of the registry, as in the command palette. */
function ResultTabMenu({ session, x, y, name, renamed, onClose }: { session: string; x: number; y: number; name: string; renamed: boolean; onClose(focus: boolean): void }) {
  const { t } = useTranslation("editor");
  const ref = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });
  const close = useRef(onClose);
  close.current = onClose;

  useLayoutEffect(() => {
    const rect = ref.current?.getBoundingClientRect();
    if (!rect) return;
    setPosition({
      left: Math.max(8, Math.min(x, window.innerWidth - rect.width - 8)),
      top: Math.max(8, Math.min(y, window.innerHeight - rect.height - 8)),
    });
    ref.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  }, [x, y]);

  useEffect(() => {
    const outside = (e: MouseEvent) => {
      if (!(e.target as Element | null)?.closest(".result-tab-menu")) close.current(false);
    };
    const blur = () => close.current(false);
    window.addEventListener("mousedown", outside);
    window.addEventListener("blur", blur);
    return () => {
      window.removeEventListener("mousedown", outside);
      window.removeEventListener("blur", blur);
    };
  }, []);

  const item = (command: CommandId) => <CommandMenuItem id={command} tab={session} onClose={() => onClose(false)} />;
  return createPortal(
    <div
      ref={ref}
      className="grid-menu tab-menu result-tab-menu"
      role="menu"
      aria-label={t("resultTabs.menu", { name })}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onKeyDown={(e) => {
        if (e.key === "Escape" || e.key === "Tab") {
          e.preventDefault();
          e.stopPropagation();
          onClose(true);
          return;
        }
        if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "Home" && e.key !== "End") return;
        e.preventDefault();
        const items = [...(ref.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
        const at = items.indexOf(document.activeElement as HTMLButtonElement);
        const next = e.key === "Home" ? 0
          : e.key === "End" ? items.length - 1
            : e.key === "ArrowDown" ? (at + 1) % items.length
              : (at + items.length - 1) % items.length;
        items[next]?.focus();
      }}
    >
      {item("results.rerun")}
      {item("results.pin")}
      {item("results.rename")}
      {/* Only when the user named it (SDB-407). */}
      {renamed && item("results.resetName")}
      <hr />
      {item("results.close")}
    </div>,
    document.body,
  );
}
