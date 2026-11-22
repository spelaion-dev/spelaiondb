// The app's one icon set, Lucide (`lucide-react`, ISC), behind one component (SDB-73,
// docs/ui-guidelines.md): 16px with Lucide's own stroke, 20px in the activity bar, in
// `currentColor`, hidden from assistive technology (the button around it carries the name).
// Each icon is imported on its own, so only the ones listed here reach the bundle.

import {
  ArrowUpRight,
  ChartColumn,
  ChevronLeft,
  ChevronRight,
  ChevronsLeft,
  ChevronsRight,
  Database,
  FolderOpen,
  Layers,
  Link,
  type LucideIcon,
  Maximize2,
  Minimize2,
  Minus,
  Network,
  Pin,
  Plus,
  RefreshCw,
  Scan,
  ShieldAlert,
  Sigma,
  Star,
  TableProperties,
  X,
} from "lucide-react";

/** The icons the app uses, by what they mean here. */
const ICONS = {
  x: X,
  plus: Plus,
  minus: Minus,
  pin: Pin,
  maximize: Maximize2,
  minimize: Minimize2,
  folderOpen: FolderOpen,
  link: Link,
  arrowUpRight: ArrowUpRight,
  sigma: Sigma,
  chevronRight: ChevronRight,
  chevronLeft: ChevronLeft,
  chevronsLeft: ChevronsLeft,
  chevronsRight: ChevronsRight,
  // A query plan: steps in a tree.
  plan: Network,
  chart: ChartColumn,
  scan: Scan,
  refresh: RefreshCw,
  // Rows stacked into groups.
  group: Layers,
  // A record shown as name and value columns.
  transpose: TableProperties,
  // The neutral mark of an engine without a licensed one (SDB-363).
  database: Database,
  shieldAlert: ShieldAlert,
  star: Star,
} satisfies Record<string, LucideIcon>;

export type IconName = keyof typeof ICONS;

/** Lucide's arrow-up-right as paths on its 24px grid, for the result grid's canvas, which
 *  draws a foreign key's mark itself and cannot hold a component. */
export const PATHS = {
  arrowUpRight: ["M7 7h10v10", "M7 17 17 7"],
};

/** One icon. `filled` fills its outline, for a toggle that is on (a default's star). The size is
 *  16, or 20 in the activity bar; the few other sizes left go as their areas move to Lucide. */
export function Icon({ name, size = 16, filled = false }: { name: IconName; size?: number; filled?: boolean }) {
  const Shape = ICONS[name];
  return <Shape className="icon" size={size} fill={filled ? "currentColor" : "none"} aria-hidden="true" focusable="false" />;
}
