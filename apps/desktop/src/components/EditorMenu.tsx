// The SQL editor's menu (SDB-438): the right click, the Menu key and the ☰ button open the same
// one. Its groups, their order and its submenus are the registry's (`editorMenu()` in
// `commandRegistry.ts`); each item runs its command as the action column and the shortcuts do.

import { Fragment, useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { editorMenu, type CommandId } from "../commandRegistry";
import { commandShortcut } from "../shortcuts";
import { activeContext, commandEnabled, commandLabel, runCommand } from "../commands";
import { Submenu, moveInMenu } from "./Submenu";

export function EditorActionsButton({ runName }: { runName: string }) {
  const { t } = useTranslation("editor");
  const [open, setOpen] = useState<{ x: number; y: number } | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  return <>
    <button ref={button} className="editor-action editor-menu-button" aria-label={t("actions.menu")} title={t("actions.menu")} aria-haspopup="menu" aria-expanded={open !== null} onClick={() => {
      const rect = button.current!.getBoundingClientRect(); setOpen(open ? null : { x: rect.right, y: rect.top });
    }}>
      <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true"><path d="M4 6h16M4 12h16M4 18h16" /></svg>
    </button>
    {open && <EditorMenu {...open} runName={runName} onClose={() => { setOpen(null); button.current?.focus(); }} />}
  </>;
}

/** `runName`: Run named as the action column names it ("Run statement", "Run selection"…). */
export function EditorMenu({ x, y, runName, onClose }: { x: number; y: number; runName: string; onClose(): void }) {
  const { t } = useTranslation("editor");
  const root = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });
  const close = useRef(onClose); close.current = onClose;
  useLayoutEffect(() => {
    const rect = root.current!.getBoundingClientRect();
    setPosition({ left: Math.max(0, Math.min(x, window.innerWidth - rect.width)), top: Math.max(0, Math.min(y, window.innerHeight - rect.height)) });
    root.current?.querySelector<HTMLButtonElement>(":scope > button:not(:disabled)")?.focus();
  }, [x, y]);
  useEffect(() => {
    const outside = (event: MouseEvent) => { if (!root.current?.contains(event.target as Node) && !(event.target as Element).closest(".editor-menu-button")) close.current(); };
    const blur = () => close.current();
    window.addEventListener("mousedown", outside); window.addEventListener("blur", blur);
    return () => { window.removeEventListener("mousedown", outside); window.removeEventListener("blur", blur); };
  }, []);
  const context = activeContext("menu");
  const item = (id: CommandId) => {
    const label = id === "editor.run" ? runName : t(commandLabel(id, context));
    return (
      <button role="menuitem" data-command={id} aria-label={label} key={id} disabled={!commandEnabled(id, context)} onClick={() => { onClose(); runCommand(id, activeContext("menu")); }}>
        <span>{label}</span><span className="menu-shortcut">{commandShortcut(id)}</span>
      </button>
    );
  };
  return createPortal(<div ref={root} className="grid-menu tab-menu editor-menu" role="menu" aria-label={t("actions.menu")} style={{ position: "fixed", ...position }} onKeyDown={(event) => {
    if (event.key === "Escape" || event.key === "Tab") { event.preventDefault(); event.stopPropagation(); onClose(); return; }
    if (moveInMenu(root.current!, event.key)) event.preventDefault();
  }}>
    {editorMenu().map((group, index) => (
      <Fragment key={index}>
        {index > 0 && <hr />}
        {group.map((entry) => entry.kind === "command" ? item(entry.id) : (
          <Submenu key={entry.submenu} label={t(`editorMenu.${entry.submenu}`)} keyboard>
            {entry.items.map(({ id, section }, at) => (
              <Fragment key={id}>
                {section && at > 0 && <hr />}
                {item(id)}
              </Fragment>
            ))}
          </Submenu>
        ))}
      </Fragment>
    ))}
  </div>, document.body);
}
