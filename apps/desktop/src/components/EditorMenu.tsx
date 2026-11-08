import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { menuCommands, type CommandId } from "../commandRegistry";
import { activeContext, commandEnabled, runCommand } from "../commands";

export function EditorActionsButton() {
  const { t } = useTranslation("editor");
  const [open, setOpen] = useState<{ x: number; y: number } | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  return <>
    <button ref={button} className="editor-action editor-menu-button" aria-label={t("actions.menu")} title={t("actions.menu")} aria-haspopup="menu" aria-expanded={open !== null} onClick={() => {
      const rect = button.current!.getBoundingClientRect(); setOpen(open ? null : { x: rect.right, y: rect.top });
    }}>
      <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" aria-hidden="true"><path d="M4 6h16M4 12h16M4 18h16" /></svg>
    </button>
    {open && <EditorMenu {...open} onClose={() => { setOpen(null); button.current?.focus(); }} />}
  </>;
}

export function EditorMenu({ x, y, onClose }: { x: number; y: number; onClose(): void }) {
  const { t } = useTranslation("editor");
  const root = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });
  const close = useRef(onClose); close.current = onClose;
  useLayoutEffect(() => {
    const rect = root.current!.getBoundingClientRect();
    setPosition({ left: Math.max(0, Math.min(x, window.innerWidth - rect.width)), top: Math.max(0, Math.min(y, window.innerHeight - rect.height)) });
    root.current?.querySelector<HTMLButtonElement>("button")?.focus();
  }, [x, y]);
  useEffect(() => {
    const outside = (event: MouseEvent) => { if (!root.current?.contains(event.target as Node) && !(event.target as Element).closest(".editor-menu-button")) close.current(); };
    const blur = () => close.current();
    window.addEventListener("mousedown", outside); window.addEventListener("blur", blur);
    return () => { window.removeEventListener("mousedown", outside); window.removeEventListener("blur", blur); };
  }, []);
  return createPortal(<div ref={root} className="grid-menu tab-menu editor-menu" role="menu" aria-label={t("actions.menu")} style={{ position: "fixed", ...position }} onKeyDown={(event) => {
    if (event.key === "Escape" || event.key === "Tab") { event.preventDefault(); event.stopPropagation(); onClose(); return; }
    if (!["ArrowDown", "ArrowUp", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const items = [...root.current!.querySelectorAll<HTMLButtonElement>("button")];
    const at = items.indexOf(document.activeElement as HTMLButtonElement);
    items[event.key === "Home" ? 0 : event.key === "End" ? items.length - 1 : (at + (event.key === "ArrowDown" ? 1 : items.length - 1)) % items.length]?.focus();
  }}>
    {menuCommands("editor").map((command) => <button role="menuitem" data-command={command.id} aria-label={t(command.label)} key={command.id} disabled={!commandEnabled(command.id as CommandId, activeContext("menu"))} onClick={() => { onClose(); runCommand(command.id as CommandId, activeContext("menu")); }}>
      <span>{t(command.label)}</span><span className="menu-shortcut">{command.shortcut}</span>
    </button>)}
  </div>, document.body);
}
