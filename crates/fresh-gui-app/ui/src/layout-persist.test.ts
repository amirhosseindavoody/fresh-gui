/**
 * Unit tests for session layout restore planning (issue #55).
 * Run: `bun test src/layout-persist.test.ts` from `crates/fresh-gui-app/ui`.
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  LAYOUT_VERSION,
  leafCwdsFromTree,
  normalizeExplorerRootKey,
  planSessionRestore,
  setLeafCwdInTree,
  stampLeafCwds,
  type LayoutBlob,
} from "./layout-persist";
import type { PaneNode } from "./panes";

describe("planSessionRestore", () => {
  it("partitions multiple terminal tabs across live PTYs", () => {
    const blob: LayoutBlob = {
      version: 4,
      activeTab: 1,
      tabs: [
        {
          kind: "terminal",
          id: "t1",
          title: "a",
          paneTree: { type: "leaf", id: "p1", cwd: "/tmp/a" },
          activeLeafId: "p1",
        },
        {
          kind: "terminal",
          id: "t2",
          title: "b",
          paneTree: {
            type: "split",
            direction: "row",
            children: [
              { type: "leaf", id: "p2" },
              { type: "leaf", id: "p3", cwd: "/tmp/b" },
            ],
          },
          activeLeafId: "p3",
        },
        { kind: "editor", id: "e1", path: "/tmp/readme.md", preview: true, pinned: true },
      ],
    };
    const plan = planSessionRestore(blob, ["p1", "p2", "p3", "p4"], { restoreEditors: true });
    assert.equal(plan.activeTab, 1);
    assert.equal(plan.items.length, 4);
    assert.equal(plan.items[0]?.kind, "terminal");
    assert.equal(plan.items[1]?.kind, "terminal");
    assert.equal(plan.items[2]?.kind, "editor");
    assert.equal(plan.items[3]?.kind, "orphan");
    if (plan.items[0]?.kind === "terminal") {
      assert.deepEqual(plan.items[0].leafIds, ["p1"]);
    }
    if (plan.items[1]?.kind === "terminal") {
      assert.deepEqual(plan.items[1].leafIds, ["p2", "p3"]);
      assert.equal(plan.items[1].activeLeafId, "p3");
    }
    if (plan.items[2]?.kind === "editor") {
      assert.equal(plan.items[2].path, "/tmp/readme.md");
      assert.equal(plan.items[2].preview, true);
      assert.equal(plan.items[2].pinned, true);
    }
    if (plan.items[3]?.kind === "orphan") {
      assert.equal(plan.items[3].ptyId, "p4");
    }
  });

  it("skips a terminal tab when any leaf PTY is missing", () => {
    const blob: LayoutBlob = {
      version: 3,
      tabs: [
        {
          kind: "terminal",
          id: "t1",
          title: "gone",
          paneTree: {
            type: "split",
            direction: "col",
            children: [{ type: "leaf", id: "alive" }, { type: "leaf", id: "dead" }],
          },
          activeLeafId: "alive",
        },
        {
          kind: "terminal",
          id: "t2",
          title: "ok",
          paneTree: { type: "leaf", id: "alive" },
          activeLeafId: "alive",
        },
      ],
    };
    const plan = planSessionRestore(blob, ["alive"]);
    assert.equal(plan.items.length, 1);
    assert.equal(plan.items[0]?.kind, "terminal");
    if (plan.items[0]?.kind === "terminal") {
      assert.equal(plan.items[0].id, "t2");
    }
  });

  it("falls back to orphans for unsupported versions", () => {
    const plan = planSessionRestore({ version: 1, tabs: [] }, ["a", "b"]);
    assert.deepEqual(plan.items, [
      { kind: "orphan", ptyId: "a" },
      { kind: "orphan", ptyId: "b" },
    ]);
  });

  it("omits editors when restoreEditors is false", () => {
    const blob: LayoutBlob = {
      version: LAYOUT_VERSION,
      tabs: [
        { kind: "terminal", id: "t", title: "t", paneTree: { type: "leaf", id: "p" }, activeLeafId: "p" },
        { kind: "editor", id: "e", path: "/x" },
      ],
    };
    const plan = planSessionRestore(blob, ["p"], { restoreEditors: false });
    assert.equal(plan.items.length, 1);
    assert.equal(plan.items[0]?.kind, "terminal");
  });
});

describe("leaf cwd helpers", () => {
  it("stamps and reads leaf cwds", () => {
    const tree: PaneNode = {
      type: "split",
      direction: "row",
      children: [{ type: "leaf", id: "a" }, { type: "leaf", id: "b" }],
    };
    const stamped = stampLeafCwds(tree, new Map([["a", "/tmp/a"], ["b", undefined]]));
    assert.deepEqual(leafCwdsFromTree(stamped), new Map([["a", "/tmp/a"]]));
    const updated = setLeafCwdInTree(stamped, "b", "/tmp/b");
    assert.deepEqual(leafCwdsFromTree(updated), new Map([
      ["a", "/tmp/a"],
      ["b", "/tmp/b"],
    ]));
  });
});

describe("normalizeExplorerRootKey", () => {
  it("strips trailing slashes", () => {
    assert.equal(normalizeExplorerRootKey("/tmp/work/"), "/tmp/work");
    assert.equal(normalizeExplorerRootKey("/"), "/");
  });
});
