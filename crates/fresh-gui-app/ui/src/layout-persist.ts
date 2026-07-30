/**
 * Session layout blob helpers (v4).
 *
 * The Rust `SessionStore` owns the opaque layout string via `layout_set` /
 * `session_attached`; this module is the host schema + restore planner.
 * Explorer snapshot fields mirror Fresh `FileExplorerState` (`expanded_dirs`,
 * `scroll_offset`) without calling Fresh workspace disk I/O (ADE uses a host
 * VirtualTree + sandboxed FS).
 */

import { collectLeafIds, type PaneNode } from "./panes";

/** Current layout blob version written by `persistLayout`. */
export const LAYOUT_VERSION = 4;

export type ExplorerSnapshot = {
  /** Absolute directory paths that are expanded. */
  expanded: string[];
  scrollTop: number;
};

export type LayoutTerminalTab = {
  kind: "terminal";
  id: string;
  title: string;
  paneTree: PaneNode;
  activeLeafId: string;
  pinned?: boolean;
};

export type LayoutEditorTab = {
  kind: "editor";
  id: string;
  path: string;
  preview?: boolean;
  pinned?: boolean;
};

export type LayoutTab = LayoutTerminalTab | LayoutEditorTab;

export type LayoutBlob = {
  version?: number;
  activeTab?: number;
  sidebarWidth?: number;
  sidebarCollapsed?: boolean;
  tabs?: LayoutTab[];
  /** Per view-root explorer UI (Fresh FileExplorerState-inspired). */
  explorerByRoot?: Record<string, ExplorerSnapshot>;
};

export type PlannedTerminalRestore = {
  kind: "terminal";
  id: string;
  title: string;
  paneTree: PaneNode;
  activeLeafId: string;
  pinned: boolean;
  leafIds: string[];
};

export type PlannedEditorRestore = {
  kind: "editor";
  path: string;
  preview: boolean;
  pinned: boolean;
};

export type PlannedOrphanPty = {
  kind: "orphan";
  ptyId: string;
};

export type PlannedRestoreItem =
  | PlannedTerminalRestore
  | PlannedEditorRestore
  | PlannedOrphanPty;

export type SessionRestorePlan = {
  items: PlannedRestoreItem[];
  activeTab: number;
};

/** Normalize a view-root path for explorer snapshot keys. */
export function normalizeExplorerRootKey(path: string): string {
  const s = path.replace(/\\/g, "/").replace(/\/+$/, "");
  return s || "/";
}

/** Collect leaf id → cwd from a pane tree (when stamped during persist). */
export function leafCwdsFromTree(node: PaneNode): Map<string, string> {
  const out = new Map<string, string>();
  const walk = (n: PaneNode): void => {
    if (n.type === "leaf") {
      if (n.cwd) out.set(n.id, n.cwd);
      return;
    }
    for (const child of n.children) walk(child);
  };
  walk(node);
  return out;
}

/** Stamp leaf cwd values into a pane tree for persistence. */
export function stampLeafCwds(
  node: PaneNode,
  cwdByLeaf: ReadonlyMap<string, string | undefined>,
): PaneNode {
  if (node.type === "leaf") {
    const cwd = cwdByLeaf.get(node.id);
    return cwd ? { type: "leaf", id: node.id, cwd } : { type: "leaf", id: node.id };
  }
  return {
    ...node,
    children: node.children.map((c) => stampLeafCwds(c, cwdByLeaf)),
  };
}

/** Update one leaf's cwd inside a pane tree (immutably). */
export function setLeafCwdInTree(node: PaneNode, leafId: string, cwd: string): PaneNode {
  if (node.type === "leaf") {
    return node.id === leafId ? { type: "leaf", id: node.id, cwd } : node;
  }
  return {
    ...node,
    children: node.children.map((c) => setLeafCwdInTree(c, leafId, cwd)),
  };
}

/**
 * Plan how to rebuild tabs after `session_attach`.
 *
 * Terminal tabs whose leaf PTY ids are a subset of still-live PTYs are
 * restored in blob order (partitioning the live set). Leftover PTYs become
 * orphan single-pane tabs. Editor tabs are planned when `restoreEditors` is on.
 *
 * Accepts layout versions 2–4 (v2/v3 lacked explorer snapshots / multi-tab
 * partition restore historically, but the same tab list shape applies).
 */
export function planSessionRestore(
  blob: LayoutBlob,
  livePtyIds: readonly string[],
  opts: { restoreEditors?: boolean } = {},
): SessionRestorePlan {
  const version = blob.version ?? 0;
  const items: PlannedRestoreItem[] = [];
  const restoreEditors = opts.restoreEditors !== false;

  if ((version !== 2 && version !== 3 && version !== 4) || !Array.isArray(blob.tabs)) {
    for (const id of livePtyIds) items.push({ kind: "orphan", ptyId: id });
    return { items, activeTab: 0 };
  }

  const remaining = new Set(livePtyIds);

  for (const tb of blob.tabs) {
    if (tb.kind === "terminal") {
      let leafIds: string[];
      try {
        leafIds = collectLeafIds(tb.paneTree);
      } catch {
        continue;
      }
      if (leafIds.length === 0) continue;
      if (new Set(leafIds).size !== leafIds.length) continue;
      if (!leafIds.every((id) => remaining.has(id))) continue;
      for (const id of leafIds) remaining.delete(id);
      const activeLeafId = leafIds.includes(tb.activeLeafId) ? tb.activeLeafId : leafIds[0]!;
      items.push({
        kind: "terminal",
        id: tb.id || `term-${leafIds[0]}`,
        title: tb.title || "sh",
        paneTree: tb.paneTree,
        activeLeafId,
        pinned: !!tb.pinned,
        leafIds,
      });
    } else if (tb.kind === "editor" && restoreEditors && typeof tb.path === "string" && tb.path) {
      items.push({
        kind: "editor",
        path: tb.path,
        preview: !!tb.preview,
        pinned: !!tb.pinned,
      });
    }
  }

  for (const id of livePtyIds) {
    if (remaining.has(id)) {
      items.push({ kind: "orphan", ptyId: id });
      remaining.delete(id);
    }
  }

  const activeTab = typeof blob.activeTab === "number" && Number.isFinite(blob.activeTab)
    ? Math.max(0, Math.floor(blob.activeTab))
    : 0;

  return { items, activeTab };
}
