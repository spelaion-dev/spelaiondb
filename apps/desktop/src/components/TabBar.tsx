// The tab bar: tabs in their order (pinned first), each with its menu, an inline rename, drag to
// reorder, and the Ctrl+Tab switcher over the most recently used tabs. What each action does is
// in `tabActions.ts`; the tabs themselves live in the store.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { closeQueryTab, defaultTabLabel, tabLabel } from "../connect";
import { topDialog } from "../modalFocus";
import { isModified } from "../sqlFiles";
import { startRename } from "../tabActions";
import { type Tab, useApp } from "../store";
import { FileMenuButton } from "./FileMenu";
import { Icon } from "./Icons";
import { commandSpec, type CommandId } from "../commandRegistry";
import { CommandMenuItem } from "./CommandMenuItem";

/** Distinguishes SQL, table data and table metadata without adding labels to the tab bar. */
function TabIcon({ kind }: { kind: "query" | "file" | "table" | "details" | "diagram" }) {
  return (
    <svg className="tab-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      {kind === "query" ? <><path d="m3 4 3 3-3 3M8 11h5" /></> :
        kind === "file" ? <><path d="M4 2.5h5.5L12.5 5.5v8H4z" /><path d="M9.5 2.5v3h3M6 9h4.5M6 11.5h3" /></> :
        kind === "table" ? <><rect x="2.5" y="3" width="11" height="10" rx="1" /><path d="M2.5 6.5h11M6.2 6.5V13M9.8 6.5V13" /></> :
        kind === "diagram" ? <><rect x="1.5" y="2" width="6" height="5" rx="1" /><rect x="8.5" y="9" width="6" height="5" rx="1" /><path d="M7.5 4.5h2.5v4.5" /></> :
          <><rect x="2.5" y="3" width="11" height="10" rx="1" /><path d="M6 3v10M8 6h3.5M8 9.5h3.5" /></>}
    </svg>
  );
}

function tabKind(tab: Tab): "query" | "file" | "table" | "details" | "diagram" {
  return tab.diagram ? "diagram" : tab.tableDetails ? "details" : tab.table ? "table" : tab.file ? "file" : "query";
}

/** Pixels the pointer moves before a press on a tab becomes a drag. */
const DRAG_THRESHOLD = 5;

interface Drag {
  id: string;
  pointer: number;
  startX: number;
  dragging: boolean;
  /** Index among the other tabs where it would land. */
  target: number;
}

export function TabBar({ hidden }: { hidden: boolean }) {
  const { t } = useTranslation(["app", "shell", "editor"]);
  const tabs = useApp((s) => s.tabs);
  const activeTab = useApp((s) => s.activeTab);
  const renaming = useApp((s) => s.renamingTab);
  const setActiveTab = useApp((s) => s.setActiveTab);
  const addTab = useApp((s) => s.addTab);
  const [menu, setMenu] = useState<{ id: string; x: number; y: number } | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const dragRef = useRef<Drag | null>(null);
  // A drag ends with a click on the tab it started on: that click does not count.
  const swallowClick = useRef(false);
  const bar = useRef<HTMLDivElement>(null);

  // The active tab stays in view, also when its name grows after it opened (a table tab gets
  // its table's name once its view is set) or it moves.
  const activeName = useApp((s) => {
    const tab = s.tabs.find((item) => item.id === s.activeTab);
    return tab ? `${tabLabel(tab)}|${s.tabs.indexOf(tab)}` : "";
  });
  useEffect(() => {
    if (activeTab && !hidden) document.getElementById(`workspace-${activeTab}`)?.scrollIntoView({ block: "nearest", inline: "nearest" });
  }, [activeTab, activeName, hidden]);

  const updateDrag = (next: Drag | null) => {
    dragRef.current = next;
    setDrag(next);
  };

  /** Where among the other tabs a tab dropped at `x` lands, kept inside its group. */
  function dropIndex(id: string, x: number): number {
    const others = tabs.filter((tab) => tab.id !== id);
    let index = 0;
    for (const tab of others) {
      const rect = document.getElementById(`workspace-${tab.id}`)?.getBoundingClientRect();
      if (rect && rect.left + rect.width / 2 < x) index += 1;
    }
    const dragged = tabs.find((tab) => tab.id === id);
    const pinned = others.filter((tab) => tab.pinned).length;
    return dragged?.pinned ? Math.min(index, pinned) : Math.max(index, pinned);
  }

  function focusTab(id: string) {
    setActiveTab(id);
    document.getElementById(`workspace-${id}`)?.focus();
  }

  function openMenuAt(id: string, element: HTMLElement) {
    const rect = element.getBoundingClientRect();
    setMenu({ id, x: rect.left, y: rect.bottom + 2 });
  }

  // Where the dragged tab would land: before one of the others, or after the last of them.
  const others = drag?.dragging ? tabs.filter((tab) => tab.id !== drag.id) : [];
  const dropBefore = drag?.dragging ? others[drag.target]?.id ?? null : null;
  const dropAfter = drag?.dragging && drag.target >= others.length ? others[others.length - 1]?.id ?? null : null;

  return (
    <div className="tab-strip" hidden={hidden}>
      <div className="tabbar" role="tablist" ref={bar}>
        {tabs.map((tab, index) => {
          const label = tabLabel(tab);
          const active = tab.id === activeTab;
          const running = tab.run?.running ?? false;
          const classes = [
            "tab",
            active ? "active" : "",
            tab.pinned ? "pinned" : "",
            drag?.dragging && drag.id === tab.id ? "dragging" : "",
            dropBefore === tab.id ? "drop-before" : "",
            dropAfter === tab.id ? "drop-after" : "",
          ].filter(Boolean).join(" ");
          return (
            <div
              key={tab.id}
              id={`workspace-${tab.id}`}
              role="tab"
              aria-selected={active}
              data-pinned={tab.pinned ? "true" : undefined}
              className={classes}
              tabIndex={active ? 0 : -1}
              title={[tab.title ? `${tab.title} (${defaultTabLabel(tab)})` : label, tab.file?.path].filter(Boolean).join("\n")}
              onClick={() => {
                if (swallowClick.current) {
                  swallowClick.current = false;
                  return;
                }
                setActiveTab(tab.id);
              }}
              onAuxClick={(e) => {
                // A middle click closes, like a browser tab; never a pinned one.
                if (e.button !== 1 || tab.pinned) return;
                e.preventDefault();
                void closeQueryTab(tab.id);
              }}
              onContextMenu={(e) => {
                e.preventDefault();
                setActiveTab(tab.id);
                setMenu({ id: tab.id, x: e.clientX, y: e.clientY });
              }}
              onPointerDown={(e) => {
                if (e.button !== 0 || renaming === tab.id) return;
                if ((e.target as Element).closest("button, input")) return;
                updateDrag({ id: tab.id, pointer: e.pointerId, startX: e.clientX, dragging: false, target: index });
              }}
              onPointerMove={(e) => {
                const current = dragRef.current;
                if (!current || current.id !== tab.id || current.pointer !== e.pointerId) return;
                if (!current.dragging) {
                  if (Math.abs(e.clientX - current.startX) < DRAG_THRESHOLD) return;
                  e.currentTarget.setPointerCapture(e.pointerId);
                }
                const target = dropIndex(tab.id, e.clientX);
                if (!current.dragging || target !== current.target) updateDrag({ ...current, dragging: true, target });
              }}
              onPointerUp={(e) => {
                const current = dragRef.current;
                if (!current || current.id !== tab.id) return;
                updateDrag(null);
                if (e.currentTarget.hasPointerCapture(e.pointerId)) e.currentTarget.releasePointerCapture(e.pointerId);
                if (!current.dragging) return;
                swallowClick.current = true;
                // The click after the drag may not come (the pointer left the tab): forget it soon.
                setTimeout(() => { swallowClick.current = false; }, 0);
                useApp.getState().moveTab(tab.id, current.target);
                setActiveTab(tab.id);
              }}
              onPointerCancel={() => updateDrag(null)}
              onDoubleClick={(e) => {
                if ((e.target as Element).closest("button, input")) return;
                startRename(tab.id);
              }}
              onKeyDown={(e) => {
                if (e.target !== e.currentTarget || tabs.length === 0) return;
                if (e.key === "F2" && !e.ctrlKey && !e.altKey && !e.shiftKey) {
                  e.preventDefault();
                  startRename(tab.id);
                  return;
                }
                if (e.key === "ContextMenu" || (e.key === "F10" && e.shiftKey)) {
                  e.preventDefault();
                  openMenuAt(tab.id, e.currentTarget);
                  return;
                }
                let next: number;
                if (e.key === "ArrowRight") next = (index + 1) % tabs.length;
                else if (e.key === "ArrowLeft") next = (index + tabs.length - 1) % tabs.length;
                else if (e.key === "Home") next = 0;
                else if (e.key === "End") next = tabs.length - 1;
                else return;
                e.preventDefault();
                const target = tabs[next];
                if (target) focusTab(target.id);
              }}
            >
              <TabIcon kind={tabKind(tab)} />
              {running ? (
                <span className="tab-running" role="img" aria-label={t("tabs.running")} title={t("tabs.running")} />
              ) : (
                <span className={`dot dot-${tab.status}`} role="img" aria-label={t(`editor:status.${tab.status}`)} title={t(`editor:status.${tab.status}`)} />
              )}
              {(tab.transaction?.state === "open" || tab.transaction?.state === "failed") && (
                <span className={`tab-tx${tab.transaction.state === "failed" ? " tab-tx-failed" : ""}`} title={t("editor:transaction.tabBadge")} aria-label={t("editor:transaction.tabBadge")}>TX</span>
              )}
              {renaming === tab.id ? (
                <RenameInput tab={tab} onDone={() => document.getElementById(`workspace-${tab.id}`)?.focus()} />
              ) : (
                <span className="tab-label">{label}</span>
              )}
              {isModified(tab) && (
                <span className="tab-unsaved" role="img" aria-label={t("tabs.unsaved")} title={t("tabs.unsaved")} />
              )}
              {tab.pinned ? (
                <button
                  className="close tab-unpin"
                  aria-label={t("tabs.unpinButton", { name: label })}
                  title={`${t("tabs.unpin")} (${commandSpec("tab.pin").shortcut})`}
                  onClick={(e) => {
                    e.stopPropagation();
                    useApp.getState().setPinned(tab.id, false);
                  }}
                >
                  <Icon name="pin" />
                </button>
              ) : (
                <button
                  className="close"
                  aria-label={t("closeTab", { name: label })}
                  onClick={(e) => {
                    e.stopPropagation();
                    void closeQueryTab(tab.id);
                  }}
                >
                  <Icon name="x" />
                </button>
              )}
            </div>
          );
        })}
      </div>
      <button className="new-tab" type="button" onClick={() => addTab(null)} title={t("newQueryShortcut")} aria-label={t("newQueryShortcut")}><Icon name="plus" /></button>
      <FileMenuButton />
      {menu && <TabMenu key={menu.id} {...menu} onClose={() => setMenu(null)} />}
      <TabSwitcher />
    </div>
  );
}

/** The tab's name, edited in place: Enter or leaving keeps it, Escape forgets it. */
function RenameInput({ tab, onDone }: { tab: Tab; onDone(): void }) {
  const { t } = useTranslation("app");
  const [value, setValue] = useState(tabLabel(tab));
  const input = useRef<HTMLInputElement>(null);
  const finished = useRef(false);
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);
  const finish = (keep: boolean) => {
    if (finished.current) return;
    finished.current = true;
    const app = useApp.getState();
    // The default name typed back is no name of the user's.
    if (keep) app.renameTab(tab.id, value === defaultTabLabel(tab) ? "" : value);
    app.setRenamingTab(null);
    onDone();
  };
  return (
    <input
      ref={input}
      className="tab-rename"
      aria-label={t("tabs.renameLabel", { name: tabLabel(tab) })}
      value={value}
      size={Math.max(8, Math.min(40, value.length + 1))}
      onChange={(e) => setValue(e.target.value)}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") {
          e.preventDefault();
          finish(true);
        } else if (e.key === "Escape") {
          e.preventDefault();
          finish(false);
        }
      }}
      onBlur={() => finish(true)}
    />
  );
}

function TabMenu({ id, x, y, onClose }: { id: string; x: number; y: number; onClose(): void }) {
  const { t } = useTranslation("app");
  const tab = useApp((s) => s.tabs.find((item) => item.id === id));
  const ref = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });

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
    const close = (e: MouseEvent) => {
      if (!(e.target as Element | null)?.closest(".tab-menu")) onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);

  if (!tab) return null;
  // Every item is a command of the registry (commandRegistry.ts), as in the command palette.
  const item = (command: CommandId) => <CommandMenuItem id={command} tab={id} onClose={onClose} />;
  return (
    <div
      ref={ref}
      className="grid-menu tab-menu"
      role="menu"
      aria-label={t("tabs.menu", { name: tabLabel(tab) })}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          onClose();
          document.getElementById(`workspace-${id}`)?.focus();
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
      {item("tab.rename")}
      {item("tab.duplicate")}
      {item("tab.pin")}
      {!tab.table && !tab.tableDetails && !tab.plan && !tab.diagram && (
        <>
          <hr />
          {tab.file && item("file.save")}
          {item("file.saveAs")}
          {tab.file && item("file.reload")}
        </>
      )}
      <hr />
      {item("tab.close")}
      {item("tab.closeOthers")}
      {item("tab.closeRight")}
      {item("tab.closeAll")}
      <hr />
      {item("tab.reopen")}
    </div>
  );
}

/** Tabs most recently used first; tabs never activated follow in bar order. */
function recentOrder(): string[] {
  const { tabs, recentTabs } = useApp.getState();
  const open = new Set(tabs.map((tab) => tab.id));
  const recent = recentTabs.filter((id) => open.has(id));
  return [...recent, ...tabs.map((tab) => tab.id).filter((id) => !recent.includes(id))];
}

/**
 * Ctrl+Tab: while Ctrl is held, Tab and Shift+Tab move through the tabs most recently used
 * first; releasing Ctrl switches to the chosen one, Escape keeps the current one. One quick
 * Ctrl+Tab goes back to the previous tab. A click on a row switches to it too.
 */
function TabSwitcher() {
  const { t } = useTranslation("app");
  const [state, setState] = useState<{ order: string[]; index: number } | null>(null);
  const ref = useRef(state);
  const tabs = useApp((s) => s.tabs);

  useEffect(() => {
    const update = (next: { order: string[]; index: number } | null) => {
      ref.current = next;
      setState(next);
    };
    const commit = (index?: number) => {
      const current = ref.current;
      if (!current) return;
      const id = current.order[index ?? current.index];
      update(null);
      if (id) useApp.getState().setActiveTab(id);
    };
    const step = (back: boolean) => {
      const current = ref.current;
      if (current) {
        const count = current.order.length;
        update({ ...current, index: (current.index + (back ? count - 1 : 1)) % count });
        return;
      }
      const order = recentOrder();
      if (order.length < 2) return;
      update({ order, index: back ? order.length - 1 : 1 });
    };
    const down = (e: KeyboardEvent) => {
      const mod = e.ctrlKey || e.metaKey;
      if (e.key === "Tab" && mod && !e.altKey) {
        // A modal dialog keeps the tabs behind it as they are (its own focus trap handles Tab).
        if (!ref.current && topDialog()) return;
        e.preventDefault();
        e.stopPropagation();
        step(e.shiftKey);
      } else if (ref.current) {
        let handled = true;
        if (e.key === "Escape") update(null);
        else if (e.key === "Enter") commit();
        else if (e.key === "ArrowDown") step(false);
        else if (e.key === "ArrowUp") step(true);
        else handled = false;
        if (handled) {
          e.preventDefault();
          e.stopPropagation();
        }
      }
    };
    const up = (e: KeyboardEvent) => {
      if ((e.key === "Control" || e.key === "Meta") && ref.current) commit();
    };
    const blur = () => update(null);
    window.addEventListener("keydown", down, true);
    window.addEventListener("keyup", up, true);
    window.addEventListener("blur", blur);
    return () => {
      window.removeEventListener("keydown", down, true);
      window.removeEventListener("keyup", up, true);
      window.removeEventListener("blur", blur);
    };
  }, []);

  if (!state) return null;
  return (
    <div className="tab-switcher" role="listbox" aria-label={t("tabs.switcher")}>
      {state.order.map((id, index) => {
        const tab = tabs.find((item) => item.id === id);
        if (!tab) return null;
        return (
          <div
            key={id}
            role="option"
            aria-selected={index === state.index}
            className={`tab-switcher-item${index === state.index ? " selected" : ""}`}
            onMouseDown={(e) => {
              e.preventDefault();
              ref.current = null;
              setState(null);
              useApp.getState().setActiveTab(id);
            }}
          >
            <TabIcon kind={tabKind(tab)} />
            <span>{tabLabel(tab)}</span>
            {tab.pinned && <Icon name="pin" />}
          </div>
        );
      })}
      <div className="tab-switcher-hint muted small">{t("tabs.switcherHint")}</div>
    </div>
  );
}
