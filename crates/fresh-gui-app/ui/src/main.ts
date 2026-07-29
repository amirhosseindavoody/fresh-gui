/** Phase UI-1 host shell: unified tabs, connection strip, status bar, CM6 + xterm WebGL. */
import type { EditorView } from "@codemirror/view";
import "./styles.css";
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type FsEntry,
  type ServerMessage,
} from "./protocol";
import {
  $,
  $button,
  $input,
  b64decode,
  b64encode,
  basename,
  escapeHtml,
} from "./dom";
import { createEditorView } from "./editor";
import { createTerminal, disposeTerminal, type TermBundle } from "./terminal";

const SESSION_KEY = "fresh-gui.sessionId";
const LAYOUT_KEY = "fresh-gui.layout";
const SIDEBAR_WIDTH_KEY = "fresh-gui.sidebarWidth";
const SIDEBAR_COLLAPSED_KEY = "fresh-gui.sidebarCollapsed";

interface Pending<T> {
  resolve: (value: T) => void;
  reject: (err: Error) => void;
}

interface OpenedInfo {
  buffer_id: string;
  path: string;
  language?: string;
}

interface PendingEditor extends Pending<OpenedInfo & { rev: number; text: string }> {
  _opened?: OpenedInfo;
}

interface TerminalTab {
  kind: "terminal";
  id: string;
  title: string;
  bundle: TermBundle;
}

interface EditorTab {
  kind: "editor";
  id: string;
  bufferId: string;
  path: string;
  rev: number;
  dirty: boolean;
  preview: boolean;
  view: EditorView;
  host: HTMLElement;
  suppressChange: boolean;
}

type Tab = TerminalTab | EditorTab;
type SplitMode = "horizontal" | "vertical";

interface LayoutBlob {
  activeTab?: number;
  split?: SplitMode | null;
  splitTab?: number;
  sidebarWidth?: number;
  sidebarCollapsed?: boolean;
  tabs?: Array<{ kind: "terminal"; id: string; title: string } | { kind: "editor"; id: string; path: string }>;
}

const connectionStrip = $("connection-strip");
const stripToggle = $button("strip-toggle");
const stripCompact = $("strip-compact");
const stripHost = $("strip-host");
const stripSession = $("strip-session");
const stripState = $("strip-state");
const connectBtn = $button("connect");
const disconnectBtn = $button("disconnect");
const workspaceEl = $("workspace");
const sidebarToggle = $button("sidebar-toggle");
const sidebarResizer = $("sidebar-resizer");
const treeEl = $("tree");
const tabsEl = $("tabs");
const tabPill = $("tab-pill");
const panesEl = $("panes");
const emptyStack = $("empty-stack");
const terminalStack = $("terminal-stack");
const editorStack = $("editor-stack");
const statusLeft = $("status-left");
const statusRight = $("status-right");
const editorSaveBtn = $button("editor-save");
const newTabBtn = $button("new-tab");
const splitHBtn = $button("split-h");
const splitVBtn = $button("split-v");
const splitOffBtn = $button("split-off");

const terminalPark = document.createElement("div");
terminalPark.className = "terminal-park";
terminalPark.style.cssText =
  "position:absolute;visibility:hidden;pointer-events:none;width:0;height:0;overflow:hidden";
terminalStack.appendChild(terminalPark);

let ws: WebSocket | null = null;
let sessionId: string | null = null;
let reqSeq = 0;
let hasEditor = false;
let watchId: string | null = null;
let stripForceExpanded = false;
let connected = false;

const pendingFs = new Map<string, Pending<{ path: string; entries: FsEntry[] }>>();
const pendingEditor = new Map<string, PendingEditor>();
const pendingEdit = new Map<string, Pending<number>>();
const pendingSave = new Map<string, Pending<{ path: string; rev: number }>>();

let selectedPath: string | null = null;
let treeRefreshTimer: ReturnType<typeof setTimeout> | null = null;
let treeLoading = false;
let treeLoaded = false;
let treeNeedsRefresh = false;
const expandedPaths = new Set<string>();
let treeQuietUntil = 0;

const WATCH_IGNORE_DIRS = new Set([
  ".git",
  "target",
  ".pixi",
  "node_modules",
  "vendor",
  ".cursor",
  "dist",
]);

let tabs: Tab[] = [];
let activeTabIndex = 0;
let splitMode: SplitMode | null = null;
/** Unified tab index of the second terminal pane when split. */
let splitTabIndex = 1;
let pendingSplit: SplitMode | null = null;

function isMod(ev: KeyboardEvent): boolean {
  return ev.metaKey || ev.ctrlKey;
}

function setStatusLeft(text: string): void {
  statusLeft.textContent = text;
  statusLeft.title = text;
}

function updateStatusRight(): void {
  const caps: string[] = [];
  if (connected) caps.push("online");
  if (hasEditor) caps.push("editor");
  if (connected) caps.push("fs");
  if (watchId) caps.push("watch");
  const dirty = tabs.filter((t) => t.kind === "editor" && t.dirty).length;
  if (dirty > 0) caps.push(`${dirty} dirty`);
  statusRight.textContent = caps.join(" · ");
}

function updateStripChips(): void {
  const url = $input("url").value.trim();
  stripHost.textContent = url || "—";
  stripHost.title = url;
  stripSession.textContent = sessionId ? sessionId.slice(0, 12) + (sessionId.length > 12 ? "…" : "") : "—";
  stripSession.title = sessionId || "";
  stripState.textContent = connected ? "connected" : ws ? "connecting" : "disconnected";
  stripState.classList.toggle("connected", connected);
}

function updateStripLayout(): void {
  if (!connected) {
    connectionStrip.classList.remove("compact");
    connectionStrip.classList.add("expanded");
    stripToggle.hidden = true;
    stripCompact.hidden = true;
    return;
  }
  stripToggle.hidden = false;
  stripCompact.hidden = false;
  updateStripChips();
  if (stripForceExpanded) {
    connectionStrip.classList.remove("compact");
    connectionStrip.classList.add("expanded");
  } else {
    connectionStrip.classList.add("compact");
    connectionStrip.classList.remove("expanded");
  }
}

function expandStrip(): void {
  stripForceExpanded = true;
  updateStripLayout();
}

function compactStrip(): void {
  stripForceExpanded = false;
  updateStripLayout();
}

function applySidebarWidth(width: number): void {
  const w = Math.max(160, Math.min(480, width));
  document.documentElement.style.setProperty("--sidebar-width", `${w}px`);
  localStorage.setItem(SIDEBAR_WIDTH_KEY, String(w));
}

function isSidebarCollapsed(): boolean {
  return workspaceEl.classList.contains("sidebar-collapsed");
}

function setSidebarCollapsed(collapsed: boolean): void {
  workspaceEl.classList.toggle("sidebar-collapsed", collapsed);
  localStorage.setItem(SIDEBAR_COLLAPSED_KEY, collapsed ? "1" : "0");
  sidebarToggle.textContent = collapsed ? "›" : "‹";
}

function toggleSidebar(): void {
  setSidebarCollapsed(!isSidebarCollapsed());
  persistLayout();
  requestAnimationFrame(measurePill);
}

function loadSidebarPrefs(): void {
  const w = localStorage.getItem(SIDEBAR_WIDTH_KEY);
  if (w) applySidebarWidth(Number.parseInt(w, 10) || 260);
  setSidebarCollapsed(localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === "1");
}

function readLayoutBlob(): LayoutBlob {
  try {
    const raw = localStorage.getItem(LAYOUT_KEY);
    if (!raw) return {};
    return JSON.parse(raw) as LayoutBlob;
  } catch {
    return {};
  }
}

function persistLayout(): void {
  const layout: LayoutBlob = {
    activeTab: activeTabIndex,
    split: splitMode,
    splitTab: splitTabIndex,
    sidebarWidth: Number.parseInt(
      getComputedStyle(document.documentElement).getPropertyValue("--sidebar-width") || "260",
      10,
    ),
    sidebarCollapsed: isSidebarCollapsed(),
    tabs: tabs.map((t) =>
      t.kind === "terminal"
        ? { kind: "terminal", id: t.id, title: t.title }
        : { kind: "editor", id: t.id, path: t.path },
    ),
  };
  const json = JSON.stringify(layout);
  localStorage.setItem(LAYOUT_KEY, json);
  if (sessionId) send({ type: "layout_set", layout: json });
}

function send(msg: ClientMessage): void {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

function nextRequestId(): string {
  reqSeq += 1;
  return `ui-${reqSeq}`;
}

function fsList(path: string): Promise<{ path: string; entries: FsEntry[] }> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFs.set(request_id, { resolve, reject });
    send({ type: "fs_list", request_id, path: path || "" });
    setTimeout(() => {
      if (pendingFs.has(request_id)) {
        pendingFs.delete(request_id);
        reject(new Error("fs_list timeout"));
      }
    }, 8000);
  });
}

function editorOpen(path: string, preview = false): Promise<OpenedInfo & { rev: number; text: string }> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingEditor.set(request_id, { resolve, reject });
    send({ type: "editor_open", request_id, path, preview: !!preview });
    setTimeout(() => {
      if (pendingEditor.has(request_id)) {
        pendingEditor.delete(request_id);
        reject(new Error("editor_open timeout"));
      }
    }, 15000);
  });
}

function bufferEdit(bufferId: string, baseRev: number, text: string): Promise<number> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingEdit.set(request_id, { resolve, reject });
    send({
      type: "buffer_edit",
      request_id,
      buffer_id: bufferId,
      base_rev: baseRev,
      text,
    });
    setTimeout(() => {
      if (pendingEdit.has(request_id)) {
        pendingEdit.delete(request_id);
        reject(new Error("buffer_edit timeout"));
      }
    }, 15000);
  });
}

function bufferSave(bufferId: string, baseRev: number): Promise<{ path: string; rev: number }> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingSave.set(request_id, { resolve, reject });
    send({
      type: "buffer_save",
      request_id,
      buffer_id: bufferId,
      base_rev: baseRev,
    });
    setTimeout(() => {
      if (pendingSave.has(request_id)) {
        pendingSave.delete(request_id);
        reject(new Error("buffer_save timeout"));
      }
    }, 15000);
  });
}

function rejectPendingError(msg: Extract<ServerMessage, { type: "error" }>): void {
  const err = new Error(`${msg.code}: ${msg.message}`);
  const tryReject = <T>(map: Map<string, Pending<T>>): boolean => {
    for (const [id, pending] of map) {
      if (msg.message && msg.message.startsWith(id)) {
        map.delete(id);
        pending.reject(err);
        return true;
      }
    }
    return false;
  };
  if (
    tryReject(pendingFs) ||
    tryReject(pendingEditor) ||
    tryReject(pendingEdit) ||
    tryReject(pendingSave)
  ) {
    return;
  }
}

function terminalIndices(): number[] {
  const out: number[] = [];
  tabs.forEach((t, i) => {
    if (t.kind === "terminal") out.push(i);
  });
  return out;
}

function activeTerminalIndex(): number | null {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") return activeTabIndex;
  const terms = terminalIndices();
  return terms.length ? terms[0] : null;
}

function measurePill(): void {
  const activeEl = tabsEl.querySelector(".tab.active");
  if (!(activeEl instanceof HTMLElement) || tabs.length === 0) {
    tabPill.hidden = true;
    return;
  }
  tabPill.hidden = false;
  const tabsRect = tabsEl.getBoundingClientRect();
  const rect = activeEl.getBoundingClientRect();
  tabPill.style.left = `${rect.left - tabsRect.left + tabsEl.scrollLeft}px`;
  tabPill.style.width = `${rect.width}px`;
}

function updateSaveButton(): void {
  const active = tabs[activeTabIndex];
  const editorActive = active?.kind === "editor";
  editorSaveBtn.hidden = !editorActive;
  editorSaveBtn.disabled = !editorActive || !active.dirty;
  splitHBtn.hidden = editorActive;
  splitVBtn.hidden = editorActive;
  splitOffBtn.hidden = editorActive;
}

function updateStacks(): void {
  const hasTabs = tabs.length > 0;
  emptyStack.hidden = connected && hasTabs;
  const active = tabs[activeTabIndex];
  terminalStack.hidden = !connected || active?.kind !== "terminal";
  editorStack.hidden = !connected || active?.kind !== "editor";
  tabs.forEach((t, i) => {
    if (t.kind === "editor") {
      t.host.hidden = i !== activeTabIndex;
    }
  });
}

function setWorkspaceControls(enabled: boolean): void {
  newTabBtn.disabled = !enabled;
  splitHBtn.disabled = !enabled;
  splitVBtn.disabled = !enabled;
  splitOffBtn.disabled = !enabled;
}

function openPtyForDims(cols: number, rows: number): void {
  send({ type: "pty_open", cols, rows });
}

function wireTerminalTab(tab: TerminalTab): void {
  const { term, el } = tab.bundle;
  el.dataset.ptyId = tab.id;
  term.onData((data) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_data", id: tab.id, data: b64encode(data) });
  });
  term.onResize(({ cols, rows }) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_resize", id: tab.id, cols, rows });
  });
}

function createTerminalTab(ptyId: string, title?: string, opts: { silentActivate?: boolean } = {}): TerminalTab {
  const bundle = createTerminal();
  const tab: TerminalTab = {
    kind: "terminal",
    id: ptyId,
    title: title || `sh ${terminalIndices().length + 1}`,
    bundle,
  };
  wireTerminalTab(tab);
  tabs.push(tab);
  if (!opts.silentActivate) activeTabIndex = tabs.length - 1;
  renderAll();
  persistLayout();
  return tab;
}

function disposeEditorTab(tab: EditorTab): void {
  send({ type: "editor_close", buffer_id: tab.bufferId });
  try {
    tab.view.destroy();
  } catch {
    /* ignore */
  }
  tab.host.remove();
}

function closeTabAt(index: number, opts: { force?: boolean } = {}): void {
  const tab = tabs[index];
  if (!tab) return;
  if (tab.kind === "editor") {
    if (tab.dirty && !opts.force) {
      if (!confirm(`Discard unsaved changes to ${basename(tab.path)}?`)) return;
    }
    disposeEditorTab(tab);
  } else {
    send({ type: "pty_close", id: tab.id });
    disposeTerminal(tab.bundle);
  }
  tabs.splice(index, 1);
  if (activeTabIndex >= tabs.length) activeTabIndex = Math.max(0, tabs.length - 1);
  if (activeTabIndex === index && tabs.length) {
    activeTabIndex = Math.min(index, tabs.length - 1);
  }
  const terms = terminalIndices();
  if (splitTabIndex >= tabs.length) splitTabIndex = terms.length > 1 ? terms[1] : terms[0] ?? 0;
  if (terms.length < 2) splitMode = null;
  renderAll();
  persistLayout();
}

function tabLabel(tab: Tab): string {
  if (tab.kind === "terminal") return tab.title;
  return basename(tab.path);
}

function renderTabs(): void {
  const pill = tabPill;
  tabsEl.innerHTML = "";
  tabsEl.appendChild(pill);

  tabs.forEach((tab, i) => {
    const el = document.createElement("div");
    el.className = "tab";
    el.dataset.index = String(i);
    if (i === activeTabIndex) el.classList.add("active");
    if (tab.kind === "editor") {
      if (tab.preview) el.classList.add("preview");
      if (tab.dirty) el.classList.add("dirty");
    }
    el.setAttribute("role", "tab");
    el.innerHTML = `<span class="tab-label">${escapeHtml(tabLabel(tab))}</span>`;
    const x = document.createElement("button");
    x.className = "x";
    x.type = "button";
    x.textContent = "×";
    x.title = "Close tab";
    x.addEventListener("click", (ev) => {
      ev.stopPropagation();
      closeTabAt(i);
    });
    el.appendChild(x);
    el.addEventListener("click", () => {
      activeTabIndex = i;
      renderAll();
      persistLayout();
      focusActiveTab();
    });
    tabsEl.appendChild(el);
  });

  requestAnimationFrame(measurePill);
}

function fitTerminalTab(tab: TerminalTab): void {
  try {
    tab.bundle.fit.fit();
  } catch {
    /* ignore */
  }
}

function renderPanes(): void {
  panesEl.innerHTML = "";
  panesEl.className =
    splitMode === "horizontal"
      ? "panes split-horizontal"
      : splitMode === "vertical"
        ? "panes split-vertical"
        : "panes no-split";

  terminalPark.innerHTML = "";

  const terms = terminalIndices();
  if (!terms.length) return;

  let primary = activeTerminalIndex() ?? terms[0];
  let secondary = splitTabIndex;
  if (!terms.includes(primary)) primary = terms[0];
  if (splitMode && terms.length >= 2) {
    if (!terms.includes(secondary) || secondary === primary) {
      secondary = terms.find((idx) => idx !== primary) ?? terms[0];
      splitTabIndex = secondary;
    }
    const indices = [primary, secondary];
    for (const idx of indices) {
      const tab = tabs[idx] as TerminalTab;
      const pane = document.createElement("div");
      pane.className = "pane";
      const label = document.createElement("div");
      label.className = "pane-label";
      label.textContent = tab.title;
      pane.appendChild(label);
      pane.appendChild(tab.bundle.el);
      panesEl.appendChild(pane);
      requestAnimationFrame(() => fitTerminalTab(tab));
    }
    for (const idx of terms) {
      if (!indices.includes(idx)) {
        const tab = tabs[idx] as TerminalTab;
        terminalPark.appendChild(tab.bundle.el);
      }
    }
  } else {
    const tab = tabs[primary] as TerminalTab;
    const pane = document.createElement("div");
    pane.className = "pane";
    pane.appendChild(tab.bundle.el);
    panesEl.appendChild(pane);
    requestAnimationFrame(() => fitTerminalTab(tab));
    for (const idx of terms) {
      if (idx !== primary) {
        const t = tabs[idx] as TerminalTab;
        terminalPark.appendChild(t.bundle.el);
      }
    }
  }
}

function focusActiveTab(): void {
  const active = tabs[activeTabIndex];
  if (!active) return;
  if (active.kind === "terminal") {
    requestAnimationFrame(() => {
      fitTerminalTab(active);
      active.bundle.term.focus();
    });
  } else {
    active.view.focus();
  }
}

function renderAll(): void {
  renderTabs();
  renderPanes();
  updateStacks();
  updateSaveButton();
  updateStatusRight();
}

function clearTabs(): void {
  for (const tab of tabs) {
    if (tab.kind === "terminal") disposeTerminal(tab.bundle);
    else disposeEditorTab(tab);
  }
  tabs = [];
  activeTabIndex = 0;
  splitTabIndex = 1;
  splitMode = null;
  pendingSplit = null;
  panesEl.innerHTML = "";
  panesEl.className = "panes no-split";
  terminalPark.innerHTML = "";
  editorStack.innerHTML = "";
  renderAll();
}

function clearTree(): void {
  treeEl.innerHTML = "";
  pendingFs.clear();
  selectedPath = null;
  expandedPaths.clear();
  treeLoaded = false;
  treeLoading = false;
  treeNeedsRefresh = false;
  if (treeRefreshTimer) {
    clearTimeout(treeRefreshTimer);
    treeRefreshTimer = null;
  }
}

function pathIsNoisy(path: string): boolean {
  const parts = String(path).split(/[/\\]/).filter(Boolean);
  return parts.some((p) => WATCH_IGNORE_DIRS.has(p));
}

function noteTreeInteraction(): void {
  treeQuietUntil = Date.now() + 2000;
}

function scheduleTreeRefresh(): void {
  if (Date.now() < treeQuietUntil) {
    if (treeRefreshTimer) clearTimeout(treeRefreshTimer);
    treeRefreshTimer = setTimeout(() => {
      treeRefreshTimer = null;
      scheduleTreeRefresh();
    }, Math.max(200, treeQuietUntil - Date.now() + 50));
    return;
  }
  if (treeRefreshTimer) clearTimeout(treeRefreshTimer);
  treeRefreshTimer = setTimeout(() => {
    treeRefreshTimer = null;
    if (Date.now() < treeQuietUntil) {
      scheduleTreeRefresh();
      return;
    }
    loadRoot({ silent: true }).catch(() => {});
  }, 800);
}

function startFsWatch(): void {
  const request_id = nextRequestId();
  send({ type: "fs_watch", request_id, path: "", recursive: true });
}

async function loadRoot(opts: { silent?: boolean } = {}): Promise<void> {
  const silent = !!opts.silent && treeLoaded;
  if (treeLoading) {
    treeNeedsRefresh = true;
    return;
  }
  treeLoading = true;
  if (!silent) {
    clearTree();
    treeEl.textContent = "Loading…";
  }
  try {
    const listed = await fsList("");
    const prevSelected = selectedPath;
    const keepExpanded = new Set(expandedPaths);
    treeEl.innerHTML = "";
    const rootLabel = document.createElement("div");
    rootLabel.className = "tree-item";
    rootLabel.innerHTML = `<span class="twist"></span><span class="kind">⌂</span><span>${escapeHtml(listed.path)}</span>`;
    treeEl.appendChild(rootLabel);
    const children = document.createElement("div");
    children.className = "tree-children";
    treeEl.appendChild(children);
    renderEntries(children, listed.entries);
    treeLoaded = true;
    if (prevSelected) selectedPath = prevSelected;
    if (keepExpanded.size) await restoreExpanded(children, keepExpanded);
    if (!silent) setStatusLeft(`session ${sessionId || "?"} · ${listed.path}`);
  } catch (err) {
    if (!silent || !treeLoaded) {
      treeEl.innerHTML = `<div class="tree-empty">${escapeHtml(String(err))}</div>`;
    }
  } finally {
    treeLoading = false;
    if (treeNeedsRefresh) {
      treeNeedsRefresh = false;
      scheduleTreeRefresh();
    }
  }
}

async function restoreExpanded(container: HTMLElement, paths: Set<string>): Promise<void> {
  const rows = [...container.querySelectorAll(":scope > .tree-item")].filter(
    (el): el is HTMLElement => el instanceof HTMLElement,
  );
  for (const row of rows) {
    const childBox = row.nextElementSibling;
    if (!(childBox instanceof HTMLElement) || !childBox.classList.contains("tree-children")) continue;
    const path = row.dataset.path;
    if (!path || !paths.has(path)) continue;
    const twist = row.querySelector(".twist");
    try {
      if (!childBox.dataset.loaded) {
        if (twist) twist.textContent = "…";
        const listed = await fsList(path);
        childBox.innerHTML = "";
        renderEntries(childBox, listed.entries);
        childBox.dataset.loaded = "1";
      }
      childBox.hidden = false;
      if (twist) twist.textContent = "▾";
      expandedPaths.add(path);
      await restoreExpanded(childBox, paths);
    } catch {
      if (twist) twist.textContent = "▸";
      expandedPaths.delete(path);
    }
  }
}

function kindIcon(kind: string): string {
  if (kind === "dir") return "▸";
  if (kind === "symlink") return "↗";
  return "·";
}

function renderEntries(container: HTMLElement, entries: FsEntry[]): void {
  for (const entry of entries) {
    const row = document.createElement("div");
    row.className = "tree-item";
    row.dataset.path = entry.path;
    if (selectedPath === entry.path) row.classList.add("selected");
    const twist = document.createElement("span");
    twist.className = "twist";
    twist.textContent = entry.kind === "dir" ? "▸" : "";
    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = kindIcon(entry.kind);
    const name = document.createElement("span");
    name.textContent = entry.name;
    row.append(twist, kind, name);
    const childBox = document.createElement("div");
    childBox.className = "tree-children";
    childBox.hidden = true;

    let fileClickTimer: ReturnType<typeof setTimeout> | null = null;

    row.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      noteTreeInteraction();
      document.querySelectorAll(".tree-item.selected").forEach((el) => el.classList.remove("selected"));
      row.classList.add("selected");
      selectedPath = entry.path;
      setStatusLeft(`${entry.kind}: ${entry.path}`);

      if (entry.kind === "dir") {
        if (!childBox.dataset.loaded) {
          twist.textContent = "…";
          try {
            const listed = await fsList(entry.path);
            childBox.innerHTML = "";
            renderEntries(childBox, listed.entries);
            childBox.dataset.loaded = "1";
            childBox.hidden = false;
            twist.textContent = "▾";
            expandedPaths.add(entry.path);
          } catch (err) {
            twist.textContent = "▸";
            expandedPaths.delete(entry.path);
            setStatusLeft(`fs error: ${err instanceof Error ? err.message : String(err)}`);
          }
        } else if (childBox.hidden) {
          childBox.hidden = false;
          twist.textContent = "▾";
          expandedPaths.add(entry.path);
        } else {
          childBox.hidden = true;
          twist.textContent = "▸";
          expandedPaths.delete(entry.path);
        }
        return;
      }

      if (entry.kind === "file") {
        if (fileClickTimer) clearTimeout(fileClickTimer);
        fileClickTimer = setTimeout(() => {
          fileClickTimer = null;
          void openEditorTab(entry.path, true);
        }, 220);
      }
    });

    if (entry.kind === "file") {
      row.addEventListener("dblclick", (ev) => {
        ev.stopPropagation();
        noteTreeInteraction();
        if (fileClickTimer) {
          clearTimeout(fileClickTimer);
          fileClickTimer = null;
        }
        void openEditorTab(entry.path, false);
      });
    }

    container.append(row, childBox);
  }
}

async function openEditorTab(path: string, preview: boolean): Promise<void> {
  if (!hasEditor) {
    setStatusLeft("backend has no editor capability");
    return;
  }

  const existingIdx = tabs.findIndex((t) => t.kind === "editor" && t.path === path);
  if (existingIdx >= 0) {
    const et = tabs[existingIdx] as EditorTab;
    if (!preview) et.preview = false;
    activeTabIndex = existingIdx;
    renderAll();
    focusActiveTab();
    return;
  }

  if (preview) {
    const previewIdx = tabs.findIndex(
      (t) => t.kind === "editor" && t.preview && !t.dirty,
    );
    if (previewIdx >= 0) closeTabAt(previewIdx, { force: true });
  }

  setStatusLeft(`opening ${path}…`);
  try {
    const opened = await editorOpen(path, preview);
    const host = document.createElement("div");
    host.className = "editor-host";
    editorStack.appendChild(host);

    let tabRef!: EditorTab;
    const view = createEditorView(host, opened.text, opened.path, () => {
      if (tabRef.suppressChange) return;
      tabRef.dirty = true;
      tabRef.preview = false;
      renderTabs();
      updateSaveButton();
      updateStatusRight();
    });

    tabRef = {
      kind: "editor",
      id: `editor-${opened.buffer_id}`,
      bufferId: opened.buffer_id,
      path: opened.path,
      rev: opened.rev,
      dirty: false,
      preview,
      view,
      host,
      suppressChange: false,
    };

    tabs.push(tabRef);
    activeTabIndex = tabs.length - 1;
    renderAll();
    focusActiveTab();
    setStatusLeft(`opened ${opened.path}`);
  } catch (err) {
    setStatusLeft(`editor error: ${err instanceof Error ? err.message : String(err)}`);
  }
}

async function saveActiveEditor(): Promise<void> {
  const active = tabs[activeTabIndex];
  if (active?.kind !== "editor" || !active.dirty) return;
  const text = active.view.state.doc.toString();
  setStatusLeft(`saving ${active.path}…`);
  try {
    const revAfterEdit = await bufferEdit(active.bufferId, active.rev, text);
    active.rev = revAfterEdit;
    const saved = await bufferSave(active.bufferId, active.rev);
    active.rev = saved.rev;
    active.dirty = false;
    active.preview = false;
    renderAll();
    setStatusLeft(`saved ${saved.path}`);
  } catch (err) {
    setStatusLeft(`save error: ${err instanceof Error ? err.message : String(err)}`);
  }
}

function afterSessionReady(): void {
  connected = true;
  setWorkspaceControls(true);
  updateStripLayout();
  updateStatusRight();
  loadRoot()
    .then(() => startFsWatch())
    .catch(() => startFsWatch());
}

function applyLayoutFromBlob(layout: LayoutBlob): void {
  splitMode = layout.split || null;
  activeTabIndex = layout.activeTab ?? 0;
  splitTabIndex = layout.splitTab ?? 1;
  if (layout.sidebarWidth) applySidebarWidth(layout.sidebarWidth);
  if (layout.sidebarCollapsed !== undefined) setSidebarCollapsed(layout.sidebarCollapsed);
}

function onMessage(raw: string): void {
  let msg: ServerMessage;
  try {
    msg = JSON.parse(raw) as ServerMessage;
  } catch {
    setStatusLeft("bad json from backend");
    return;
  }

  switch (msg.type) {
    case "hello":
      hasEditor = Array.isArray(msg.capabilities) && msg.capabilities.includes("editor");
      send({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        role: "client",
        implementation: "fresh-gui-ui/0.4",
        capabilities: ["ping", "pty", "fs", "session", "editor", "scene"],
      });
      {
        const token = $input("token").value;
        if (token) send({ type: "auth", token });
        else beginSession();
      }
      setStatusLeft(`hello from ${msg.implementation}${hasEditor ? " · editor" : " · no editor"}`);
      updateStatusRight();
      break;
    case "auth_ok":
      beginSession();
      setStatusLeft("authenticated");
      break;
    case "auth_error":
      setStatusLeft(`auth failed: ${msg.message}`);
      break;
    case "session_created":
      sessionId = msg.session_id;
      $input("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      afterSessionReady();
      openPtyForDims(80, 24);
      setStatusLeft(`session ${sessionId}`);
      updateStripChips();
      break;
    case "session_attached":
      sessionId = msg.session_id;
      $input("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      clearTabs();
      if (typeof msg.layout === "string" && msg.layout) {
        try {
          applyLayoutFromBlob(JSON.parse(msg.layout) as LayoutBlob);
        } catch {
          /* ignore bad layout */
        }
      } else {
        applyLayoutFromBlob(readLayoutBlob());
      }
      for (const p of msg.ptys || []) {
        createTerminalTab(p.id, `sh ${terminalIndices().length + 1}`, { silentActivate: true });
      }
      if (terminalIndices().length === 0) openPtyForDims(80, 24);
      else {
        activeTabIndex = Math.min(activeTabIndex, tabs.length - 1);
        renderAll();
      }
      afterSessionReady();
      setStatusLeft(`reattached ${sessionId} (${(msg.ptys || []).length} ptys)`);
      updateStripChips();
      break;
    case "pty_opened": {
      const tab = createTerminalTab(msg.id, `sh ${terminalIndices().length}`);
      if (pendingSplit && terminalIndices().length >= 2) {
        splitMode = pendingSplit;
        const terms = terminalIndices();
        splitTabIndex = terms[terms.length - 1];
        pendingSplit = null;
        renderPanes();
        persistLayout();
        setStatusLeft(`split ${splitMode}`);
      }
      requestAnimationFrame(() => {
        fitTerminalTab(tab);
        if (tabs[activeTabIndex]?.kind === "terminal") tab.bundle.term.focus();
      });
      break;
    }
    case "pty_data": {
      const tab = tabs.find((t) => t.kind === "terminal" && t.id === msg.id) as TerminalTab | undefined;
      if (tab) tab.bundle.term.write(b64decode(msg.data));
      break;
    }
    case "pty_closed": {
      const idx = tabs.findIndex((t) => t.kind === "terminal" && t.id === msg.id);
      if (idx >= 0) closeTabAt(idx, { force: true });
      break;
    }
    case "fs_listed": {
      const pending = pendingFs.get(msg.request_id);
      if (pending) {
        pendingFs.delete(msg.request_id);
        pending.resolve(msg);
      }
      break;
    }
    case "editor_opened": {
      const pending = pendingEditor.get(msg.request_id);
      if (pending) {
        pending._opened = {
          buffer_id: msg.buffer_id,
          path: msg.path,
          language: msg.language,
        };
      }
      break;
    }
    case "buffer_snapshot": {
      for (const [id, pending] of pendingEditor) {
        if (pending._opened && pending._opened.buffer_id === msg.buffer_id) {
          pendingEditor.delete(id);
          pending.resolve({
            ...pending._opened,
            rev: msg.rev,
            text: msg.text,
            path: msg.path || pending._opened.path,
          });
          break;
        }
      }
      break;
    }
    case "buffer_changed": {
      const pending = pendingEdit.get(msg.request_id);
      if (pending) {
        pendingEdit.delete(msg.request_id);
        pending.resolve(msg.rev);
      }
      break;
    }
    case "buffer_saved": {
      const pending = pendingSave.get(msg.request_id);
      if (pending) {
        pendingSave.delete(msg.request_id);
        pending.resolve({ path: msg.path, rev: msg.rev });
      }
      break;
    }
    case "fs_watch_started":
      watchId = msg.watch_id;
      updateStatusRight();
      break;
    case "fs_changed": {
      const paths = Array.isArray(msg.paths) ? msg.paths : [];
      if (paths.length && paths.every(pathIsNoisy)) break;
      scheduleTreeRefresh();
      break;
    }
    case "error":
      rejectPendingError(msg);
      if (msg.code === "session_attach_failed") {
        localStorage.removeItem(SESSION_KEY);
        $input("session").value = "";
        setStatusLeft(`${msg.code}: ${msg.message}; creating new session…`);
        send({ type: "session_create", layout: localStorage.getItem(LAYOUT_KEY) || undefined });
        break;
      }
      setStatusLeft(`${msg.code}: ${msg.message}`);
      break;
    default:
      break;
  }
}

function beginSession(): void {
  const wanted = $input("session").value.trim() || localStorage.getItem(SESSION_KEY) || "";
  if (wanted) {
    $input("session").value = wanted;
    send({ type: "session_attach", session_id: wanted });
  } else {
    send({ type: "session_create", layout: localStorage.getItem(LAYOUT_KEY) || undefined });
  }
}

function disconnect(): void {
  if (watchId) send({ type: "fs_unwatch", watch_id: watchId });
  watchId = null;
  if (ws) {
    try {
      ws.close();
    } catch {
      /* ignore */
    }
  }
  ws = null;
  connected = false;
  sessionId = null;
  hasEditor = false;
  stripForceExpanded = false;
  pendingEditor.clear();
  pendingEdit.clear();
  pendingSave.clear();
  clearTabs();
  clearTree();
  treeEl.innerHTML = '<div class="tree-empty">Connect to load remote tree</div>';
  connectBtn.disabled = false;
  disconnectBtn.disabled = true;
  setWorkspaceControls(false);
  updateStripLayout();
  updateStatusRight();
  setStatusLeft("disconnected (session kept on backend if created)");
}

function connect(): void {
  disconnect();
  const url = $input("url").value.trim();
  setStatusLeft(`connecting ${url}…`);
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    connectBtn.disabled = true;
    disconnectBtn.disabled = false;
    setStatusLeft("socket open, waiting for hello…");
    updateStripChips();
  });
  ws.addEventListener("message", (ev) => {
    if (typeof ev.data === "string") onMessage(ev.data);
  });
  ws.addEventListener("close", () => {
    connected = false;
    sessionId = null;
    connectBtn.disabled = false;
    disconnectBtn.disabled = true;
    setWorkspaceControls(false);
    updateStripLayout();
    updateStatusRight();
    setStatusLeft("disconnected (session kept on backend if created)");
  });
  ws.addEventListener("error", () => setStatusLeft("websocket error"));
}

function requestSplit(mode: SplitMode): void {
  const terms = terminalIndices();
  if (terms.length < 1) {
    setStatusLeft("open a shell first");
    return;
  }
  if (terms.length < 2) {
    pendingSplit = mode;
    setStatusLeft(`opening second shell for ${mode} split…`);
    const primary = tabs[terms[0]] as TerminalTab;
    const dims = primary.bundle.fit.proposeDimensions?.() || { cols: 80, rows: 24 };
    openPtyForDims(dims.cols || 80, dims.rows || 24);
    return;
  }
  splitMode = mode;
  const primary = activeTerminalIndex() ?? terms[0];
  if (splitTabIndex === primary || !terms.includes(splitTabIndex)) {
    splitTabIndex = terms.find((i) => i !== primary) ?? terms[0];
  }
  renderPanes();
  persistLayout();
  setStatusLeft(`split ${mode}`);
}

function setupSidebarResizer(): void {
  let dragging = false;
  let startX = 0;
  let startWidth = 0;

  sidebarResizer.addEventListener("mousedown", (ev) => {
    if (isSidebarCollapsed()) return;
    dragging = true;
    startX = ev.clientX;
    startWidth = Number.parseInt(
      getComputedStyle(document.documentElement).getPropertyValue("--sidebar-width") || "260",
      10,
    );
    sidebarResizer.classList.add("dragging");
    ev.preventDefault();
  });

  window.addEventListener("mousemove", (ev) => {
    if (!dragging) return;
    applySidebarWidth(startWidth + (ev.clientX - startX));
  });

  window.addEventListener("mouseup", () => {
    if (!dragging) return;
    dragging = false;
    sidebarResizer.classList.remove("dragging");
    persistLayout();
    requestAnimationFrame(measurePill);
  });
}

connectBtn.addEventListener("click", connect);
disconnectBtn.addEventListener("click", disconnect);
stripToggle.addEventListener("click", () => {
  if (stripForceExpanded) compactStrip();
  else expandStrip();
});
stripCompact.addEventListener("click", expandStrip);
editorSaveBtn.addEventListener("click", () => {
  void saveActiveEditor();
});
newTabBtn.addEventListener("click", () => {
  const idx = activeTerminalIndex();
  const dims =
    (idx !== null ? (tabs[idx] as TerminalTab).bundle.fit.proposeDimensions?.() : null) ||
    { cols: 80, rows: 24 };
  openPtyForDims(dims.cols || 80, dims.rows || 24);
});
splitHBtn.addEventListener("click", () => requestSplit("horizontal"));
splitVBtn.addEventListener("click", () => requestSplit("vertical"));
splitOffBtn.addEventListener("click", () => {
  splitMode = null;
  pendingSplit = null;
  renderPanes();
  persistLayout();
  setStatusLeft("no split");
});
sidebarToggle.addEventListener("click", toggleSidebar);

tabsEl.addEventListener("scroll", () => requestAnimationFrame(measurePill));

window.addEventListener("keydown", (ev) => {
  if (isMod(ev) && ev.key === "s") {
    const active = tabs[activeTabIndex];
    if (active?.kind === "editor" && active.dirty) {
      ev.preventDefault();
      void saveActiveEditor();
    }
    return;
  }
  if (isMod(ev) && ev.key === "t") {
    ev.preventDefault();
    if (!connected) return;
    const idx = activeTerminalIndex();
    const dims =
      (idx !== null ? (tabs[idx] as TerminalTab).bundle.fit.proposeDimensions?.() : null) ||
      { cols: 80, rows: 24 };
    openPtyForDims(dims.cols || 80, dims.rows || 24);
    return;
  }
  if (isMod(ev) && ev.key === "w") {
    if (tabs.length === 0) return;
    ev.preventDefault();
    closeTabAt(activeTabIndex);
    return;
  }
  if (isMod(ev) && ev.key === "d" && ev.shiftKey) {
    ev.preventDefault();
    if (connected) requestSplit("vertical");
    return;
  }
  if (isMod(ev) && ev.key === "d" && !ev.shiftKey) {
    ev.preventDefault();
    if (connected) requestSplit("horizontal");
    return;
  }
  if (isMod(ev) && ev.key === "b") {
    ev.preventDefault();
    toggleSidebar();
  }
});

window.addEventListener("resize", () => {
  for (const tab of tabs) {
    if (tab.kind === "terminal") fitTerminalTab(tab);
  }
  requestAnimationFrame(measurePill);
});

loadSidebarPrefs();
updateStripLayout();
updateStatusRight();
setStatusLeft("disconnected");

function defaultWsUrl(): string {
  // Vite dev UI is on :1420; backend stays on :7420.
  if (import.meta.env.DEV) {
    return "ws://127.0.0.1:7420/ws";
  }
  const { protocol, host } = window.location;
  if (protocol === "http:" || protocol === "https:") {
    const wsProto = protocol === "https:" ? "wss:" : "ws:";
    return `${wsProto}//${host}/ws`;
  }
  return "ws://127.0.0.1:7420/ws";
}

$input("url").value = defaultWsUrl();

const savedSession = localStorage.getItem(SESSION_KEY);
if (savedSession) $input("session").value = savedSession;

setupSidebarResizer();
renderAll();
