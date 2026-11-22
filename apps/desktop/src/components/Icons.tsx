// The app's one icon set, Lucide (`lucide-react`, ISC), behind one component (SDB-73,
// docs/ui-guidelines.md): 16px with Lucide's own stroke, 20px in the activity bar, in
// `currentColor`, hidden from assistive technology (the button around it carries the name).
// Each icon is imported on its own, so only the ones listed here reach the bundle.

import {
  Activity,
  AlignLeft,
  ArrowDown,
  ArrowUp,
  ArrowUpRight,
  Bookmark,
  ChartColumn,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronsDownUp,
  ChevronsLeft,
  ChevronsRight,
  Circle,
  Columns3,
  Cpu,
  Database,
  FilePlus2,
  FileText,
  Folder,
  FolderOpen,
  GitFork,
  History,
  Info,
  KeyRound,
  Layers,
  Link,
  Link2,
  List,
  ListVideo,
  LoaderCircle,
  type LucideIcon,
  Maximize2,
  Menu,
  Minimize2,
  Minus,
  Network,
  Pin,
  Play,
  Plug,
  Plus,
  RectangleVertical,
  RefreshCw,
  Rows3,
  Save,
  Scan,
  Search,
  Settings,
  ShieldAlert,
  Sigma,
  SlidersHorizontal,
  Square,
  Star,
  Table,
  TableProperties,
  Terminal,
  TriangleAlert,
  Workflow,
  X,
  Zap,
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
  // A step's state: waiting, running, done, failed, cancelled (`minus` and `x` too).
  waiting: Circle,
  running: LoaderCircle,
  check: Check,
  stopped: Square,
  arrowUp: ArrowUp,
  arrowDown: ArrowDown,
  // The activity bar's views (the explorer is `database`).
  processes: Activity,
  search: Search,
  history: History,
  saved: Bookmark,
  mcp: Cpu,
  settings: Settings,
  // A tab's kind: a query, a SQL file, table data, a table's details, an ER diagram.
  query: Terminal,
  file: FileText,
  table: Table,
  details: Info,
  diagram: Workflow,
  // The editor's action column (cancel is `stopped`, filled).
  run: Play,
  runScript: ListVideo,
  runOptions: SlidersHorizontal,
  menu: Menu,
  format: AlignLeft,
  save: Save,
  saveAs: FilePlus2,
  warning: TriangleAlert,
  // The explorer: an expander, its header's tools and its nodes (a table is `table`, a node's
  // properties `details`).
  chevronDown: ChevronDown,
  collapseAll: ChevronsDownUp,
  plug: Plug,
  folder: Folder,
  columns: Columns3,
  column: RectangleVertical,
  key: KeyRound,
  foreignKey: Link2,
  indexes: List,
  trigger: Zap,
  partitions: Rows3,
  dependencies: GitFork,
} satisfies Record<string, LucideIcon>;

export type IconName = keyof typeof ICONS;

/** Lucide's arrow-up-right as paths on its 24px grid, for the result grid's canvas, which
 *  draws a foreign key's mark itself and cannot hold a component. */
export const PATHS = {
  arrowUpRight: ["M7 7h10v10", "M7 17 17 7"],
};

/** One icon. `filled` fills its outline, for a toggle that is on (a default's star). The size is
 *  16, or 20 in the activity bar; the few other sizes left go as their areas move to Lucide.
 *  `className` adds a place's own class (a tab's kind takes the tab's colour) to `icon`. */
export function Icon({ name, size = 16, filled = false, className }: { name: IconName; size?: number; filled?: boolean; className?: string }) {
  const Shape = ICONS[name];
  return <Shape className={className ? `icon ${className}` : "icon"} size={size} fill={filled ? "currentColor" : "none"} aria-hidden="true" focusable="false" />;
}
