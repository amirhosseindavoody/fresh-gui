/** Lazy, virtualized file tree with keyboard navigation (UI-2). */

import type { FsEntry } from "./protocol";
import { basename, escapeHtml } from "./dom";
import { fileIcon, iconSvg } from "./icons";

export type TreeListFn = (path: string) => Promise<{ path: string; entries: FsEntry[] }>;

export type TreeVisibility = {
  /** Show names starting with `.` (except `.git`). */
  showDotfiles: boolean;
  /** Show `.git` directories. Independent of `showDotfiles`. */
  showGitDirs: boolean;
};

/** Expanded dirs + scroll for one explorer view root (session persistence). */
export type ExplorerUiState = {
  expanded: string[];
  scrollTop: number;
};

export type TreeCallbacks = {
  onSelect?: (entry: FsEntry | null) => void;
  onOpenFile?: (entry: FsEntry, preview: boolean) => void;
  onStatus?: (text: string) => void;
  noteInteraction?: () => void;
  /** Fired after the displayed root path changes (cwd sync / reload). */
  onRootChange?: (rootPath: string) => void;
  /** Right-click on a tree row (file or directory). */
  onContextMenu?: (entry: FsEntry, clientX: number, clientY: number) => void;
  /** Fired when expanded dirs or scroll change (for session layout persistence). */
  onExplorerUiChange?: () => void;
};

export type SetViewRootOpts = {
  /** Restore expanded/scroll after re-root (issue #55 / Fresh FileExplorerState). */
  restore?: ExplorerUiState | null;
};

type Row = {
  path: string;
  name: string;
  kind: FsEntry["kind"];
  depth: number;
  expanded: boolean;
  hasChildren: boolean;
};

/** Keep in sync with `--tree-row-height` / indent tokens in `tokens.css`. */
const ROW_HEIGHT = 22;
const TREE_INDENT = 12;
const TREE_BASE = 8;

/** Whether an FS entry should appear in the explorer for the given visibility prefs. */
export function isTreeEntryVisible(entry: FsEntry, vis: TreeVisibility): boolean {
  if (entry.name === ".git") return vis.showGitDirs;
  if (entry.name.startsWith(".")) return vis.showDotfiles;
  return true;
}

function normalizePath(p: string): string {
  const s = p.replace(/\\/g, "/").replace(/\/+$/, "");
  return s || "/";
}

export class VirtualTree {
  private host: HTMLElement;
  private viewport: HTMLElement;
  private spacer: HTMLElement;
  private listFn: TreeListFn;
  private callbacks: TreeCallbacks;
  private childrenCache = new Map<string, FsEntry[]>();
  private visibility: TreeVisibility = { showDotfiles: false, showGitDirs: false };
  /** Path passed to `fs_list` for the view root (`""` = backend sandbox root). */
  private listRoot = "";
  private rootPath = "";
  private expanded = new Set<string>();
  private selected: string | null = null;
  private rows: Row[] = [];
  private scrollTop = 0;
  private raf = 0;
  private loadGen = 0;
  private viewRootToken = 0;
  private viewRootInFlight: string | null = null;
  /** Absolute sandbox root from the first `fs_list("")`. */
  private workspaceRoot = "";

  constructor(host: HTMLElement, listFn: TreeListFn, callbacks: TreeCallbacks = {}) {
    this.host = host;
    this.listFn = listFn;
    this.callbacks = callbacks;
    host.classList.add("vtree");
    host.tabIndex = 0;
    host.replaceChildren();

    this.viewport = document.createElement("div");
    this.viewport.className = "vtree-viewport";
    this.spacer = document.createElement("div");
    this.spacer.className = "vtree-spacer";
    this.viewport.appendChild(this.spacer);
    host.appendChild(this.viewport);

    this.viewport.addEventListener("scroll", () => {
      this.scrollTop = this.viewport.scrollTop;
      this.schedulePaint();
      this.callbacks.onExplorerUiChange?.();
    });
    host.addEventListener("keydown", (ev) => this.onKey(ev));
  }

  getExpandedPaths(): Set<string> {
    return new Set(this.expanded);
  }

  getScrollTop(): number {
    return this.scrollTop;
  }

  /** Snapshot expanded dirs + scroll for the current view root. */
  captureExplorerUi(): ExplorerUiState {
    return {
      expanded: [...this.expanded],
      scrollTop: this.scrollTop,
    };
  }

  getSelectedPath(): string | null {
    return this.selected;
  }

  getRootPath(): string {
    return this.rootPath;
  }

  getWorkspaceRoot(): string {
    return this.workspaceRoot;
  }

  /** Update explorer visibility prefs and refresh rows from the cached listings. */
  setVisibility(vis: TreeVisibility): void {
    this.visibility = {
      showDotfiles: !!vis.showDotfiles,
      showGitDirs: !!vis.showGitDirs,
    };
    this.rebuildRows();
    this.schedulePaint();
  }

  /** Map absolute cwd → fs_list path ("" at workspace root). */
  pathForFsList(absOrRel: string): string {
    const n = normalizePath(absOrRel);
    if (!this.workspaceRoot) return absOrRel;
    const root = normalizePath(this.workspaceRoot);
    if (n === root) return "";
    if (n.startsWith(root + "/")) return n.slice(root.length + 1);
    return absOrRel;
  }

  /**
   * Terax-style re-root. Isolated from `loadRoot` / fs_watch via viewRootInFlight.
   * Optional `restore` reapplies expanded dirs + scroll (per-cwd session persistence).
   */
  async setViewRoot(path: string, opts: SetViewRootOpts = {}): Promise<boolean> {
    const listPath = this.workspaceRoot ? this.pathForFsList(path) : path;
    const token = ++this.viewRootToken;
    this.viewRootInFlight = listPath;

    if (this.rootPath && this.workspaceRoot) {
      const wantAbs =
        listPath === ""
          ? normalizePath(this.workspaceRoot)
          : normalizePath(
              listPath.startsWith("/")
                ? listPath
                : `${normalizePath(this.workspaceRoot)}/${listPath}`,
            );
      if (normalizePath(this.rootPath) === wantAbs) {
        this.selected = this.rootPath;
        this.scrollIntoView(this.rootPath);
        this.schedulePaint();
        if (this.viewRootToken === token) this.viewRootInFlight = null;
        return true;
      }
    }

    const prevListRoot = this.listRoot;
    const prevRoot = this.rootPath;
    const prevExpanded = new Set(this.expanded);
    const prevCache = new Map(this.childrenCache);
    const prevRows = this.rows;
    const prevSelected = this.selected;
    const prevScrollTop = this.scrollTop;

    this.listRoot = listPath;
    this.expanded.clear();
    this.childrenCache.clear();
    this.selected = null;

    try {
      const listed = await this.listFn(listPath);
      if (token !== this.viewRootToken) return false;
      if (!this.workspaceRoot) this.workspaceRoot = listPath === "" ? listed.path : this.workspaceRoot;
      // If we re-rooted before the first workspace load, remember sandbox from listed if under path.
      if (!this.workspaceRoot && listed.path) {
        // Best-effort: keep listed.path's ancestor unknown; workspace set on next loadRoot("").
      }
      this.rootPath = listed.path;
      this.childrenCache.set("", listed.entries);

      const restore = opts.restore;
      if (restore && restore.expanded.length > 0) {
        this.expanded = new Set(
          restore.expanded.filter((p) => p !== "" && p !== this.rootPath),
        );
        await this.ensureExpandedLoaded();
        if (token !== this.viewRootToken) return false;
      }

      this.rebuildRows();
      this.selected = this.rootPath || null;
      const scrollTop = restore ? Math.max(0, restore.scrollTop || 0) : 0;
      this.viewport.scrollTop = scrollTop;
      this.scrollTop = this.viewport.scrollTop;
      this.schedulePaint();
      this.callbacks.onRootChange?.(this.rootPath);
      return true;
    } catch (err) {
      if (token !== this.viewRootToken) return false;
      this.listRoot = prevListRoot;
      this.rootPath = prevRoot;
      this.expanded = prevExpanded;
      this.childrenCache = prevCache;
      this.rows = prevRows;
      this.selected = prevSelected;
      this.viewport.scrollTop = prevScrollTop;
      this.scrollTop = prevScrollTop;
      this.schedulePaint();
      this.callbacks.onRootChange?.(this.rootPath);
      this.callbacks.onStatus?.(
        `explorer cwd failed: ${err instanceof Error ? err.message : String(err)}`,
      );
      return false;
    } finally {
      if (token === this.viewRootToken) this.viewRootInFlight = null;
    }
  }

  async loadRoot(opts: { silent?: boolean; keepExpanded?: Set<string> } = {}): Promise<void> {
    if (this.viewRootInFlight !== null) return;

    if (!opts.silent) {
      this.host.classList.add("loading");
    }
    const gen = ++this.loadGen;
    const listPath = this.listRoot;
    try {
      const listed = await this.listFn(listPath);
      if (gen !== this.loadGen || this.listRoot !== listPath || this.viewRootInFlight !== null) {
        return;
      }
      if (listPath === "") this.workspaceRoot = listed.path;
      this.rootPath = listed.path;
      this.childrenCache.set("", listed.entries);
      if (opts.keepExpanded) {
        this.expanded = new Set(
          [...opts.keepExpanded].filter((p) => p !== "" && p !== this.rootPath),
        );
      }
      await this.ensureExpandedLoaded();
      if (gen !== this.loadGen || this.listRoot !== listPath || this.viewRootInFlight !== null) {
        return;
      }
      this.rebuildRows();
      this.schedulePaint();
      this.callbacks.onRootChange?.(this.rootPath);
      if (!opts.silent) {
        this.callbacks.onStatus?.(`tree ${this.rootPath}`);
      }
    } finally {
      if (gen === this.loadGen) {
        this.host.classList.remove("loading");
      }
    }
  }

  clear(): void {
    this.listRoot = "";
    this.rootPath = "";
    this.workspaceRoot = "";
    this.viewRootInFlight = null;
    this.viewRootToken += 1;
    this.loadGen += 1;
    this.childrenCache.clear();
    this.expanded.clear();
    this.selected = null;
    this.rows = [];
    this.spacer.style.height = "0px";
    this.viewport.querySelectorAll(".vtree-row").forEach((el) => el.remove());
    this.callbacks.onRootChange?.("");
  }

  private scrollIntoView(path: string): void {
    const idx = this.rows.findIndex((r) => r.path === path);
    if (idx < 0) return;
    const top = idx * ROW_HEIGHT;
    if (top < this.viewport.scrollTop) this.viewport.scrollTop = top;
    if (top + ROW_HEIGHT > this.viewport.scrollTop + this.viewport.clientHeight) {
      this.viewport.scrollTop = top + ROW_HEIGHT - this.viewport.clientHeight;
    }
    this.scrollTop = this.viewport.scrollTop;
  }

  private async ensureExpandedLoaded(): Promise<void> {
    const pending = [...this.expanded];
    for (const path of pending) {
      if (!this.childrenCache.has(path)) {
        try {
          const listed = await this.listFn(path);
          this.childrenCache.set(path, listed.entries);
        } catch {
          this.expanded.delete(path);
        }
      }
    }
  }

  private rebuildRows(): void {
    const rows: Row[] = [];
    const rootLabel = this.rootPath ? basename(this.rootPath) || this.rootPath : "/";
    rows.push({
      path: this.rootPath || "",
      name: rootLabel,
      kind: "dir",
      depth: 0,
      expanded: true,
      hasChildren: true,
    });
    const walk = (parentKey: string, depth: number) => {
      const entries = this.childrenCache.get(parentKey) || [];
      for (const entry of entries) {
        if (!isTreeEntryVisible(entry, this.visibility)) continue;
        const isDir = entry.kind === "dir";
        const expanded = isDir && this.expanded.has(entry.path);
        rows.push({
          path: entry.path,
          name: entry.name,
          kind: entry.kind,
          depth,
          expanded,
          hasChildren: isDir,
        });
        if (expanded) walk(entry.path, depth + 1);
      }
    };
    walk("", 1);
    this.rows = rows;
    this.spacer.style.height = `${rows.length * ROW_HEIGHT}px`;
  }

  private schedulePaint(): void {
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = requestAnimationFrame(() => {
      this.raf = 0;
      this.paint();
    });
  }

  private paint(): void {
    const height = this.viewport.clientHeight || 300;
    const start = Math.max(0, Math.floor(this.scrollTop / ROW_HEIGHT) - 5);
    const end = Math.min(this.rows.length, Math.ceil((this.scrollTop + height) / ROW_HEIGHT) + 5);

    const existing = new Map<string, HTMLElement>();
    this.viewport.querySelectorAll<HTMLElement>(".vtree-row").forEach((el) => {
      const path = el.dataset.path || "";
      existing.set(path, el);
    });

    const keep = new Set<string>();
    for (let i = start; i < end; i++) {
      const row = this.rows[i];
      keep.add(row.path);
      let el = existing.get(row.path);
      if (!el) {
        el = document.createElement("div");
        el.className = "vtree-row";
        el.dataset.path = row.path;
        el.setAttribute("role", "treeitem");
        el.addEventListener("click", (ev) => {
          ev.preventDefault();
          const current = this.rows.find((r) => r.path === el!.dataset.path);
          if (current) void this.activateRow(current, false);
        });
        el.addEventListener("dblclick", (ev) => {
          ev.preventDefault();
          const current = this.rows.find((r) => r.path === el!.dataset.path);
          if (!current || current.kind !== "file") return;
          this.callbacks.noteInteraction?.();
          this.callbacks.onOpenFile?.(
            { name: current.name, path: current.path, kind: current.kind },
            false,
          );
        });
        el.addEventListener("contextmenu", (ev) => {
          ev.preventDefault();
          const current = this.rows.find((r) => r.path === el!.dataset.path);
          if (!current) return;
          this.callbacks.noteInteraction?.();
          this.selected = current.path;
          this.callbacks.onSelect?.(this.entryForRow(current));
          this.callbacks.onContextMenu?.(this.entryForRow(current), ev.clientX, ev.clientY);
          this.schedulePaint();
        });
        this.viewport.appendChild(el);
      }
      el.style.top = `${i * ROW_HEIGHT}px`;
      el.classList.toggle("selected", this.selected === row.path);
      el.classList.toggle("is-dir", row.kind === "dir");
      el.classList.toggle("expanded", row.kind === "dir" && row.expanded);
      el.dataset.depth = String(row.depth);
      el.dataset.kind = row.kind;
      if (row.kind === "dir") {
        el.setAttribute("aria-expanded", row.expanded ? "true" : "false");
      } else {
        el.removeAttribute("aria-expanded");
      }
      el.innerHTML = this.rowHtml(row);
    }

    for (const [path, el] of existing) {
      if (!keep.has(path)) el.remove();
    }
  }

  private rowHtml(row: Row): string {
    const indentWidth = Math.max(0, row.depth) * TREE_INDENT;
    const guides: string[] = [];
    for (let d = 0; d < row.depth; d++) {
      const left = TREE_BASE + d * TREE_INDENT + TREE_INDENT / 2;
      guides.push(`<span class="vtree-guide" style="left:${left}px"></span>`);
    }
    const icon = fileIcon(row.name, row.kind, row.expanded);
    const twist =
      row.kind === "dir"
        ? `<span class="vtree-twist" aria-hidden="true"></span>`
        : `<span class="vtree-twist is-leaf" aria-hidden="true"></span>`;
    return (
      `<span class="vtree-indent" style="width:${TREE_BASE + indentWidth}px">${guides.join("")}</span>` +
      twist +
      `<span class="vtree-icon ${escapeHtml(icon.tone ?? "")}" aria-hidden="true">${iconSvg(icon)}</span>` +
      `<span class="vtree-name">${escapeHtml(row.name)}</span>`
    );
  }

  private entryForRow(row: Row): FsEntry {
    return { name: row.name, path: row.path, kind: row.kind };
  }

  private async activateRow(row: Row, _fromKeyboard: boolean): Promise<void> {
    this.callbacks.noteInteraction?.();
    this.selected = row.path;
    this.callbacks.onSelect?.(this.entryForRow(row));
    this.callbacks.onStatus?.(`${row.kind}: ${row.path}`);

    if (row.path === this.rootPath || row.path === "") {
      this.schedulePaint();
      return;
    }

    if (row.kind === "file") {
      this.callbacks.onOpenFile?.(this.entryForRow(row), true);
      this.schedulePaint();
      return;
    }

    if (row.kind === "dir") {
      if (this.expanded.has(row.path)) {
        this.expanded.delete(row.path);
      } else {
        if (!this.childrenCache.has(row.path)) {
          try {
            const listed = await this.listFn(row.path);
            this.childrenCache.set(row.path, listed.entries);
          } catch (err) {
            this.callbacks.onStatus?.(
              `fs error: ${err instanceof Error ? err.message : String(err)}`,
            );
            this.schedulePaint();
            return;
          }
        }
        this.expanded.add(row.path);
      }
      this.rebuildRows();
      this.schedulePaint();
      this.callbacks.onExplorerUiChange?.();
    }
  }

  private selectedIndex(): number {
    if (!this.selected) return 0;
    const idx = this.rows.findIndex((r) => r.path === this.selected);
    return idx < 0 ? 0 : idx;
  }

  private focusIndex(idx: number): void {
    if (this.rows.length === 0) return;
    const i = Math.max(0, Math.min(this.rows.length - 1, idx));
    const row = this.rows[i];
    this.selected = row.path;
    this.callbacks.onSelect?.(this.entryForRow(row));
    this.scrollIntoView(row.path);
    this.schedulePaint();
  }

  private onKey(ev: KeyboardEvent): void {
    if (ev.key === "ArrowDown") {
      ev.preventDefault();
      this.focusIndex(this.selectedIndex() + 1);
    } else if (ev.key === "ArrowUp") {
      ev.preventDefault();
      this.focusIndex(this.selectedIndex() - 1);
    } else if (ev.key === "ArrowRight") {
      ev.preventDefault();
      const row = this.rows[this.selectedIndex()];
      if (row?.kind === "dir" && !this.expanded.has(row.path)) {
        void this.activateRow(row, true);
      } else if (row?.kind === "dir" && this.expanded.has(row.path)) {
        this.focusIndex(this.selectedIndex() + 1);
      }
    } else if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      const row = this.rows[this.selectedIndex()];
      if (row?.kind === "dir" && this.expanded.has(row.path)) {
        this.expanded.delete(row.path);
        this.rebuildRows();
        this.schedulePaint();
        this.callbacks.onExplorerUiChange?.();
      } else if (row && row.depth > 0) {
        for (let i = this.selectedIndex() - 1; i >= 0; i--) {
          if (this.rows[i].depth < row.depth) {
            this.focusIndex(i);
            break;
          }
        }
      }
    } else if (ev.key === "Enter") {
      ev.preventDefault();
      const row = this.rows[this.selectedIndex()];
      if (!row) return;
      if (row.kind === "file") {
        this.callbacks.noteInteraction?.();
        this.callbacks.onOpenFile?.(this.entryForRow(row), false);
      } else {
        void this.activateRow(row, true);
      }
    }
  }
}
