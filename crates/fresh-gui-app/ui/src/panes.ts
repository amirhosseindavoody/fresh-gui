/** Pure pane-tree helpers (Terax-style; max 4 leaves per terminal tab). */

export const MAX_LEAVES_PER_TAB = 4;

export type SplitDir = "row" | "col";

export type PaneNode =
  | { type: "leaf"; id: string; cwd?: string }
  | { type: "split"; direction: SplitDir; children: PaneNode[]; sizes?: number[] };

export function leafCount(node: PaneNode): number {
  if (node.type === "leaf") return 1;
  return node.children.reduce((n, c) => n + leafCount(c), 0);
}

export function collectLeafIds(node: PaneNode): string[] {
  if (node.type === "leaf") return [node.id];
  return node.children.flatMap(collectLeafIds);
}

export function findLeaf(node: PaneNode, id: string): PaneNode | null {
  if (node.type === "leaf") return node.id === id ? node : null;
  for (const child of node.children) {
    const found = findLeaf(child, id);
    if (found) return found;
  }
  return null;
}

/** Replace a leaf with a split containing that leaf and a new leaf. */
export function splitLeaf(
  tree: PaneNode,
  targetId: string,
  newLeafId: string,
  direction: SplitDir,
): PaneNode | null {
  if (leafCount(tree) >= MAX_LEAVES_PER_TAB) return null;

  const splitAt = (node: PaneNode): PaneNode | null => {
    if (node.type === "leaf") {
      if (node.id !== targetId) return null;
      return {
        type: "split",
        direction,
        children: [node, { type: "leaf", id: newLeafId }],
        sizes: [0.5, 0.5],
      };
    }
    for (let i = 0; i < node.children.length; i++) {
      const next = splitAt(node.children[i]);
      if (next) {
        const children = node.children.slice();
        children[i] = next;
        return { ...node, children };
      }
    }
    return null;
  };

  return splitAt(tree);
}

/** Remove a leaf; promote the sibling if a split collapses to one child. */
export function removeLeaf(tree: PaneNode, targetId: string): PaneNode | null {
  const remove = (node: PaneNode): PaneNode | null | undefined => {
    if (node.type === "leaf") {
      return node.id === targetId ? null : node;
    }
    const nextChildren: PaneNode[] = [];
    for (const child of node.children) {
      const r = remove(child);
      if (r === undefined) return undefined;
      if (r !== null) nextChildren.push(r);
    }
    if (nextChildren.length === 0) return null;
    if (nextChildren.length === 1) return nextChildren[0];
    const sizes = node.sizes?.slice(0, nextChildren.length);
    return { ...node, children: nextChildren, sizes };
  };
  const result = remove(tree);
  return result === undefined ? tree : result;
}

export function nextLeafId(tree: PaneNode, activeId: string, delta: 1 | -1): string {
  const ids = collectLeafIds(tree);
  if (ids.length === 0) return activeId;
  const idx = ids.indexOf(activeId);
  const start = idx < 0 ? 0 : idx;
  const next = (start + delta + ids.length) % ids.length;
  return ids[next];
}

export function directionFromSplitMode(mode: "horizontal" | "vertical"): SplitDir {
  // horizontal = side-by-side = row of columns
  return mode === "horizontal" ? "row" : "col";
}
