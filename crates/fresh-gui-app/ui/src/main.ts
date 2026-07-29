/** Phase UI-3 host shell: OSC 7 cwd, find bar, activity bar, icons, light theme + settings. */
import type { EditorView } from "@codemirror/view";
import "./styles.css";
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type FsEntry,
  type PtyInfo,
  type ServerMessage,
} from "./protocol";
import { $, $button, $input, b64decode, b64encode, basename, relativePath } from "./dom";
import { applyEditorFontSize, applyEditorTheme, createEditorView, openEditorSearch } from "./editor";
import {
  applyTerminalFontSize,
  applyTerminalTheme,
  createTerminal,
  disposeTerminal,
  type TermBundle,
} from "./terminal";
import { feedOsc7Chunk } from "./osc7";
import {
  MAX_LEAVES_PER_TAB,
  type PaneNode,
  type SplitDir,
  collectLeafIds,
  directionFromSplitMode,
  leafCount,
  nextLeafId,
  removeLeaf,
  splitLeaf,
} from "./panes";
import { installShortcuts, type ShortcutHandlers, type ShortcutId } from "./shortcuts";
import { defaultPaletteCommands, openPalette, setPaletteCommands } from "./palette";
import { VirtualTree } from "./tree";
import { applyTheme, getResolvedTheme, initTheme, onResolvedThemeChange, resolveTheme } from "./theme";
import {
  loadSettings,
  normalizeUiSettings,
  saveSettings,
  uiSettingsFromConfigText,
  type UiSettings,
} from "./settings";
import { closeFindBar, openFindBar, setSearchTarget } from "./search";
import {
  copyToClipboard,
  openContextMenu,
  type ContextMenuItem,
} from "./context-menu";

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
  /** Tab id, distinct from any pty id (a tab may host several ptys as leaves). */
  id: string;
  title: string;
  paneTree: PaneNode;
  leaves: Map<string, TermBundle>;
  activeLeafId: string;
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
  version?: number;
  activeTab?: number;
  sidebarWidth?: number;
  sidebarCollapsed?: boolean;
  tabs?: Array<
    | { kind: "terminal"; id: string; title: string; paneTree: PaneNode; activeLeafId: string }
    | { kind: "editor"; id: string; path: string; preview?: boolean }
  >;
}

/** Deferred intent consumed by the next `pty_opened` reply (requests are FIFO on one socket). */
type PtyIntent = { kind: "newTab" } | { kind: "split"; tabId: string; leafId: string; direction: SplitDir };

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
const findBtn = $button("find-btn");
const activityExplorer = $button("activity-explorer");
const activitySettings = $button("activity-settings");

initTheme();
let uiSettings: UiSettings = loadSettings();
applyTheme(uiSettings.theme);
/** Absolute path to backend `config.json` (from Hello). */
let configPath: string | null = null;

const terminalPark = document.createElement("div");
terminalPark.className = "terminal-park";
terminalPark.style.cssText =
  "position:absolute;visibility:hidden;pointer-events:none;width:0;height:0;overflow:hidden";
terminalStack.appendChild(terminalPark);

let ws: WebSocket | null = null;
let sessionId: string | null = null;
let reqSeq = 0;
let tabSeq = 0;
let hasEditor = false;
let watchId: string | null = null;
let stripForceExpanded = false;
let connected = false;

const pendingFs = new Map<string, Pending<{ path: string; entries: FsEntry[] }>>();
const pendingFsAuth = new Map<string, Pending<{ path: string }>>();
const pendingEditor = new Map<string, PendingEditor>();
const pendingEdit = new Map<string, Pending<number>>();
const pendingSave = new Map<string, Pending<{ path: string; rev: number }>>();
const pendingPtyIntents: PtyIntent[] = [];

let treeRefreshTimer: ReturnType<typeof setTimeout> | null = null;
let treeLoading = false;
let treeLoaded = false;
let treeNeedsRefresh = false;
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
  activityExplorer.classList.toggle("active", !collapsed);
  activityExplorer.setAttribute("aria-pressed", collapsed ? "false" : "true");
}

function toggleSidebar(): void {
  setSidebarCollapsed(!isSidebarCollapsed());
  persistLayout();
  requestAnimationFrame(measurePill);
  requestAnimationFrame(fitActiveLeaves);
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

function applySidebarFromBlob(layout: LayoutBlob): void {
  if (layout.sidebarWidth) applySidebarWidth(layout.sidebarWidth);
  if (layout.sidebarCollapsed !== undefined) setSidebarCollapsed(layout.sidebarCollapsed);
}

function persistLayout(): void {
  const layout: LayoutBlob = {
    version: 2,
    activeTab: activeTabIndex,
    sidebarWidth: Number.parseInt(
      getComputedStyle(document.documentElement).getPropertyValue("--sidebar-width") || "260",
      10,
    ),
    sidebarCollapsed: isSidebarCollapsed(),
    tabs: tabs.map((t) =>
      t.kind === "terminal"
        ? { kind: "terminal", id: t.id, title: t.title, paneTree: t.paneTree, activeLeafId: t.activeLeafId }
        : { kind: "editor", id: t.id, path: t.path, preview: t.preview },
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

function nextTabId(): string {
  tabSeq += 1;
  return `term-${tabSeq}`;
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

/** Terax-style: allow explorer/editor FS under a terminal cwd (may be outside `--root`). */
function fsAuthorize(path: string): Promise<{ path: string }> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFsAuth.set(request_id, { resolve, reject });
    send({ type: "fs_authorize", request_id, path });
    setTimeout(() => {
      if (pendingFsAuth.has(request_id)) {
        pendingFsAuth.delete(request_id);
        reject(new Error("fs_authorize timeout"));
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
    tryReject(pendingFsAuth) ||
    tryReject(pendingEditor) ||
    tryReject(pendingEdit) ||
    tryReject(pendingSave)
  ) {
    return;
  }
}

function terminalTabs(): TerminalTab[] {
  return tabs.filter((t): t is TerminalTab => t.kind === "terminal");
}

function activeTerminalTab(): TerminalTab | null {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") return active;
  return terminalTabs()[0] ?? null;
}

function findLeafOwner(ptyId: string): { tab: TerminalTab; bundle: TermBundle } | null {
  for (const t of tabs) {
    if (t.kind === "terminal" && t.leaves.has(ptyId)) {
      return { tab: t, bundle: t.leaves.get(ptyId)! };
    }
  }
  return null;
}

function proposedDims(tab: TerminalTab | null): { cols: number; rows: number } {
  const bundle = tab ? tab.leaves.get(tab.activeLeafId) : undefined;
  const dims = bundle?.fit.proposeDimensions?.();
  return { cols: dims?.cols || 80, rows: dims?.rows || 24 };
}

function requestNewPty(cols: number, rows: number, intent: PtyIntent, cwd?: string): void {
  pendingPtyIntents.push(intent);
  send({ type: "pty_open", cols, rows, ...(cwd ? { cwd } : {}) });
}

function titleFromCwd(cwd: string | undefined, fallback: string): string {
  if (!cwd) return fallback;
  const base = basename(cwd.replace(/[/\\]+$/, "") || cwd);
  return base || fallback;
}

/** Last known terminal leaf cwd (Terax: explorer + new shells follow this, not editor folder). */
let lastTerminalCwd: string | null = null;
let explorerSyncGen = 0;

/** Prefer active terminal leaf cwd (OSC 7), else last terminal cwd — not the editor file’s parent. */
function resolveSpawnCwd(): string | undefined {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") {
    const leaf = active.leaves.get(active.activeLeafId);
    if (leaf?.cwd) return leaf.cwd;
  }
  if (lastTerminalCwd) return lastTerminalCwd;
  for (const t of tabs) {
    if (t.kind !== "terminal") continue;
    const leaf = t.leaves.get(t.activeLeafId);
    if (leaf?.cwd) return leaf.cwd;
  }
  return undefined;
}

function updateExplorerTitle(rootPath: string): void {
  const title = document.getElementById("sidebar-title");
  if (!title) return;
  if (!rootPath) {
    title.textContent = "Explorer";
    title.removeAttribute("title");
    return;
  }
  title.textContent = basename(rootPath) || "Explorer";
  title.title = rootPath;
}

/** Re-root explorer immediately (authorize outside `--root` first, like Terax). */
function syncExplorerToCwd(cwd: string): void {
  lastTerminalCwd = cwd;
  try {
    updateExplorerTitle(cwd);
    setTreeEmptyHint(false);
  } catch {
    /* ignore chrome update errors */
  }
  explorerSyncGen += 1;
  const gen = explorerSyncGen;
  void (async () => {
    try {
      await fsAuthorize(cwd);
      if (gen !== explorerSyncGen) return;
      const ok = await tree.setViewRoot(cwd);
      if (gen !== explorerSyncGen) return;
      if (!ok) {
        setStatusLeft(`explorer stuck at ${tree.getRootPath() || "?"} · want ${cwd}`);
        return;
      }
      updateExplorerTitle(tree.getRootPath() || cwd);
    } catch (err) {
      if (gen !== explorerSyncGen) return;
      setStatusLeft(
        `explorer: cannot open ${cwd} (${err instanceof Error ? err.message : String(err)})`,
      );
    }
  })();
}

/** Sync explorer to the active terminal leaf cwd (Terax `explorerRoot`). */
function syncExplorerToActiveContext(): void {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") {
    const cwd = active.leaves.get(active.activeLeafId)?.cwd;
    if (cwd) syncExplorerToCwd(cwd);
  } else if (lastTerminalCwd) {
    syncExplorerToCwd(lastTerminalCwd);
  }
}

function applyLeafCwd(tab: TerminalTab, ptyId: string, cwd: string): void {
  const bundle = tab.leaves.get(ptyId);
  if (!bundle) return;
  if (bundle.cwd === cwd) return;
  bundle.cwd = cwd;

  const follow = tab.activeLeafId === ptyId || tab.leaves.size === 1;
  if (follow) {
    tab.title = titleFromCwd(cwd, tab.title);
    renderTabs();
    lastTerminalCwd = cwd;
    if (tabs[activeTabIndex] === tab) setStatusLeft(cwd);
    syncExplorerToCwd(cwd);
  }

  panesEl.querySelectorAll<HTMLElement>(".pane-leaf").forEach((el) => {
    if (el.dataset.leafId !== ptyId) return;
    const label = el.querySelector(".pane-label");
    if (label) label.textContent = titleFromCwd(cwd, tab.title);
  });
}

function syncSearchTarget(): void {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") {
    const bundle = active.leaves.get(active.activeLeafId);
    if (!bundle) {
      setSearchTarget(null);
      return;
    }
    const opts = {
      caseSensitive: false,
      incremental: false,
      regex: false,
      wholeWord: false,
      decorations: {
        matchBackground: "#3fb95055",
        activeMatchBackground: "#3fb95088",
        matchOverviewRuler: "#3fb950",
        activeMatchColorOverviewRuler: "#3fb950",
      },
    };
    setSearchTarget({
      kind: "terminal",
      findNext: (q, backwards) => {
        if (!q) return false;
        return backwards
          ? !!bundle.search.findPrevious(q, opts)
          : !!bundle.search.findNext(q, opts);
      },
      clear: () => bundle.search.clearDecorations(),
    });
  } else if (active?.kind === "editor") {
    setSearchTarget({
      kind: "editor",
      open: () => openEditorSearch(active.view),
    });
  } else {
    setSearchTarget(null);
  }
}

function focusFind(): void {
  syncSearchTarget();
  openFindBar();
}

function applyUiSettings(next: UiSettings): void {
  const prevResolved = resolveTheme(uiSettings.theme);
  uiSettings = next;
  saveSettings(next);
  const resolved = resolveTheme(next.theme);
  restyleOpenPanes(resolved, resolved !== prevResolved);
  requestAnimationFrame(fitActiveLeaves);
}

function pathsEqual(a: string, b: string): boolean {
  return a === b || a.replace(/\/+$/, "") === b.replace(/\/+$/, "");
}

function isConfigPath(path: string): boolean {
  return !!configPath && pathsEqual(path, configPath);
}

/** Open backend `config.json` as an editor tab (theme, fonts, shell, …). */
async function openSettingsFile(): Promise<void> {
  if (!connected) {
    setStatusLeft("connect to a backend to edit settings");
    return;
  }
  if (!hasEditor) {
    setStatusLeft("editor unavailable — cannot open settings");
    return;
  }
  if (!configPath) {
    setStatusLeft("backend did not advertise a config path");
    return;
  }
  await openEditorTab(configPath, false);
  setStatusLeft(`settings: ${configPath} (save with Mod+S)`);
}

/** Restyle open terminals/editors to match the resolved chrome theme. */
function restyleOpenPanes(resolved: ReturnType<typeof resolveTheme>, themeChanged: boolean): void {
  for (const tab of tabs) {
    if (tab.kind === "terminal") {
      for (const bundle of tab.leaves.values()) {
        applyTerminalFontSize(bundle, uiSettings.terminalFontSize);
        if (themeChanged) applyTerminalTheme(bundle);
      }
    } else {
      applyEditorFontSize(tab.view, uiSettings.editorFontSize);
      if (themeChanged) applyEditorTheme(tab.view, resolved);
    }
  }
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
  emptyStack.textContent = connected
    ? "Open a terminal or file to get started"
    : "Connect to a backend to open a session";
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

function wireTerminalLeaf(ptyId: string, bundle: TermBundle): void {
  bundle.el.dataset.ptyId = ptyId;
  bundle.term.onData((data) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_data", id: ptyId, data: b64encode(data) });
  });
  bundle.term.onResize(({ cols, rows }) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_resize", id: ptyId, cols, rows });
  });
}

function makeTerminal(): TermBundle {
  const bundle = createTerminal({
    settings: uiSettings,
    onCwd: (cwd) => {
      const id = bundle.el.dataset.ptyId;
      if (!id) return;
      const owner = findLeafOwner(id);
      if (owner) applyLeafCwd(owner.tab, id, cwd);
    },
  });
  return bundle;
}

function fitBundle(bundle: TermBundle): void {
  try {
    bundle.fit.fit();
  } catch {
    /* ignore */
  }
}

function fitActiveLeaves(): void {
  const active = tabs[activeTabIndex];
  if (active?.kind !== "terminal") return;
  for (const bundle of active.leaves.values()) fitBundle(bundle);
}

function createTerminalTab(
  ptyId: string,
  bundle: TermBundle,
  opts: { silentActivate?: boolean; title?: string } = {},
): TerminalTab {
  const title = opts.title || `sh ${terminalTabs().length + 1}`;
  const tab: TerminalTab = {
    kind: "terminal",
    id: nextTabId(),
    title,
    paneTree: { type: "leaf", id: ptyId },
    leaves: new Map([[ptyId, bundle]]),
    activeLeafId: ptyId,
  };
  wireTerminalLeaf(ptyId, bundle);
  tabs.push(tab);
  if (!opts.silentActivate) activeTabIndex = tabs.length - 1;
  renderAll();
  persistLayout();
  requestAnimationFrame(() => {
    fitBundle(bundle);
    if (tabs[activeTabIndex] === tab) bundle.term.focus();
  });
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

function closeWholeTerminalTab(tab: TerminalTab): void {
  for (const [ptyId, bundle] of tab.leaves) {
    send({ type: "pty_close", id: ptyId });
    disposeTerminal(bundle);
  }
  const idx = tabs.indexOf(tab);
  if (idx < 0) return;
  tabs.splice(idx, 1);
  if (activeTabIndex >= tabs.length) activeTabIndex = Math.max(0, tabs.length - 1);
  else if (activeTabIndex > idx) activeTabIndex -= 1;
  renderAll();
  persistLayout();
}

/** Close a single leaf; collapses/removes the tab if it was the last leaf. */
function closeLeaf(tab: TerminalTab, ptyId: string, opts: { alreadyClosedRemote?: boolean } = {}): void {
  if (!opts.alreadyClosedRemote) send({ type: "pty_close", id: ptyId });
  const bundle = tab.leaves.get(ptyId);
  if (bundle) disposeTerminal(bundle);
  tab.leaves.delete(ptyId);

  const nextTree = removeLeaf(tab.paneTree, ptyId);
  const idx = tabs.indexOf(tab);
  if (nextTree === null || tab.leaves.size === 0) {
    if (idx >= 0) tabs.splice(idx, 1);
    if (activeTabIndex >= tabs.length) activeTabIndex = Math.max(0, tabs.length - 1);
    else if (idx >= 0 && activeTabIndex > idx) activeTabIndex -= 1;
  } else {
    tab.paneTree = nextTree;
    if (tab.activeLeafId === ptyId) {
      const ids = collectLeafIds(tab.paneTree);
      tab.activeLeafId = ids[0] ?? tab.activeLeafId;
    }
  }
  renderAll();
  persistLayout();
}

function closeTabAt(index: number, opts: { force?: boolean } = {}): void {
  const tab = tabs[index];
  if (!tab) return;
  if (tab.kind === "editor") {
    if (tab.dirty && !opts.force) {
      if (!confirm(`Discard unsaved changes to ${basename(tab.path)}?`)) return;
    }
    disposeEditorTab(tab);
    tabs.splice(index, 1);
    if (activeTabIndex >= tabs.length) activeTabIndex = Math.max(0, tabs.length - 1);
    else if (activeTabIndex > index) activeTabIndex -= 1;
    renderAll();
    persistLayout();
    return;
  }
  closeWholeTerminalTab(tab);
}

/** Mod+W: close only the active pane if the active tab has more than one; otherwise close the tab. */
function closeActiveTabOrLeaf(): void {
  const tab = tabs[activeTabIndex];
  if (!tab) return;
  if (tab.kind === "terminal" && tab.leaves.size > 1) {
    closeLeaf(tab, tab.activeLeafId);
    return;
  }
  closeTabAt(activeTabIndex);
}

function collapseToActiveLeaf(): void {
  const tab = activeTerminalTab();
  if (!tab) return;
  const others = collectLeafIds(tab.paneTree).filter((id) => id !== tab.activeLeafId);
  if (others.length === 0) {
    setStatusLeft("no split");
    return;
  }
  for (const id of others) {
    send({ type: "pty_close", id });
    const bundle = tab.leaves.get(id);
    if (bundle) disposeTerminal(bundle);
    tab.leaves.delete(id);
  }
  tab.paneTree = { type: "leaf", id: tab.activeLeafId };
  activeTabIndex = tabs.indexOf(tab);
  renderAll();
  persistLayout();
  setStatusLeft("no split");
}

function tabLabel(tab: Tab): string {
  if (tab.kind === "terminal") return tab.title;
  return basename(tab.path);
}

/** Workspace / explorer root used for relative paths. */
function pathAnchorRoot(): string {
  return tree.getWorkspaceRoot() || tree.getRootPath() || "";
}

async function copyPathFeedback(label: string, value: string): Promise<void> {
  const ok = await copyToClipboard(value);
  setStatusLeft(ok ? `copied ${label}: ${value}` : `copy failed: ${value}`);
}

function pathContextItems(absPath: string): ContextMenuItem[] {
  const abs = absPath;
  const rel = relativePath(abs, pathAnchorRoot());
  const name = basename(abs);
  return [
    {
      kind: "item",
      label: "Copy Absolute Path",
      run: () => copyPathFeedback("absolute path", abs),
    },
    {
      kind: "item",
      label: "Copy Relative Path",
      run: () => copyPathFeedback("relative path", rel),
    },
    {
      kind: "item",
      label: "Copy File Name",
      run: () => copyPathFeedback("name", name),
    },
  ];
}

function openPathContextMenu(absPath: string, clientX: number, clientY: number): void {
  if (!absPath) return;
  openContextMenu(clientX, clientY, pathContextItems(absPath));
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
    const label = document.createElement("span");
    label.className = "tab-label";
    label.textContent = tabLabel(tab);
    if (tab.kind === "editor") label.title = tab.path;
    else if (tab.leaves.get(tab.activeLeafId)?.cwd) {
      label.title = tab.leaves.get(tab.activeLeafId)!.cwd!;
    }
    el.appendChild(label);
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
      syncExplorerToActiveContext();
      focusActiveTab();
    });
    el.addEventListener("contextmenu", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      activeTabIndex = i;
      renderAll();
      persistLayout();
      syncExplorerToActiveContext();

      const items: ContextMenuItem[] = [];
      if (tab.kind === "editor") {
        items.push(...pathContextItems(tab.path));
        items.push({ kind: "separator" });
      } else {
        const cwd = tab.leaves.get(tab.activeLeafId)?.cwd;
        if (cwd) {
          items.push(...pathContextItems(cwd));
          items.push({ kind: "separator" });
        }
      }
      items.push({
        kind: "item",
        label: "Close",
        run: () => closeTabAt(i),
      });
      openContextMenu(ev.clientX, ev.clientY, items);
    });
    tabsEl.appendChild(el);
  });

  requestAnimationFrame(measurePill);
}

function buildLeafEl(node: Extract<PaneNode, { type: "leaf" }>, tab: TerminalTab): HTMLElement {
  const bundle = tab.leaves.get(node.id);
  const leafEl = document.createElement("div");
  leafEl.className = "pane-leaf" + (node.id === tab.activeLeafId ? " active" : "");
  leafEl.dataset.leafId = node.id;
  if (bundle) {
    const label = document.createElement("div");
    label.className = "pane-label";
    label.textContent = titleFromCwd(bundle.cwd, tab.title);
    leafEl.appendChild(label);
    leafEl.appendChild(bundle.el);
  }
  leafEl.addEventListener("mousedown", () => focusLeaf(tab, node.id));
  return leafEl;
}

function renderPaneChildren(node: PaneNode, tab: TerminalTab, container: HTMLElement): void {
  if (node.type === "leaf") {
    container.appendChild(buildLeafEl(node, tab));
    return;
  }
  for (const child of node.children) {
    const branch = document.createElement("div");
    branch.className = "pane-branch";
    if (child.type === "leaf") {
      branch.appendChild(buildLeafEl(child, tab));
    } else {
      const nested = document.createElement("div");
      nested.className = `pane-tree ${child.direction === "row" ? "split-row" : "split-col"}`;
      renderPaneChildren(child, tab, nested);
      branch.appendChild(nested);
    }
    container.appendChild(branch);
  }
}

function renderPanes(): void {
  terminalPark.innerHTML = "";
  const active = tabs[activeTabIndex];
  const activeTermTab = active?.kind === "terminal" ? active : null;

  panesEl.innerHTML = "";
  if (activeTermTab) {
    const root = activeTermTab.paneTree;
    panesEl.className =
      root.type === "split" ? `pane-tree ${root.direction === "row" ? "split-row" : "split-col"}` : "pane-tree";
    renderPaneChildren(root, activeTermTab, panesEl);
    for (const id of collectLeafIds(root)) {
      const bundle = activeTermTab.leaves.get(id);
      if (bundle) requestAnimationFrame(() => fitBundle(bundle));
    }
  } else {
    panesEl.className = "pane-tree";
  }

  for (const t of tabs) {
    if (t.kind !== "terminal" || t === activeTermTab) continue;
    for (const bundle of t.leaves.values()) terminalPark.appendChild(bundle.el);
  }
}

function updatePaneActiveClasses(tab: TerminalTab): void {
  panesEl.querySelectorAll<HTMLElement>(".pane-leaf").forEach((el) => {
    el.classList.toggle("active", el.dataset.leafId === tab.activeLeafId);
  });
}

/** Switch focus to a leaf without rebuilding the pane DOM (avoids xterm re-mount flicker). */
function focusLeaf(tab: TerminalTab, leafId: string): void {
  const bundle = tab.leaves.get(leafId);
  if (tab.activeLeafId !== leafId) {
    tab.activeLeafId = leafId;
    updatePaneActiveClasses(tab);
    persistLayout();
    const cwd = bundle?.cwd;
    if (cwd && tabs[activeTabIndex] === tab) syncExplorerToCwd(cwd);
  }
  bundle?.term.focus();
}

function focusPaneRelative(delta: 1 | -1): void {
  const tab = tabs[activeTabIndex];
  if (tab?.kind !== "terminal") return;
  const next = nextLeafId(tab.paneTree, tab.activeLeafId, delta);
  focusLeaf(tab, next);
}

function selectTabRelative(delta: 1 | -1): void {
  if (tabs.length === 0) return;
  activeTabIndex = (activeTabIndex + delta + tabs.length) % tabs.length;
  renderAll();
  persistLayout();
  syncExplorerToActiveContext();
  focusActiveTab();
}

function focusActiveTab(): void {
  const active = tabs[activeTabIndex];
  if (!active) return;
  if (active.kind === "terminal") {
    const bundle = active.leaves.get(active.activeLeafId);
    requestAnimationFrame(() => {
      if (!bundle) return;
      fitBundle(bundle);
      bundle.term.focus();
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
  syncSearchTarget();
}

function clearTabs(): void {
  for (const tab of tabs) {
    if (tab.kind === "terminal") {
      for (const bundle of tab.leaves.values()) disposeTerminal(bundle);
    } else {
      disposeEditorTab(tab);
    }
  }
  tabs = [];
  activeTabIndex = 0;
  pendingPtyIntents.length = 0;
  panesEl.innerHTML = "";
  panesEl.className = "pane-tree";
  terminalPark.innerHTML = "";
  editorStack.innerHTML = "";
  renderAll();
}

function setTreeEmptyHint(show: boolean, text = "Connect to load remote tree"): void {
  treeEl.classList.toggle("empty", show);
  if (show) treeEl.dataset.emptyText = text;
  else delete treeEl.dataset.emptyText;
}

// Selection state lives inside VirtualTree (tree.getSelectedPath()); no duplicate module state needed.
const tree = new VirtualTree(treeEl, fsList, {
  onOpenFile: (entry, preview) => {
    void openEditorTab(entry.path, preview);
  },
  onStatus: (text) => setStatusLeft(text),
  noteInteraction: () => noteTreeInteraction(),
  onRootChange: (rootPath) => updateExplorerTitle(rootPath),
  onContextMenu: (entry, x, y) => openPathContextMenu(entry.path, x, y),
});
setTreeEmptyHint(true);

function clearTree(): void {
  tree.clear();
  pendingFs.clear();
  pendingFsAuth.clear();
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
  try {
    const keepExpanded = silent ? tree.getExpandedPaths() : undefined;
    await tree.loadRoot({ silent, keepExpanded });
    treeLoaded = true;
    setTreeEmptyHint(false);
  } catch (err) {
    if (!treeLoaded) {
      setStatusLeft(`tree error: ${err instanceof Error ? err.message : String(err)}`);
    }
  } finally {
    treeLoading = false;
    if (treeNeedsRefresh) {
      treeNeedsRefresh = false;
      scheduleTreeRefresh();
    }
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
    }, { fontSize: uiSettings.editorFontSize, theme: getResolvedTheme() });

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
    if (configPath && (pathsEqual(opened.path, configPath) || pathsEqual(path, configPath))) {
      configPath = opened.path;
    }
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
    if (isConfigPath(saved.path) || isConfigPath(active.path)) {
      try {
        applyUiSettings(uiSettingsFromConfigText(text));
        setStatusLeft(`saved settings · theme ${uiSettings.theme}`);
      } catch (err) {
        setStatusLeft(
          `saved ${saved.path}, but ui parse failed: ${err instanceof Error ? err.message : String(err)}`,
        );
      }
    } else {
      setStatusLeft(`saved ${saved.path}`);
    }
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

/** Try to restore a single multi-pane terminal tab whose leaf ids exactly match the reattached ptys. */
function restoreTerminalTabsFromBlob(blob: LayoutBlob, ptyList: PtyInfo[]): boolean {
  const ptyIds = ptyList.map((p) => p.id);
  if (ptyIds.length === 0 || blob.version !== 2 || !Array.isArray(blob.tabs)) return false;
  try {
    for (const tb of blob.tabs) {
      if (tb.kind !== "terminal") continue;
      const leafIds = collectLeafIds(tb.paneTree);
      const sameSet = leafIds.length === ptyIds.length && leafIds.every((id) => ptyIds.includes(id));
      if (!sameSet) continue;
      const leaves = new Map<string, TermBundle>();
      for (const id of leafIds) {
        const bundle = makeTerminal();
        wireTerminalLeaf(id, bundle);
        leaves.set(id, bundle);
      }
      const activeLeafId = leafIds.includes(tb.activeLeafId) ? tb.activeLeafId : leafIds[0];
      const tab: TerminalTab = {
        kind: "terminal",
        id: tb.id || nextTabId(),
        title: tb.title || "sh 1",
        paneTree: tb.paneTree,
        leaves,
        activeLeafId,
      };
      tabs.push(tab);
      return true;
    }
  } catch {
    /* malformed layout blob; fall back to one tab per pty */
  }
  return false;
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
      configPath = typeof msg.config_path === "string" && msg.config_path ? msg.config_path : null;
      if (msg.ui) {
        applyUiSettings(normalizeUiSettings(msg.ui));
      }
      send({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        role: "client",
        implementation: "fresh-gui-ui/0.5",
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
      requestNewPty(80, 24, { kind: "newTab" });
      setStatusLeft(`session ${sessionId}`);
      updateStripChips();
      break;
    case "session_attached": {
      sessionId = msg.session_id;
      $input("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      clearTabs();

      let blob: LayoutBlob = {};
      if (typeof msg.layout === "string" && msg.layout) {
        try {
          blob = JSON.parse(msg.layout) as LayoutBlob;
        } catch {
          blob = {};
        }
      } else {
        blob = readLayoutBlob();
      }
      applySidebarFromBlob(blob);

      const ptyList = msg.ptys || [];
      const restored = restoreTerminalTabsFromBlob(blob, ptyList);
      if (!restored) {
        for (const p of ptyList) {
          const bundle = makeTerminal();
          createTerminalTab(p.id, bundle, { silentActivate: true });
        }
      }

      if (terminalTabs().length === 0) {
        requestNewPty(80, 24, { kind: "newTab" });
      } else {
        activeTabIndex = Math.min(Math.max(blob.activeTab ?? 0, 0), tabs.length - 1);
        renderAll();
        persistLayout();
      }
      afterSessionReady();
      setStatusLeft(`reattached ${sessionId} (${ptyList.length} ptys)`);
      updateStripChips();
      break;
    }
    case "pty_opened": {
      const intent = pendingPtyIntents.shift() ?? { kind: "newTab" as const };
      const bundle = makeTerminal();
      let handled = false;
      if (intent.kind === "split") {
        const tab = tabs.find((t): t is TerminalTab => t.kind === "terminal" && t.id === intent.tabId);
        if (tab) {
          const nextTree = splitLeaf(tab.paneTree, intent.leafId, msg.id, intent.direction);
          if (nextTree) {
            tab.paneTree = nextTree;
            tab.leaves.set(msg.id, bundle);
            tab.activeLeafId = msg.id;
            wireTerminalLeaf(msg.id, bundle);
            activeTabIndex = tabs.indexOf(tab);
            renderAll();
            persistLayout();
            setStatusLeft("split pane added");
            requestAnimationFrame(() => {
              fitBundle(bundle);
              bundle.term.focus();
            });
            handled = true;
          } else {
            setStatusLeft(`max ${MAX_LEAVES_PER_TAB} panes per tab; opened new tab instead`);
          }
        }
      }
      if (!handled) createTerminalTab(msg.id, bundle);
      break;
    }
    case "pty_data": {
      const owner = findLeafOwner(msg.id);
      if (owner) {
        const bytes = b64decode(msg.data);
        const text = new TextDecoder().decode(bytes);
        const cwd = feedOsc7Chunk(owner.bundle.oscCarry, text);
        if (cwd) applyLeafCwd(owner.tab, msg.id, cwd);
        owner.bundle.term.write(bytes);
      }
      break;
    }
    case "pty_closed": {
      const owner = findLeafOwner(msg.id);
      if (owner) closeLeaf(owner.tab, msg.id, { alreadyClosedRemote: true });
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
    case "fs_authorized": {
      const pending = pendingFsAuth.get(msg.request_id);
      if (pending) {
        pendingFsAuth.delete(msg.request_id);
        pending.resolve({ path: msg.path });
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
  configPath = null;
  stripForceExpanded = false;
  pendingEditor.clear();
  pendingEdit.clear();
  pendingSave.clear();
  clearTabs();
  clearTree();
  lastTerminalCwd = null;
  explorerSyncGen += 1;
  setTreeEmptyHint(true);
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

function openNewTerminalTab(): void {
  if (!connected) return;
  const dims = proposedDims(activeTerminalTab());
  requestNewPty(dims.cols, dims.rows, { kind: "newTab" }, resolveSpawnCwd());
}

function requestSplit(mode: SplitMode): void {
  if (!connected) return;
  const tab = activeTerminalTab();
  if (!tab) {
    setStatusLeft("open a shell first");
    return;
  }
  if (leafCount(tab.paneTree) >= MAX_LEAVES_PER_TAB) {
    setStatusLeft(`max ${MAX_LEAVES_PER_TAB} panes per tab`);
    return;
  }
  const dims = proposedDims(tab);
  const direction = directionFromSplitMode(mode);
  setStatusLeft(`opening pane (${mode})…`);
  requestNewPty(dims.cols, dims.rows, {
    kind: "split",
    tabId: tab.id,
    leafId: tab.activeLeafId,
    direction,
  }, resolveSpawnCwd());
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
    requestAnimationFrame(fitActiveLeaves);
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
newTabBtn.addEventListener("click", openNewTerminalTab);
splitHBtn.addEventListener("click", () => requestSplit("horizontal"));
splitVBtn.addEventListener("click", () => requestSplit("vertical"));
splitOffBtn.addEventListener("click", collapseToActiveLeaf);
sidebarToggle.addEventListener("click", toggleSidebar);
findBtn.addEventListener("click", () => focusFind());
activityExplorer.addEventListener("click", () => {
  toggleSidebar();
  if (!isSidebarCollapsed()) treeEl.focus();
});
activitySettings.addEventListener("click", () => {
  void openSettingsFile();
});

tabsEl.addEventListener("scroll", () => requestAnimationFrame(measurePill));

const shortcutHandlers: ShortcutHandlers = {
  "commandPalette.open": () => openPalette(),
  "tab.new": () => openNewTerminalTab(),
  "tab.close": () => closeActiveTabOrLeaf(),
  "tab.next": () => selectTabRelative(1),
  "tab.prev": () => selectTabRelative(-1),
  "pane.splitRight": () => requestSplit("horizontal"),
  "pane.splitDown": () => requestSplit("vertical"),
  "pane.focusNext": () => focusPaneRelative(1),
  "pane.focusPrev": () => focusPaneRelative(-1),
  "sidebar.toggle": () => toggleSidebar(),
  "editor.save": () => {
    void saveActiveEditor();
  },
  "search.focus": () => focusFind(),
  "settings.open": () => {
    void openSettingsFile();
  },
};

function runShortcutId(id: ShortcutId): void {
  shortcutHandlers[id]?.(new KeyboardEvent("keydown"));
}

installShortcuts(shortcutHandlers);
setPaletteCommands(defaultPaletteCommands(runShortcutId));

// OS theme flips while preference is "system" → restyle open panes.
onResolvedThemeChange((resolved) => {
  if (uiSettings.theme !== "system") return;
  restyleOpenPanes(resolved, true);
  requestAnimationFrame(fitActiveLeaves);
});

window.addEventListener("resize", () => {
  fitActiveLeaves();
  requestAnimationFrame(measurePill);
});

window.addEventListener("keydown", (ev) => {
  if (ev.key === "Escape" && !ev.ctrlKey && !ev.metaKey && !ev.altKey) {
    closeFindBar();
  }
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
