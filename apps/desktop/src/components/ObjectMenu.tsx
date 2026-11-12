// A context menu opened on an object (an explorer row, a table in the ER diagram): where it was
// opened when it fits, otherwise moved up or left to stay inside the window (SDB-246, SDB-255).
import { useRef, type ReactNode } from "react";
import { useMenuInWindow } from "../menuPlacement";

export function ObjectMenu({ at, className, children }: { at: { x: number; y: number }; className?: string; children: ReactNode }) {
  const ref = useRef<HTMLDivElement>(null);
  useMenuInWindow(ref, at.x, at.y);
  return (
    <div ref={ref} className={className ? `grid-menu object-menu ${className}` : "grid-menu object-menu"} role="menu" style={{ position: "fixed", left: at.x, top: at.y }}>
      {children}
    </div>
  );
}
