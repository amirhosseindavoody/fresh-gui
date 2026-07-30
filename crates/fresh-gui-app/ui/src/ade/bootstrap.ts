/** Phase UI-3 host shell: OSC 7 cwd, find bar, activity bar, icons, light theme + settings. */
import type { EditorView } from "@codemirror/view";
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type FsEntry,
  type PtyInfo,
  type ServerMessage,
} from "../protocol";
import { $, $button, b64decode, b64encode, basename, dirname, relativePath } from "../dom";
import {
  applyEditorFontSize,
  applyEditorLineWrap,
  applyEditorMinimap,
  applyEditorTheme,
  createEditorView,
  openEditorSearch,
  revealEditorLocation,
  setEditorDocument,
} from "../editor";
import { isMarkdownPath } from "../markdown-preview";
import {
  mountMarkdownWysiwyg,
  type MarkdownWysiwygHandle,
} from "../markdown-wysiwyg";
import {
  applyTerminalFontSize,
  applyTerminalTheme,
  createTerminal,
  disposeTerminal,
  type TermBundle,
} from "../terminal";
import { feedOsc7Chunk } from "../osc7";
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
} from "../panes";
import {
  DEFAULT_SETTINGS_OPEN_PATH,
  installShortcuts,
  setActiveShortkeys,
  type ShortcutHandlers,
  type ShortcutId,
} from "../shortcuts";
import {
  defaultPaletteCommands,
  openGotoFile,
  openPalette,
  openPaletteWithQuery,
  setGotoFileHandler,
  setPaletteCommands,
  type PaletteCommand,
} from "../palette";
import { VirtualTree } from "../tree";
import {
  LAYOUT_VERSION,
  leafCwdsFromTree,
  normalizeExplorerRootKey,
  planSessionRestore,
  setLeafCwdInTree,
  stampLeafCwds,
  type ExplorerSnapshot,
  type LayoutBlob,
  type PlannedTerminalRestore,
} from "../layout-persist";
import { applyPalette, listPalettes, paletteLabel, type PaletteId } from "../palettes";
import { getResolvedTheme, initTheme, onResolvedThemeChange, resolveTheme } from "../theme";
import {
  applyUiChrome,
  loadSettings,
  normalizeUiSettings,
  saveSettings,
  shortkeysFromConfigText,
  uiSettingsFromConfigText,
  type UiSettings,
} from "../settings";
import { closeFindBar, openFindBar, setSearchTarget } from "../search";
import {
  copyToClipboard,
  openContextMenu,
  openContextMenuForAnchor,
  promptName,
  type ContextMenuItem,
} from "../context-menu";
import {
  cacheAuthToken,
  clearCachedAuthToken,
  consumeTokenQueryParam,
  loadCachedAuthToken,
} from "../auth-token";

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
  line?: number;
  column?: number;
}

interface PendingEditor extends Pending<OpenedInfo & { rev: number; text: string }> {
  _opened?: OpenedInfo;
}

type OpenEditorOpts = {
  preview?: boolean;
  cwd?: string;
  line?: number;
  column?: number;
  pinned?: boolean;
  /** Bulk session restore: skip activate / persist / focus. */
  silent?: boolean;
};

interface TerminalTab {
  kind: "terminal";
  /** Tab id, distinct from any pty id (a tab may host several ptys as leaves). */
  id: string;
  title: string;
  paneTree: PaneNode;
  leaves: Map<string, TermBundle>;
  activeLeafId: string;
  /** Pinned tabs live in a dedicated strip and stay before unpinned tabs. */
  pinned: boolean;
}

interface EditorTab {
  kind: "editor";
  id: string;
  bufferId: string;
  path: string;
  rev: number;
  dirty: boolean;
  preview: boolean;
  /** Rendered markdown WYSIWYG view vs source (only used for markdown paths). */
  mdView: "source" | "preview";
  view: EditorView;
  host: HTMLElement;
  mdPreviewEl: HTMLElement | null;
  mdWysiwyg: MarkdownWysiwygHandle | null;
  suppressChange: boolean;
  pinned: boolean;
}

type Tab = TerminalTab | EditorTab;
type SplitMode = "horizontal" | "vertical";

/** Deferred intent consumed by the next `pty_opened` reply (requests are FIFO on one socket). */
type PtyIntent = { kind: "newTab" } | { kind: "split"; tabId: string; leafId: string; direction: SplitDir };

let workspaceEl: HTMLElement;
let sidebarToggle: HTMLButtonElement;
let sidebarResizer: HTMLElement;
let treeEl: HTMLElement;
let tabsEl: HTMLElement;
let tabPill: HTMLElement;
let pinnedTabsEl: HTMLElement;
let pinnedTabPill: HTMLElement;
let pinnedTabsSep: HTMLElement;
let panesEl: HTMLElement;
let emptyStack: HTMLElement;
let terminalStack: HTMLElement;
let editorStack: HTMLElement;
let statusLeft: HTMLElement;
let statusRight: HTMLElement;
let tabsMenuBtn: HTMLButtonElement;
let newTabBtn: HTMLButtonElement;
let sidebarParentBtn: HTMLButtonElement;
let activityExplorer: HTMLButtonElement;
let activityPalette: HTMLButtonElement;
let activitySettings: HTMLButtonElement;

let uiSettings: UiSettings = loadSettings();
/** Absolute path to backend `config.json` (from Hello). */
let configPath: string | null = null;

let terminalPark: HTMLDivElement;

let ws: WebSocket | null = null;
let sessionId: string | null = null;
let reqSeq = 0;
let tabSeq = 0;
let hasEditor = false;
let watchId: string | null = null;
let connected = false;

/** Backend WebSocket URL (derived from page origin / Vite default — no connect form). */
let wsUrl = "";
/** Bearer token from `?token=` / sessionStorage cache. */
let authToken = "";
/** Preferred session id for attach (localStorage-backed). */
let preferredSessionId = "";

const pendingFs = new Map<string, Pending<{ path: string; entries: FsEntry[] }>>();
const pendingFsAuth = new Map<string, Pending<{ path: string }>>();
const pendingFsMutate = new Map<string, Pending<FsEntry[]>>();
const pendingFsDelete = new Map<string, Pending<string[]>>();
const pendingEditor = new Map<string, PendingEditor>();
const pendingEdit = new Map<string, Pending<number>>();
const pendingSave = new Map<string, Pending<{ path: string; rev: number }>>();
const pendingPtyIntents: PtyIntent[] = [];

/** In-app explorer clipboard for Cut / Copy / Paste (paths only). */
type FileClipboard = { mode: "copy" | "cut"; paths: string[] };
let fileClipboard: FileClipboard | null = null;

let treeRefreshTimer: ReturnType<typeof setTimeout> | null = null;
let treeLoading = false;
let treeLoaded = false;
let treeNeedsRefresh = false;
let treeQuietUntil = 0;

/** Keep in sync with backend `WATCH_IGNORE_DIRS` in `fs_watch.rs`. */
const WATCH_IGNORE_DIRS = new Set([
  ".git",
  "target",
  ".pixi",
  "node_modules",
  "vendor",
  ".cursor",
  "dist",
  ".hg",
  ".svn",
  "__pycache__",
  ".next",
  ".cache",
  "build",
]);

let tabs: Tab[] = [];
let activeTabIndex = 0;
/** Suppress layout_set / localStorage writes during bulk session restore. */
let restoringSession = false;
/** Per view-root explorer expanded/scroll (also written into the layout blob). */
const explorerByRoot = new Map<string, ExplorerSnapshot>();
let explorerPersistTimer: ReturnType<typeof setTimeout> | null = null;

function setStatusLeft(text: string): void {
  statusLeft.textContent = text;
  statusLeft.title = text;
}

function updateStatusRight(): void {
  const caps: string[] = [];
  if (connected) caps.push("online");
  else if (ws) caps.push("connecting");
  if (sessionId) {
    caps.push(sessionId.length > 12 ? `${sessionId.slice(0, 12)}…` : sessionId);
  }
  if (hasEditor) caps.push("editor");
  if (connected) caps.push("fs");
  if (watchId) caps.push("watch");
  const dirty = tabs.filter((t) => t.kind === "editor" && t.dirty).length;
  if (dirty > 0) caps.push(`${dirty} dirty`);
  statusRight.textContent = caps.join(" · ");
  statusRight.title = sessionId ? `session ${sessionId}` : "";
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
  if (restoringSession) return;
  rememberCurrentExplorerUi();
  const cwdByLeaf = new Map<string, string | undefined>();
  for (const t of tabs) {
    if (t.kind !== "terminal") continue;
    for (const [id, bundle] of t.leaves) cwdByLeaf.set(id, bundle.cwd);
  }
  const explorerEntries = Object.fromEntries(explorerByRoot.entries());
  const layout: LayoutBlob = {
    version: LAYOUT_VERSION,
    activeTab: activeTabIndex,
    sidebarWidth: Number.parseInt(
      getComputedStyle(document.documentElement).getPropertyValue("--sidebar-width") || "260",
      10,
    ),
    sidebarCollapsed: isSidebarCollapsed(),
    tabs: tabs.map((t) =>
      t.kind === "terminal"
        ? {
            kind: "terminal",
            id: t.id,
            title: t.title,
            paneTree: stampLeafCwds(t.paneTree, cwdByLeaf),
            activeLeafId: t.activeLeafId,
            pinned: t.pinned,
          }
        : { kind: "editor", id: t.id, path: t.path, preview: t.preview, pinned: t.pinned },
    ),
    explorerByRoot: Object.keys(explorerEntries).length > 0 ? explorerEntries : undefined,
  };
  const json = JSON.stringify(layout);
  localStorage.setItem(LAYOUT_KEY, json);
  if (sessionId) send({ type: "layout_set", layout: json });
}

function rememberCurrentExplorerUi(): void {
  let root: string;
  let snap: ExplorerSnapshot;
  try {
    root = tree.getRootPath();
    if (!root) return;
    snap = tree.captureExplorerUi();
  } catch {
    return;
  }
  explorerByRoot.set(normalizeExplorerRootKey(root), snap);
}

function scheduleExplorerPersist(): void {
  rememberCurrentExplorerUi();
  if (explorerPersistTimer) clearTimeout(explorerPersistTimer);
  explorerPersistTimer = setTimeout(() => {
    explorerPersistTimer = null;
    persistLayout();
  }, 250);
}

function loadExplorerSnapshotsFromBlob(blob: LayoutBlob): void {
  explorerByRoot.clear();
  const raw = blob.explorerByRoot;
  if (!raw || typeof raw !== "object") return;
  for (const [key, snap] of Object.entries(raw)) {
    if (!snap || !Array.isArray(snap.expanded)) continue;
    explorerByRoot.set(normalizeExplorerRootKey(key), {
      expanded: snap.expanded.filter((p): p is string => typeof p === "string"),
      scrollTop: typeof snap.scrollTop === "number" ? Math.max(0, snap.scrollTop) : 0,
    });
  }
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

function fsCreate(parent: string, name: string, kind: "file" | "dir"): Promise<FsEntry> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFsMutate.set(request_id, {
      resolve: (entries) => resolve(entries[0]!),
      reject,
    });
    send({ type: "fs_create", request_id, parent: parent || "", name, kind });
    setTimeout(() => {
      if (pendingFsMutate.has(request_id)) {
        pendingFsMutate.delete(request_id);
        reject(new Error("fs_create timeout"));
      }
    }, 15_000);
  });
}

function fsCopy(sources: string[], destination: string): Promise<FsEntry[]> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFsMutate.set(request_id, { resolve, reject });
    send({ type: "fs_copy", request_id, sources, destination });
    setTimeout(() => {
      if (pendingFsMutate.has(request_id)) {
        pendingFsMutate.delete(request_id);
        reject(new Error("fs_copy timeout"));
      }
    }, 60_000);
  });
}

function fsMove(sources: string[], destination: string): Promise<FsEntry[]> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFsMutate.set(request_id, { resolve, reject });
    send({ type: "fs_move", request_id, sources, destination });
    setTimeout(() => {
      if (pendingFsMutate.has(request_id)) {
        pendingFsMutate.delete(request_id);
        reject(new Error("fs_move timeout"));
      }
    }, 60_000);
  });
}

function fsDelete(paths: string[]): Promise<string[]> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingFsDelete.set(request_id, { resolve, reject });
    send({ type: "fs_delete", request_id, paths });
    setTimeout(() => {
      if (pendingFsDelete.has(request_id)) {
        pendingFsDelete.delete(request_id);
        reject(new Error("fs_delete timeout"));
      }
    }, 60_000);
  });
}

function editorOpen(
  path: string,
  opts: OpenEditorOpts = {},
): Promise<OpenedInfo & { rev: number; text: string }> {
  const request_id = nextRequestId();
  const preview = !!opts.preview;
  return new Promise((resolve, reject) => {
    pendingEditor.set(request_id, { resolve, reject });
    send({
      type: "editor_open",
      request_id,
      path,
      preview,
      ...(opts.cwd ? { cwd: opts.cwd } : {}),
      ...(opts.line != null ? { line: opts.line } : {}),
      ...(opts.column != null ? { column: opts.column } : {}),
    });
    setTimeout(() => {
      if (pendingEditor.has(request_id)) {
        pendingEditor.delete(request_id);
        reject(new Error("editor_open timeout"));
      }
    }, 15000);
  });
}

function editorOpenLink(
  lineText: string,
  column: number,
  opts: { preview?: boolean; cwd?: string } = {},
): Promise<OpenedInfo & { rev: number; text: string }> {
  const request_id = nextRequestId();
  return new Promise((resolve, reject) => {
    pendingEditor.set(request_id, { resolve, reject });
    send({
      type: "editor_open_link",
      request_id,
      line_text: lineText,
      column,
      preview: !!opts.preview,
      ...(opts.cwd ? { cwd: opts.cwd } : {}),
    });
    setTimeout(() => {
      if (pendingEditor.has(request_id)) {
        pendingEditor.delete(request_id);
        reject(new Error("editor_open_link timeout"));
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
    tryReject(pendingFsMutate) ||
    tryReject(pendingFsDelete) ||
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

/** Browser tab title: active working directory (or editor file) — product name. */
function syncDocumentTitle(): void {
  const active = tabs[activeTabIndex];
  let head: string | null = null;
  if (active?.kind === "terminal") {
    const cwd = active.leaves.get(active.activeLeafId)?.cwd || lastTerminalCwd;
    if (cwd) head = titleFromCwd(cwd, "");
  } else if (active?.kind === "editor") {
    head = basename(active.path) || null;
  }
  if (!head && lastTerminalCwd) head = titleFromCwd(lastTerminalCwd, "");
  document.title = head ? `${head} — fresh-gui` : "fresh-gui";
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
  } else {
    title.textContent = basename(rootPath) || "Explorer";
    title.title = rootPath;
  }
  updateParentButton();
}

function explorerParentPath(rootPath: string): string | null {
  if (!rootPath) return null;
  const parent = dirname(rootPath);
  if (!parent) return null;
  if (normalizePathSafe(parent) === normalizePathSafe(rootPath)) return null;
  return parent;
}

function updateParentButton(): void {
  if (!sidebarParentBtn) return;
  if (typeof tree === "undefined") {
    sidebarParentBtn.disabled = true;
    return;
  }
  const parent = explorerParentPath(tree.getRootPath() || "");
  sidebarParentBtn.disabled = !connected || !parent;
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
      rememberCurrentExplorerUi();
      await fsAuthorize(cwd);
      if (gen !== explorerSyncGen) return;
      const snap = explorerByRoot.get(normalizeExplorerRootKey(cwd)) ?? null;
      const ok = await tree.setViewRoot(cwd, { restore: snap });
      if (gen !== explorerSyncGen) return;
      if (!ok) {
        setStatusLeft(`explorer stuck at ${tree.getRootPath() || "?"} · want ${cwd}`);
        return;
      }
      updateExplorerTitle(tree.getRootPath() || cwd);
      rememberCurrentExplorerUi();
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
  tab.paneTree = setLeafCwdInTree(tab.paneTree, ptyId, cwd);

  const follow = tab.activeLeafId === ptyId || tab.leaves.size === 1;
  if (follow) {
    tab.title = titleFromCwd(cwd, tab.title);
    renderTabs();
    lastTerminalCwd = cwd;
    if (tabs[activeTabIndex] === tab) {
      setStatusLeft(cwd);
      syncDocumentTitle();
    }
    syncExplorerToCwd(cwd);
  }

  panesEl.querySelectorAll<HTMLElement>(".pane-leaf").forEach((el) => {
    if (el.dataset.leafId !== ptyId) return;
    const label = el.querySelector(".pane-label");
    if (label) label.textContent = titleFromCwd(cwd, tab.title);
  });
  persistLayout();
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
  const prev = uiSettings;
  uiSettings = next;
  saveSettings(next);
  const chromeChanged =
    prev.theme !== next.theme ||
    prev.palette !== next.palette ||
    prev.terminalFontSize !== next.terminalFontSize ||
    prev.editorFontSize !== next.editorFontSize ||
    prev.fontWeight !== next.fontWeight ||
    prev.monoFontWeight !== next.monoFontWeight ||
    prev.fontFamily !== next.fontFamily ||
    prev.monoFontFamily !== next.monoFontFamily;
  const minimapChanged = prev.editorMinimap !== next.editorMinimap;
  const lineWrapChanged = prev.editorLineWrap !== next.editorLineWrap;
  restyleOpenPanes(getResolvedTheme(), chromeChanged, { minimapChanged, lineWrapChanged });
  tree.setVisibility({
    showDotfiles: next.showDotfiles,
    showGitDirs: next.showGitDirs,
  });
  if (prev.palette !== next.palette) refreshPaletteCommands?.();
  requestAnimationFrame(fitActiveLeaves);
}

/** Patch `ui.palette` in a config.json document (JSONC-tolerant string replace). */
function patchPaletteInConfigText(text: string, palette: PaletteId): string {
  if (/"palette"\s*:/.test(text)) {
    return text.replace(/("palette"\s*:\s*")([^"]*)(")/, `$1${palette}$3`);
  }
  if (/"ui"\s*:\s*\{/.test(text)) {
    return text.replace(/("ui"\s*:\s*\{)/, `$1\n    "palette": "${palette}",`);
  }
  // Bare / empty config — write a minimal ui block the backend template understands.
  const trimmed = text.trim();
  if (!trimmed || trimmed === "{}") {
    return `{\n  "ui": {\n    "palette": "${palette}"\n  }\n}\n`;
  }
  return text;
}

async function persistPaletteToConfig(palette: PaletteId): Promise<void> {
  if (!configPath) throw new Error("no config path");
  const existing = tabs.find(
    (t): t is EditorTab => t.kind === "editor" && isConfigPath(t.path),
  );
  if (existing) {
    const next = patchPaletteInConfigText(existing.view.state.doc.toString(), palette);
    existing.suppressChange = true;
    existing.view.dispatch({
      changes: { from: 0, to: existing.view.state.doc.length, insert: next },
    });
    existing.suppressChange = false;
    existing.dirty = true;
    const revAfterEdit = await bufferEdit(existing.bufferId, existing.rev, next);
    existing.rev = revAfterEdit;
    const saved = await bufferSave(existing.bufferId, existing.rev);
    existing.rev = saved.rev;
    existing.dirty = false;
    existing.preview = false;
    renderAll();
    return;
  }
  const opened = await editorOpen(configPath, { preview: false });
  const next = patchPaletteInConfigText(opened.text, palette);
  const revAfterEdit = await bufferEdit(opened.buffer_id, opened.rev, next);
  await bufferSave(opened.buffer_id, revAfterEdit);
}

/** Patch a boolean `ui.<key>` in config.json (JSONC-tolerant). */
function patchUiBoolInConfigText(text: string, key: string, value: boolean): string {
  const lit = value ? "true" : "false";
  const keyRe = new RegExp(`("${key}"\\s*:\\s*)(true|false)`);
  if (keyRe.test(text)) {
    return text.replace(keyRe, `$1${lit}`);
  }
  if (/"ui"\s*:\s*\{/.test(text)) {
    return text.replace(/("ui"\s*:\s*\{)/, `$1\n    "${key}": ${lit},`);
  }
  const trimmed = text.trim();
  if (!trimmed || trimmed === "{}") {
    return `{\n  "ui": {\n    "${key}": ${lit}\n  }\n}\n`;
  }
  return text;
}

async function persistUiBoolToConfig(key: string, value: boolean): Promise<void> {
  if (!configPath) throw new Error("no config path");
  const existing = tabs.find(
    (t): t is EditorTab => t.kind === "editor" && isConfigPath(t.path),
  );
  if (existing) {
    const next = patchUiBoolInConfigText(existing.view.state.doc.toString(), key, value);
    existing.suppressChange = true;
    existing.view.dispatch({
      changes: { from: 0, to: existing.view.state.doc.length, insert: next },
    });
    existing.suppressChange = false;
    existing.dirty = true;
    const revAfterEdit = await bufferEdit(existing.bufferId, existing.rev, next);
    existing.rev = revAfterEdit;
    const saved = await bufferSave(existing.bufferId, existing.rev);
    existing.rev = saved.rev;
    existing.dirty = false;
    existing.preview = false;
    renderAll();
    return;
  }
  const opened = await editorOpen(configPath, { preview: false });
  const next = patchUiBoolInConfigText(opened.text, key, value);
  const revAfterEdit = await bufferEdit(opened.buffer_id, opened.rev, next);
  await bufferSave(opened.buffer_id, revAfterEdit);
}

/** Toggle soft wrap (Fresh ToggleLineWrap); persist to config when connected. */
async function toggleEditorLineWrap(): Promise<void> {
  const next = !uiSettings.editorLineWrap;
  applyUiSettings({ ...uiSettings, editorLineWrap: next });
  setStatusLeft(next ? "editor line wrap on" : "editor line wrap off");
  if (!configPath || !connected) return;
  try {
    await persistUiBoolToConfig("editorLineWrap", next);
    setStatusLeft(
      next ? "editor line wrap on · saved to config" : "editor line wrap off · saved to config",
    );
  } catch (err) {
    setStatusLeft(
      `${next ? "line wrap on" : "line wrap off"} · applied locally (${err instanceof Error ? err.message : String(err)})`,
    );
  }
}

/** Filled inside `bootstrapAde` so command palette can list shortcuts + palettes. */
let refreshPaletteCommands: (() => void) | null = null;

async function choosePalette(id: PaletteId): Promise<void> {
  applyUiSettings({ ...uiSettings, palette: id });
  refreshPaletteCommands?.();
  if (!connected || !hasEditor || !configPath) {
    setStatusLeft(`palette · ${paletteLabel(id)}`);
    return;
  }
  try {
    await persistPaletteToConfig(id);
    setStatusLeft(`palette · ${paletteLabel(id)} · saved to config`);
  } catch (err) {
    setStatusLeft(
      `palette · ${paletteLabel(id)} · applied locally (${err instanceof Error ? err.message : String(err)})`,
    );
  }
}

function pathsEqual(a: string, b: string): boolean {
  return a === b || a.replace(/\/+$/, "") === b.replace(/\/+$/, "");
}

function isConfigPath(path: string): boolean {
  return !!configPath && pathsEqual(path, configPath);
}

/** Open backend `config.json` as an editor tab (theme, fonts, shell, shortkeys, …). */
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

/** Open the embedded default-settings catalog (temp file; deleted on close). */
async function openDefaultSettingsFile(): Promise<void> {
  if (!connected) {
    setStatusLeft("connect to a backend to view default settings");
    return;
  }
  if (!hasEditor) {
    setStatusLeft("editor unavailable — cannot open default settings");
    return;
  }
  await openEditorTab(DEFAULT_SETTINGS_OPEN_PATH, false);
  setStatusLeft("default settings (read-only catalog — copy into your user config)");
}

/** Restyle open terminals/editors to match the resolved chrome theme / palette. */
function restyleOpenPanes(
  resolved: ReturnType<typeof resolveTheme>,
  themeChanged: boolean,
  opts: { minimapChanged?: boolean; lineWrapChanged?: boolean } = {},
): void {
  for (const tab of tabs) {
    if (tab.kind === "terminal") {
      for (const bundle of tab.leaves.values()) {
        applyTerminalFontSize(bundle, uiSettings.terminalFontSize);
        if (themeChanged) applyTerminalTheme(bundle);
      }
    } else {
      applyEditorFontSize(tab.view, uiSettings.editorFontSize, uiSettings.monoFontWeight);
      if (themeChanged) {
        applyEditorTheme(tab.view, resolved);
        if (tab.mdView === "preview" && tab.mdPreviewEl) {
          // Preserve DOM edits, then re-render for Mermaid theme colors.
          syncWysiwygToCodeMirror(tab);
          applyEditorMdView(tab, { refresh: true });
        }
      }
      if (opts.minimapChanged) {
        void applyEditorMinimap(tab.view, uiSettings.editorMinimap);
      }
      if (opts.lineWrapChanged) {
        applyEditorLineWrap(tab.view, uiSettings.editorLineWrap);
      }
    }
  }
}

function measurePillIn(
  listEl: HTMLElement,
  pillEl: HTMLElement,
): void {
  const activeEl = listEl.querySelector(".tab.active");
  if (!(activeEl instanceof HTMLElement)) {
    pillEl.hidden = true;
    return;
  }
  pillEl.hidden = false;
  const listRect = listEl.getBoundingClientRect();
  const rect = activeEl.getBoundingClientRect();
  pillEl.style.left = `${rect.left - listRect.left + listEl.scrollLeft}px`;
  pillEl.style.width = `${rect.width}px`;
}

function measurePill(): void {
  if (tabs.length === 0) {
    tabPill.hidden = true;
    pinnedTabPill.hidden = true;
    return;
  }
  const active = tabs[activeTabIndex];
  if (active?.pinned) {
    tabPill.hidden = true;
    measurePillIn(pinnedTabsEl, pinnedTabPill);
  } else {
    pinnedTabPill.hidden = true;
    measurePillIn(tabsEl, tabPill);
  }
}

function setWorkspaceControls(enabled: boolean): void {
  tabsMenuBtn.disabled = !enabled;
  newTabBtn.disabled = !enabled;
  updateParentButton();
}

function updateStacks(): void {
  const hasTabs = tabs.length > 0;
  emptyStack.hidden = connected && hasTabs;
  emptyStack.textContent = connected
    ? "Open a terminal or file to get started"
    : "Open the printed Local access URL to connect";
  const active = tabs[activeTabIndex];
  terminalStack.hidden = !connected || active?.kind !== "terminal";
  editorStack.hidden = !connected || active?.kind !== "editor";
  tabs.forEach((t, i) => {
    if (t.kind === "editor") {
      t.host.hidden = i !== activeTabIndex;
      if (i === activeTabIndex) applyEditorMdView(t);
    }
  });
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
    onCopied: (text) => {
      const preview = text.length > 48 ? `${text.slice(0, 48)}…` : text;
      const oneLine = preview.replace(/\s+/g, " ");
      setStatusLeft(`copied (${text.length} chars): ${oneLine}`);
    },
    onPasteFailed: (message) => setStatusLeft(message),
    onPathLink: ({ lineText, column }) => {
      const cwd = bundle.cwd || lastTerminalCwd || undefined;
      void openEditorFromLink(lineText, column, { preview: true, cwd });
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
  opts: { silentActivate?: boolean; title?: string; pinned?: boolean } = {},
): TerminalTab {
  const title = opts.title || `sh ${terminalTabs().length + 1}`;
  const tab: TerminalTab = {
    kind: "terminal",
    id: nextTabId(),
    title,
    paneTree: { type: "leaf", id: ptyId },
    leaves: new Map([[ptyId, bundle]]),
    activeLeafId: ptyId,
    pinned: !!opts.pinned,
  };
  wireTerminalLeaf(ptyId, bundle);
  insertTab(tab);
  if (!opts.silentActivate) activeTabIndex = tabs.indexOf(tab);
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
    tab.mdWysiwyg?.destroy();
  } catch {
    /* ignore */
  }
  tab.mdWysiwyg = null;
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

/** Close every tab matching `pred`, iterating by object identity so indices stay valid. */
function closeTabsMatching(pred: (tab: Tab) => boolean): void {
  const targets = tabs.filter(pred);
  for (const tab of targets) {
    const index = tabs.indexOf(tab);
    if (index >= 0) closeTabAt(index);
  }
}

function closeAllEditors(): void {
  closeTabsMatching((tab) => tab.kind === "editor");
}

function closeAllTerminals(): void {
  closeTabsMatching((tab) => tab.kind === "terminal");
}

function closeOtherTerminals(): void {
  const active = tabs[activeTabIndex];
  closeTabsMatching((tab) => tab.kind === "terminal" && tab !== active);
}

function closeOtherTabs(): void {
  const active = tabs[activeTabIndex];
  if (!active) return;
  closeTabsMatching((tab) => tab !== active);
}

/** Keep pinned tabs before unpinned; preserve active tab identity. */
function normalizeTabOrder(): void {
  const active = tabs[activeTabIndex];
  const pinned = tabs.filter((t) => t.pinned);
  const unpinned = tabs.filter((t) => !t.pinned);
  tabs = [...pinned, ...unpinned];
  if (active) activeTabIndex = Math.max(0, tabs.indexOf(active));
}

function insertTab(tab: Tab): void {
  if (tab.pinned) {
    const pinnedCount = tabs.filter((t) => t.pinned).length;
    tabs.splice(pinnedCount, 0, tab);
  } else {
    tabs.push(tab);
  }
}

function setTabPinned(tab: Tab, pinned: boolean): void {
  if (tab.pinned === pinned) return;
  const active = tabs[activeTabIndex];
  tab.pinned = pinned;
  normalizeTabOrder();
  if (active) activeTabIndex = Math.max(0, tabs.indexOf(active));
  renderAll();
  persistLayout();
  syncExplorerToActiveContext();
  syncDocumentTitle();
  focusActiveTab();
}

function toggleActiveTabPinned(): void {
  const active = tabs[activeTabIndex];
  if (!active) return;
  setTabPinned(active, !active.pinned);
}

/**
 * Move the tab at `fromIndex` so it is inserted before `insertBefore`
 * (indexes refer to the array before the move). Clamped within its pin group.
 */
function reorderTab(fromIndex: number, insertBefore: number): void {
  if (fromIndex < 0 || fromIndex >= tabs.length) return;
  if (insertBefore === fromIndex || insertBefore === fromIndex + 1) return;
  const active = tabs[activeTabIndex];
  const tab = tabs[fromIndex]!;
  tabs.splice(fromIndex, 1);
  let insert = insertBefore > fromIndex ? insertBefore - 1 : insertBefore;
  const pinnedCount = tabs.filter((t) => t.pinned).length;
  if (tab.pinned) {
    insert = Math.max(0, Math.min(insert, pinnedCount));
  } else {
    insert = Math.max(pinnedCount, Math.min(insert, tabs.length));
  }
  tabs.splice(insert, 0, tab);
  if (active) activeTabIndex = Math.max(0, tabs.indexOf(active));
  renderAll();
  persistLayout();
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

function setFileClipboard(mode: "copy" | "cut", paths: string[]): void {
  const unique = [...new Set(paths.filter(Boolean))];
  if (!unique.length) {
    fileClipboard = null;
    return;
  }
  fileClipboard = { mode, paths: unique };
  const label = unique.length === 1 ? basename(unique[0]!) : `${unique.length} items`;
  setStatusLeft(`${mode === "cut" ? "cut" : "copied"} ${label}`);
}

function pasteTargetDir(entryPath: string, kind: FsEntry["kind"] | "dir"): string {
  if (kind === "dir") return entryPath;
  return dirname(entryPath) || tree.getRootPath() || "";
}

async function runPasteInto(destination: string): Promise<void> {
  if (!fileClipboard?.paths.length) {
    setStatusLeft("clipboard is empty");
    return;
  }
  const { mode, paths } = fileClipboard;
  setStatusLeft(`${mode === "cut" ? "moving" : "copying"}…`);
  try {
    const entries =
      mode === "cut"
        ? await fsMove(paths, destination)
        : await fsCopy(paths, destination);
    if (mode === "cut") fileClipboard = null;
    scheduleTreeRefresh();
    const label =
      entries.length === 1 ? basename(entries[0]!.path) : `${entries.length} items`;
    setStatusLeft(`${mode === "cut" ? "moved" : "copied"} ${label}`);
  } catch (err) {
    setStatusLeft(`${mode} failed: ${err instanceof Error ? err.message : String(err)}`);
  }
}

async function runCreate(parent: string, kind: "file" | "dir"): Promise<void> {
  const title = kind === "dir" ? "New Folder" : "New File";
  const name = await promptName({
    title,
    placeholder: kind === "dir" ? "folder-name" : "file-name.ts",
    confirmLabel: "Create",
  });
  if (!name) return;
  setStatusLeft(`creating ${name}…`);
  try {
    const entry = await fsCreate(parent, name, kind);
    scheduleTreeRefresh();
    setStatusLeft(`created ${entry.path}`);
    if (kind === "file") {
      void openEditorTab(entry.path, { preview: false });
    }
  } catch (err) {
    setStatusLeft(`create failed: ${err instanceof Error ? err.message : String(err)}`);
  }
}

function openTerminalAtPath(abs: string, kind: FsEntry["kind"]): void {
  if (!connected) {
    setStatusLeft("connect to open a terminal");
    return;
  }
  const cwd = kind === "dir" ? abs : dirname(abs) || abs;
  if (!cwd) {
    setStatusLeft("no folder to open in terminal");
    return;
  }
  const dims = proposedDims(activeTerminalTab());
  requestNewPty(dims.cols, dims.rows, { kind: "newTab" }, cwd);
}

function closeEditorsUnderPaths(deleted: string[]): void {
  const norms = deleted.map(normalizePathSafe);
  for (let i = tabs.length - 1; i >= 0; i -= 1) {
    const tab = tabs[i];
    if (!tab || tab.kind !== "editor") continue;
    const path = normalizePathSafe(tab.path);
    if (norms.some((d) => path === d || path.startsWith(`${d}/`))) {
      closeTabAt(i, { force: true });
    }
  }
}

async function runDelete(abs: string, kind: FsEntry["kind"]): Promise<void> {
  const label = basename(abs) || abs;
  const noun = kind === "dir" ? "folder" : "file";
  if (
    !confirm(
      `Permanently delete ${noun} “${label}”?\n\n${abs}\n\nThis cannot be undone.`,
    )
  ) {
    return;
  }
  setStatusLeft(`deleting ${label}…`);
  try {
    const deleted = await fsDelete([abs]);
    if (fileClipboard?.paths.some((p) => deleted.some((d) => normalizePathSafe(p) === normalizePathSafe(d) || normalizePathSafe(p).startsWith(`${normalizePathSafe(d)}/`)))) {
      fileClipboard = null;
    }
    closeEditorsUnderPaths(deleted);
    scheduleTreeRefresh();
    setStatusLeft(`deleted ${label}`);
  } catch (err) {
    setStatusLeft(`delete failed: ${err instanceof Error ? err.message : String(err)}`);
  }
}

function pathContextItems(
  absPath: string,
  opts: { kind?: FsEntry["kind"]; includeFileOps?: boolean } = {},
): ContextMenuItem[] {
  const abs = absPath;
  const rel = relativePath(abs, pathAnchorRoot());
  const name = basename(abs);
  const includeFileOps = opts.includeFileOps !== false;
  const kind = opts.kind ?? "file";
  const items: ContextMenuItem[] = [];

  if (includeFileOps) {
    const parent = pasteTargetDir(abs, kind);
    const workspaceRoot = tree.getWorkspaceRoot() || tree.getRootPath() || "";
    const isWorkspaceRoot =
      !!workspaceRoot && normalizePathSafe(abs) === normalizePathSafe(workspaceRoot);
    const isViewRoot =
      !!tree.getRootPath() && normalizePathSafe(abs) === normalizePathSafe(tree.getRootPath());
    items.push(
      {
        kind: "item",
        label: "Open in Terminal",
        disabled: !connected,
        run: () => openTerminalAtPath(abs, kind),
      },
      { kind: "separator" },
      {
        kind: "item",
        label: "New File…",
        run: () => runCreate(parent, "file"),
      },
      {
        kind: "item",
        label: "New Folder…",
        run: () => runCreate(parent, "dir"),
      },
      { kind: "separator" },
      {
        kind: "item",
        label: "Cut",
        disabled: isWorkspaceRoot,
        run: () => setFileClipboard("cut", [abs]),
      },
      {
        kind: "item",
        label: "Copy",
        disabled: isWorkspaceRoot,
        run: () => setFileClipboard("copy", [abs]),
      },
      {
        kind: "item",
        label: "Paste",
        disabled: !fileClipboard?.paths.length,
        run: () => runPasteInto(parent),
      },
      { kind: "separator" },
      {
        kind: "item",
        label: "Delete…",
        variant: "destructive",
        disabled: isWorkspaceRoot || isViewRoot,
        run: () => runDelete(abs, kind),
      },
      { kind: "separator" },
    );
  }

  items.push(
    {
      kind: "item",
      label: "Copy Path",
      run: () => copyPathFeedback("path", abs),
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
  );
  return items;
}

function normalizePathSafe(path: string): string {
  return String(path || "").replace(/\\/g, "/").replace(/\/+$/, "") || "/";
}

function openPathContextMenu(
  absPath: string,
  clientX: number,
  clientY: number,
  kind: FsEntry["kind"] = "file",
): void {
  if (!absPath) return;
  openContextMenu(clientX, clientY, pathContextItems(absPath, { kind, includeFileOps: true }));
}

const TAB_DRAG_THRESHOLD_PX = 4;

type TabDragState = {
  fromIndex: number;
  startX: number;
  startY: number;
  dragging: boolean;
  pointerId: number;
  overIndex: number | null;
};

let tabDrag: TabDragState | null = null;
/** Ignore the click that follows a completed tab drag. */
let suppressNextTabClick = false;

function clearTabDragOver(): void {
  for (const el of document.querySelectorAll(".tab.drag-over, .tab.dragging")) {
    el.classList.remove("drag-over", "dragging");
  }
}

function tabIndexFromPoint(clientX: number, clientY: number, pinned: boolean): number | null {
  const list = pinned ? pinnedTabsEl : tabsEl;
  const nodes = [...list.querySelectorAll<HTMLElement>(".tab")];
  if (nodes.length === 0) return null;
  for (const el of nodes) {
    const rect = el.getBoundingClientRect();
    if (clientX < rect.left || clientX > rect.right) continue;
    if (clientY < rect.top - 8 || clientY > rect.bottom + 8) continue;
    const mid = rect.left + rect.width / 2;
    const index = Number(el.dataset.index);
    if (!Number.isFinite(index)) continue;
    return clientX < mid ? index : Math.min(index + 1, tabs.length);
  }
  // Past the end of the strip → append within this group.
  const last = nodes[nodes.length - 1]!;
  const lastIndex = Number(last.dataset.index);
  if (!Number.isFinite(lastIndex)) return null;
  const rect = last.getBoundingClientRect();
  if (clientX >= rect.right) return lastIndex + 1;
  return lastIndex;
}

function endTabDrag(ev: PointerEvent): void {
  if (!tabDrag || tabDrag.pointerId !== ev.pointerId) return;
  const state = tabDrag;
  tabDrag = null;
  clearTabDragOver();
  if (!state.dragging) return;
  suppressNextTabClick = true;
  const fromTab = tabs[state.fromIndex];
  if (!fromTab) return;
  const insertBefore = tabIndexFromPoint(ev.clientX, ev.clientY, fromTab.pinned);
  if (insertBefore == null) return;
  reorderTab(state.fromIndex, insertBefore);
}

function onTabPointerMove(ev: PointerEvent): void {
  if (!tabDrag || tabDrag.pointerId !== ev.pointerId) return;
  const dx = ev.clientX - tabDrag.startX;
  const dy = ev.clientY - tabDrag.startY;
  if (!tabDrag.dragging) {
    if (Math.hypot(dx, dy) < TAB_DRAG_THRESHOLD_PX) return;
    tabDrag.dragging = true;
    const fromEl = document.querySelector(`.tab[data-index="${tabDrag.fromIndex}"]`);
    fromEl?.classList.add("dragging");
  }
  clearTabDragOver();
  const fromTab = tabs[tabDrag.fromIndex];
  if (!fromTab) return;
  const insertBefore = tabIndexFromPoint(ev.clientX, ev.clientY, fromTab.pinned);
  tabDrag.overIndex = insertBefore;
  if (insertBefore == null) return;
  const highlightIndex =
    insertBefore >= tabs.length ? tabs.length - 1 : insertBefore;
  const overEl = document.querySelector(`.tab[data-index="${highlightIndex}"]`);
  overEl?.classList.add("drag-over");
  const fromEl = document.querySelector(`.tab[data-index="${tabDrag.fromIndex}"]`);
  fromEl?.classList.add("dragging");
}

function onTabPointerUp(ev: PointerEvent): void {
  endTabDrag(ev);
}

function activateTabAt(index: number): void {
  if (index < 0 || index >= tabs.length) return;
  activeTabIndex = index;
  renderAll();
  persistLayout();
  syncExplorerToActiveContext();
  syncDocumentTitle();
  focusActiveTab();
}

function buildTabElement(tab: Tab, index: number): HTMLElement {
  const el = document.createElement("div");
  el.className = "tab";
  el.dataset.index = String(index);
  if (index === activeTabIndex) el.classList.add("active");
  if (tab.pinned) el.classList.add("pinned");
  if (tab.kind === "editor") {
    if (tab.preview) el.classList.add("preview");
    if (tab.dirty) el.classList.add("dirty");
  }
  el.setAttribute("role", "tab");
  el.setAttribute("aria-selected", index === activeTabIndex ? "true" : "false");

  const label = document.createElement("span");
  label.className = "tab-label";
  label.textContent = tabLabel(tab);
  if (tab.kind === "editor") label.title = tab.path;
  else if (tab.leaves.get(tab.activeLeafId)?.cwd) {
    label.title = tab.leaves.get(tab.activeLeafId)!.cwd!;
  }
  el.appendChild(label);

  const x = document.createElement("button");
  x.className = "tab-close";
  x.type = "button";
  x.title = "Close tab";
  x.setAttribute("aria-label", "Close tab");
  x.innerHTML =
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18 6 6 18"/><path d="m6 6 12 12"/></svg>';
  x.addEventListener("click", (ev) => {
    ev.stopPropagation();
    closeTabAt(index);
  });
  el.appendChild(x);

  el.addEventListener("click", () => {
    if (suppressNextTabClick) {
      suppressNextTabClick = false;
      return;
    }
    activateTabAt(index);
  });

  el.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0) return;
    if ((ev.target as HTMLElement | null)?.closest?.(".tab-close")) return;
    tabDrag = {
      fromIndex: index,
      startX: ev.clientX,
      startY: ev.clientY,
      dragging: false,
      pointerId: ev.pointerId,
      overIndex: null,
    };
  });

  el.addEventListener("contextmenu", (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    activateTabAt(index);

    const items: ContextMenuItem[] = [];
    items.push({
      kind: "item",
      label: tab.pinned ? "Unpin Tab" : "Pin Tab",
      run: () => setTabPinned(tab, !tab.pinned),
    });
    items.push({ kind: "separator" });
    if (tab.kind === "editor") {
      items.push(...pathContextItems(tab.path, { kind: "file", includeFileOps: false }));
      items.push({ kind: "separator" });
    } else {
      const cwd = tab.leaves.get(tab.activeLeafId)?.cwd;
      if (cwd) {
        items.push(...pathContextItems(cwd, { kind: "dir", includeFileOps: false }));
        items.push({ kind: "separator" });
      }
    }
    items.push({
      kind: "item",
      label: "Close",
      run: () => closeTabAt(tabs.indexOf(tab) >= 0 ? tabs.indexOf(tab) : index),
    });
    openContextMenu(ev.clientX, ev.clientY, items);
  });

  return el;
}

function renderTabs(): void {
  const unpinnedPill = tabPill;
  const pinnedPill = pinnedTabPill;
  tabsEl.innerHTML = "";
  tabsEl.appendChild(unpinnedPill);
  pinnedTabsEl.innerHTML = "";
  pinnedTabsEl.appendChild(pinnedPill);

  const pinned = tabs.filter((t) => t.pinned);
  const unpinned = tabs.filter((t) => !t.pinned);
  pinnedTabsEl.hidden = pinned.length === 0;
  pinnedTabsSep.hidden = pinned.length === 0;

  pinned.forEach((tab) => {
    const index = tabs.indexOf(tab);
    pinnedTabsEl.appendChild(buildTabElement(tab, index));
  });
  unpinned.forEach((tab) => {
    const index = tabs.indexOf(tab);
    tabsEl.appendChild(buildTabElement(tab, index));
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
    if (tabs[activeTabIndex] === tab) syncDocumentTitle();
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
  syncDocumentTitle();
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
  } else if (active.mdView === "preview" && active.mdWysiwyg) {
    active.mdWysiwyg.focus();
  } else if (active.mdView === "preview" && active.mdPreviewEl) {
    active.mdPreviewEl.focus();
  } else {
    active.view.focus();
  }
}

function syncWysiwygToCodeMirror(tab: EditorTab): void {
  if (!tab.mdWysiwyg) return;
  const md = tab.mdWysiwyg.flush();
  tab.suppressChange = true;
  setEditorDocument(tab.view, md);
  tab.suppressChange = false;
}

function applyEditorMdView(tab: EditorTab, opts: { refresh?: boolean } = {}): void {
  const showPreview = tab.mdView === "preview" && !!tab.mdPreviewEl;
  tab.host.classList.toggle("md-preview-active", showPreview);
  if (showPreview && tab.mdPreviewEl) {
    if (!tab.mdWysiwyg) {
      tab.mdWysiwyg = mountMarkdownWysiwyg(tab.mdPreviewEl, {
        onDirty: () => {
          if (tab.dirty) {
            tab.preview = false;
            return;
          }
          tab.dirty = true;
          tab.preview = false;
          renderTabs();
          updateStatusRight();
        },
      });
    }
    if (opts.refresh) {
      tab.mdWysiwyg.refresh(tab.view.state.doc.toString());
    }
  } else if (tab.mdWysiwyg) {
    syncWysiwygToCodeMirror(tab);
  }
}

function toggleMarkdownPreview(): void {
  const active = tabs[activeTabIndex];
  if (active?.kind !== "editor" || !isMarkdownPath(active.path) || !active.mdPreviewEl) return;
  if (active.mdView === "preview" && active.mdWysiwyg) {
    syncWysiwygToCodeMirror(active);
  }
  active.mdView = active.mdView === "preview" ? "source" : "preview";
  applyEditorMdView(active, { refresh: active.mdView === "preview" });
  focusActiveTab();
}

function renderAll(): void {
  renderTabs();
  renderPanes();
  updateStacks();
  updateStatusRight();
  syncSearchTarget();
  syncDocumentTitle();
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
let tree: VirtualTree;

function clearTree(): void {
  tree.clear();
  pendingFs.clear();
  pendingFsAuth.clear();
  pendingFsMutate.clear();
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
  // Recursive on the sandbox root; the backend skips heavyweight trees
  // (.git/target/node_modules/…) when installing watches so large workspaces
  // do not stall the PTY WebSocket during shell startup.
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

async function openEditorFromLink(
  lineText: string,
  column: number,
  opts: { preview?: boolean; cwd?: string } = {},
): Promise<void> {
  if (!hasEditor) {
    setStatusLeft("backend has no editor capability");
    return;
  }
  setStatusLeft("opening path link…");
  try {
    const opened = await editorOpenLink(lineText, column, opts);
    await presentOpenedBuffer(opened, { preview: !!opts.preview });
  } catch (err) {
    setStatusLeft(`editor error: ${err instanceof Error ? err.message : String(err)}`);
  }
}

async function presentOpenedBuffer(
  opened: OpenedInfo & { rev: number; text: string },
  opts: OpenEditorOpts = {},
): Promise<void> {
  const preview = !!opts.preview;
  const existingIdx = tabs.findIndex(
    (t) => t.kind === "editor" && (t.path === opened.path || pathsEqual(t.path, opened.path)),
  );
  if (existingIdx >= 0) {
    const et = tabs[existingIdx] as EditorTab;
    if (!preview) et.preview = false;
    activeTabIndex = existingIdx;
    renderAll();
    focusActiveTab();
    const jumpLine = opened.line ?? opts.line;
    const jumpCol = opened.column ?? opts.column;
    if (jumpLine != null) revealEditorLocation(et.view, jumpLine, jumpCol);
    setStatusLeft(`opened ${opened.path}`);
    return;
  }

  if (preview) {
    const previewIdx = tabs.findIndex((t) => t.kind === "editor" && t.preview && !t.dirty);
    if (previewIdx >= 0) closeTabAt(previewIdx, { force: true });
  }

  const host = document.createElement("div");
  host.className = "editor-host";
  editorStack.appendChild(host);

  let tabRef!: EditorTab;
  const view = createEditorView(
    host,
    opened.text,
    opened.path,
    () => {
      if (tabRef.suppressChange) return;
      tabRef.dirty = true;
      tabRef.preview = false;
      renderTabs();
      updateStatusRight();
    },
    {
      fontSize: uiSettings.editorFontSize,
      fontWeight: uiSettings.monoFontWeight,
      theme: getResolvedTheme(),
      language: opened.language,
      minimap: uiSettings.editorMinimap,
      lineWrap: uiSettings.editorLineWrap,
      onPathLink: (info) => {
        void openEditorTab(info.path, {
          preview: true,
          cwd: lastTerminalCwd || undefined,
          line: info.line,
          column: info.column,
        });
      },
    },
  );

  let mdPreviewEl: HTMLElement | null = null;
  if (isMarkdownPath(opened.path)) {
    mdPreviewEl = document.createElement("div");
    mdPreviewEl.className = "md-preview";
    mdPreviewEl.tabIndex = -1;
    mdPreviewEl.setAttribute("role", "document");
    mdPreviewEl.setAttribute("aria-label", "Markdown preview");
    host.appendChild(mdPreviewEl);
  }

  tabRef = {
    kind: "editor",
    id: `editor-${opened.buffer_id}`,
    bufferId: opened.buffer_id,
    path: opened.path,
    rev: opened.rev,
    dirty: false,
    preview,
    mdView: "source",
    view,
    host,
    mdPreviewEl,
    mdWysiwyg: null,
    suppressChange: false,
    pinned: !!opts.pinned,
  };

  insertTab(tabRef);
  if (!opts.silent) {
    activeTabIndex = tabs.indexOf(tabRef);
  }
  if (configPath && pathsEqual(opened.path, configPath)) {
    configPath = opened.path;
  }
  if (!opts.silent) {
    renderAll();
    persistLayout();
    focusActiveTab();
  }
  const jumpLine = opened.line ?? opts.line;
  const jumpCol = opened.column ?? opts.column;
  if (jumpLine != null) revealEditorLocation(view, jumpLine, jumpCol);
  if (!opts.silent) setStatusLeft(`opened ${opened.path}`);
}

async function openEditorTab(path: string, opts: boolean | OpenEditorOpts = {}): Promise<void> {
  const options: OpenEditorOpts = typeof opts === "boolean" ? { preview: opts } : opts;
  const preview = !!options.preview;

  if (!hasEditor) {
    setStatusLeft("backend has no editor capability");
    return;
  }

  const existingIdx = tabs.findIndex(
    (t) => t.kind === "editor" && (t.path === path || pathsEqual(t.path, path)),
  );
  if (existingIdx >= 0) {
    const et = tabs[existingIdx] as EditorTab;
    if (!preview) et.preview = false;
    activeTabIndex = existingIdx;
    renderAll();
    focusActiveTab();
    if (options.line != null) revealEditorLocation(et.view, options.line, options.column);
    return;
  }

  setStatusLeft(`opening ${path}…`);
  try {
    const opened = await editorOpen(path, options);
    await presentOpenedBuffer(opened, options);
  } catch (err) {
    setStatusLeft(`editor error: ${err instanceof Error ? err.message : String(err)}`);
  }
}

async function saveActiveEditor(): Promise<void> {
  const active = tabs[activeTabIndex];
  if (active?.kind !== "editor" || !active.dirty) return;
  if (active.mdView === "preview" && active.mdWysiwyg) {
    syncWysiwygToCodeMirror(active);
  }
  const text = active.view.state.doc.toString();
  setStatusLeft(`saving ${active.path}…`);
  try {
    const revAfterEdit = await bufferEdit(active.bufferId, active.rev, text);
    active.rev = revAfterEdit;
    const saved = await bufferSave(active.bufferId, active.rev);
    active.rev = saved.rev;
    active.dirty = false;
    active.preview = false;
    if (active.mdView === "preview") applyEditorMdView(active, { refresh: true });
    renderAll();
    if (isConfigPath(saved.path) || isConfigPath(active.path)) {
      try {
        applyUiSettings(uiSettingsFromConfigText(text));
        setActiveShortkeys(shortkeysFromConfigText(text));
        setStatusLeft(
          `saved settings · ${paletteLabel(uiSettings.palette)} · theme ${uiSettings.theme}`,
        );
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

/** Restore one multi-pane terminal tab from a planned layout entry. */
function restoreTerminalTabFromPlan(plan: PlannedTerminalRestore): void {
  const leaves = new Map<string, TermBundle>();
  const cwds = leafCwdsFromTree(plan.paneTree);
  for (const id of plan.leafIds) {
    const bundle = makeTerminal();
    const cwd = cwds.get(id);
    if (cwd) {
      bundle.cwd = cwd;
      lastTerminalCwd = cwd;
    }
    wireTerminalLeaf(id, bundle);
    leaves.set(id, bundle);
  }
  const tab: TerminalTab = {
    kind: "terminal",
    id: plan.id || nextTabId(),
    title: plan.title || titleFromCwd(cwds.get(plan.activeLeafId), "sh"),
    paneTree: plan.paneTree,
    leaves,
    activeLeafId: plan.activeLeafId,
    pinned: plan.pinned,
  };
  insertTab(tab);
}

/**
 * Rebuild tabs after `session_attach` from the Rust-owned layout blob
 * (localStorage is only a fallback when the session has no layout yet).
 */
async function restoreSessionFromBlob(blob: LayoutBlob, ptyList: PtyInfo[]): Promise<boolean> {
  const plan = planSessionRestore(
    blob,
    ptyList.map((p) => p.id),
    { restoreEditors: hasEditor },
  );
  if (plan.items.length === 0) return false;

  restoringSession = true;
  try {
    for (const item of plan.items) {
      if (item.kind === "terminal") {
        restoreTerminalTabFromPlan(item);
      } else if (item.kind === "orphan") {
        const bundle = makeTerminal();
        createTerminalTab(item.ptyId, bundle, { silentActivate: true });
      } else if (item.kind === "editor") {
        try {
          await openEditorTab(item.path, {
            preview: item.preview,
            pinned: item.pinned,
            silent: true,
          });
        } catch {
          /* path may be gone; skip */
        }
      }
    }
  } finally {
    restoringSession = false;
  }

  if (tabs.length === 0) return false;

  normalizeTabOrder();
  activeTabIndex = Math.min(Math.max(plan.activeTab, 0), tabs.length - 1);
  renderAll();
  persistLayout();
  requestAnimationFrame(() => {
    fitActiveLeaves();
    focusActiveTab();
  });
  return true;
}

function afterSessionReady(): void {
  connected = true;
  setWorkspaceControls(true);
  updateStatusRight();
  loadRoot()
    .then(() => {
      syncExplorerToActiveContext();
      startFsWatch();
    })
    .catch(() => {
      syncExplorerToActiveContext();
      startFsWatch();
    });
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
      setActiveShortkeys(msg.shortkeys ?? null);
      send({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        role: "client",
        implementation: "fresh-gui-ui/0.5",
        capabilities: ["ping", "pty", "fs", "session", "editor", "scene"],
      });
      {
        if (authToken) send({ type: "auth", token: authToken });
        else beginSession();
      }
      setStatusLeft(`hello from ${msg.implementation}${hasEditor ? " · editor" : " · no editor"}`);
      updateStatusRight();
      break;
    case "auth_ok": {
      if (authToken) cacheAuthToken(authToken);
      beginSession();
      setStatusLeft("authenticated");
      break;
    }
    case "auth_error":
      clearCachedAuthToken();
      authToken = "";
      setStatusLeft(`auth failed: ${msg.message}`);
      break;
    case "session_created":
      sessionId = msg.session_id;
      preferredSessionId = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      afterSessionReady();
      requestNewPty(80, 24, { kind: "newTab" });
      setStatusLeft(`session ${sessionId}`);
      updateStatusRight();
      break;
    case "session_attached": {
      sessionId = msg.session_id;
      preferredSessionId = sessionId;
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
      loadExplorerSnapshotsFromBlob(blob);

      const ptyList = msg.ptys || [];
      void (async () => {
        const restored = await restoreSessionFromBlob(blob, ptyList);
        if (!restored) {
          restoringSession = true;
          try {
            for (const p of ptyList) {
              const bundle = makeTerminal();
              createTerminalTab(p.id, bundle, { silentActivate: true });
            }
          } finally {
            restoringSession = false;
          }
        }

        if (terminalTabs().length === 0 && tabs.every((t) => t.kind !== "terminal")) {
          // Prefer at least one terminal when the session had only editors or empty layout.
          if (tabs.length === 0) {
            requestNewPty(80, 24, { kind: "newTab" });
          } else {
            renderAll();
            persistLayout();
          }
        } else if (!restored) {
          activeTabIndex = Math.min(Math.max(blob.activeTab ?? 0, 0), Math.max(0, tabs.length - 1));
          renderAll();
          persistLayout();
          syncExplorerToActiveContext();
        }
        afterSessionReady();
        setStatusLeft(`reattached ${sessionId} (${ptyList.length} ptys)`);
        updateStatusRight();
      })();
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
    case "fs_created": {
      const pending = pendingFsMutate.get(msg.request_id);
      if (pending) {
        pendingFsMutate.delete(msg.request_id);
        pending.resolve([msg.entry]);
      }
      break;
    }
    case "fs_copied":
    case "fs_moved": {
      const pending = pendingFsMutate.get(msg.request_id);
      if (pending) {
        pendingFsMutate.delete(msg.request_id);
        pending.resolve(msg.entries || []);
      }
      break;
    }
    case "fs_deleted": {
      const pending = pendingFsDelete.get(msg.request_id);
      if (pending) {
        pendingFsDelete.delete(msg.request_id);
        pending.resolve(msg.paths || []);
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
          line: msg.line,
          column: msg.column,
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
        preferredSessionId = "";
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
  const wanted = preferredSessionId.trim() || localStorage.getItem(SESSION_KEY) || "";
  if (wanted) {
    preferredSessionId = wanted;
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
  pendingEditor.clear();
  pendingEdit.clear();
  pendingSave.clear();
  clearTabs();
  clearTree();
  lastTerminalCwd = null;
  explorerSyncGen += 1;
  setTreeEmptyHint(true);
  syncDocumentTitle();
  setWorkspaceControls(false);
  updateStatusRight();
  setStatusLeft("disconnected (session kept on backend if created)");
}

function connect(): void {
  disconnect();
  const url = wsUrl.trim();
  if (!url) {
    setStatusLeft("no backend URL");
    return;
  }
  setStatusLeft(`connecting ${url}…`);
  updateStatusRight();
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    setStatusLeft("socket open, waiting for hello…");
    updateStatusRight();
  });
  ws.addEventListener("message", (ev) => {
    if (typeof ev.data === "string") onMessage(ev.data);
  });
  ws.addEventListener("close", () => {
    connected = false;
    sessionId = null;
    setWorkspaceControls(false);
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

function defaultCreateParent(): string {
  const active = tabs[activeTabIndex];
  if (active?.kind === "editor") {
    return dirname(active.path) || pathAnchorRoot();
  }
  if (active?.kind === "terminal") {
    const cwd = active.leaves.get(active.activeLeafId)?.cwd;
    if (cwd) return cwd;
  }
  if (lastTerminalCwd) return lastTerminalCwd;
  return pathAnchorRoot() || tree.getRootPath() || "";
}

function openNewFileEditor(): void {
  const parent = defaultCreateParent();
  if (!parent) {
    setStatusLeft("no workspace to create a file in");
    return;
  }
  void runCreate(parent, "file");
}

function sendPtyInput(ptyId: string, data: string): void {
  if (!ws || ws.readyState !== WebSocket.OPEN) return;
  send({ type: "pty_data", id: ptyId, data: b64encode(data) });
}

function goExplorerParent(): void {
  const active = tabs[activeTabIndex];
  if (active?.kind === "terminal") {
    const bundle = active.leaves.get(active.activeLeafId);
    if (bundle) {
      sendPtyInput(active.activeLeafId, "cd ..\r");
      const fromCwd = bundle.cwd || tree.getRootPath();
      const parent = explorerParentPath(fromCwd);
      if (parent) syncExplorerToCwd(parent);
      else {
        const viewParent = explorerParentPath(tree.getRootPath());
        if (viewParent) syncExplorerToCwd(viewParent);
      }
      bundle.term.focus();
      return;
    }
  }

  const parent = explorerParentPath(tree.getRootPath());
  if (!parent) {
    setStatusLeft("already at top");
    return;
  }
  syncExplorerToCwd(parent);
}

function tabActionsMenuItems(): ContextMenuItem[] {
  const active = tabs[activeTabIndex];
  const items: ContextMenuItem[] = [
    {
      kind: "item",
      label: "Find",
      hint: "Mod+F",
      run: () => focusFind(),
    },
  ];
  if (!active) return items;

  if (active.kind === "terminal") {
    items.push(
      { kind: "separator" },
      {
        kind: "item",
        label: "Split Right",
        hint: "Mod+D",
        disabled: !connected,
        run: () => requestSplit("horizontal"),
      },
      {
        kind: "item",
        label: "Split Down",
        hint: "Mod+Shift+D",
        disabled: !connected,
        run: () => requestSplit("vertical"),
      },
      {
        kind: "item",
        label: "Keep Only Active Pane",
        disabled: !connected || leafCount(active.paneTree) <= 1,
        run: () => collapseToActiveLeaf(),
      },
    );
  } else {
    items.push(
      { kind: "separator" },
      {
        kind: "item",
        label: "Save",
        hint: "Mod+S",
        disabled: !active.dirty,
        run: () => {
          void saveActiveEditor();
        },
      },
    );
    if (isMarkdownPath(active.path) && active.mdPreviewEl) {
      const inPreview = active.mdView === "preview";
      items.push({
        kind: "item",
        label: inPreview ? "Show Source" : "Markdown WYSIWYG",
        hint: "Mod+Shift+V",
        run: () => toggleMarkdownPreview(),
      });
    }
    items.push({
      kind: "item",
      label: "Toggle Line Wrap",
      hint: "Alt+Z",
      run: () => {
        void toggleEditorLineWrap();
      },
    });
  }

  items.push(
    { kind: "separator" },
    {
      kind: "item",
      label: active.pinned ? "Unpin Tab" : "Pin Tab",
      run: () => setTabPinned(active, !active.pinned),
    },
    { kind: "separator" },
    {
      kind: "item",
      label: "Close Tab",
      hint: "Mod+W",
      run: () => closeActiveTabOrLeaf(),
    },
    {
      kind: "item",
      label: "Close Other Tabs",
      disabled: tabs.length <= 1,
      run: () => closeOtherTabs(),
    },
    {
      kind: "item",
      label: "Close All Editors",
      disabled: !tabs.some((t) => t.kind === "editor"),
      run: () => closeAllEditors(),
    },
    {
      kind: "item",
      label: "Close All Terminals",
      disabled: !tabs.some((t) => t.kind === "terminal"),
      run: () => closeAllTerminals(),
    },
    {
      kind: "item",
      label: "Close Other Terminals",
      disabled: !tabs.some((t) => t.kind === "terminal" && t !== active),
      run: () => closeOtherTerminals(),
    },
  );
  return items;
}

function newTabMenuItems(): ContextMenuItem[] {
  return [
    {
      kind: "item",
      label: "New Terminal",
      hint: "Mod+T",
      disabled: !connected,
      run: () => openNewTerminalTab(),
    },
    {
      kind: "item",
      label: "New File…",
      disabled: !connected,
      run: () => openNewFileEditor(),
    },
  ];
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


let bootstrapped = false;

/** Bind the React-rendered shell and start the ADE controller (once). */
export function bootstrapAde(): void {
  if (bootstrapped) return;
  bootstrapped = true;

  workspaceEl = $("workspace");
  sidebarToggle = $button("sidebar-toggle");
  sidebarResizer = $("sidebar-resizer");
  treeEl = $("tree");
  tabsEl = $("tabs");
  tabPill = $("tab-pill");
  pinnedTabsEl = $("pinned-tabs");
  pinnedTabPill = $("pinned-tab-pill");
  pinnedTabsSep = $("pinned-tabs-sep");
  panesEl = $("panes");
  emptyStack = $("empty-stack");
  terminalStack = $("terminal-stack");
  editorStack = $("editor-stack");
  statusLeft = $("status-left");
  statusRight = $("status-right");
  tabsMenuBtn = $button("tabs-menu");
  newTabBtn = $button("new-tab");
  sidebarParentBtn = $button("sidebar-parent");
  activityExplorer = $button("activity-explorer");
  activityPalette = $button("activity-palette");
  activitySettings = $button("activity-settings");

  initTheme();
  uiSettings = loadSettings();
  applyUiChrome(uiSettings);

  terminalPark = document.createElement("div");
  terminalPark.className = "terminal-park";
  terminalPark.style.cssText =
    "position:absolute;visibility:hidden;pointer-events:none;width:0;height:0;overflow:hidden";
  terminalStack.appendChild(terminalPark);

  tree = new VirtualTree(treeEl, fsList, {
    onOpenFile: (entry, preview) => {
      void openEditorTab(entry.path, { preview });
    },
    onStatus: (text) => setStatusLeft(text),
    noteInteraction: () => noteTreeInteraction(),
    onRootChange: (rootPath) => updateExplorerTitle(rootPath),
    onContextMenu: (entry, x, y) => openPathContextMenu(entry.path, x, y, entry.kind),
    onExplorerUiChange: () => scheduleExplorerPersist(),
  });
  tree.setVisibility({
    showDotfiles: uiSettings.showDotfiles,
    showGitDirs: uiSettings.showGitDirs,
  });
  setTreeEmptyHint(true);
  updateParentButton();

tabsMenuBtn.addEventListener("click", (ev) => {
  ev.preventDefault();
  ev.stopPropagation();
  openContextMenuForAnchor(tabsMenuBtn, tabActionsMenuItems(), { align: "start" });
});
newTabBtn.addEventListener("click", (ev) => {
  ev.preventDefault();
  ev.stopPropagation();
  openContextMenuForAnchor(newTabBtn, newTabMenuItems(), { align: "end" });
});
sidebarParentBtn.addEventListener("click", () => goExplorerParent());
sidebarToggle.addEventListener("click", toggleSidebar);
activityExplorer.addEventListener("click", () => {
  toggleSidebar();
  if (!isSidebarCollapsed()) treeEl.focus();
});
activityPalette.addEventListener("click", () => {
  openPaletteWithQuery("Color Palette");
});
activitySettings.addEventListener("click", () => {
  void openSettingsFile();
});

tabsEl.addEventListener("scroll", () => requestAnimationFrame(measurePill));
pinnedTabsEl.addEventListener("scroll", () => requestAnimationFrame(measurePill));
window.addEventListener("pointermove", onTabPointerMove);
window.addEventListener("pointerup", onTabPointerUp);
window.addEventListener("pointercancel", onTabPointerUp);

const shortcutHandlers: ShortcutHandlers = {
  "gotoFile.open": () => openGotoFile(),
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
  "editor.markdownPreview": () => {
    toggleMarkdownPreview();
  },
  "editor.toggleLineWrap": () => {
    void toggleEditorLineWrap();
  },
  "search.focus": () => focusFind(),
  "settings.open": () => {
    void openSettingsFile();
  },
  "settings.openDefaults": () => {
    void openDefaultSettingsFile();
  },
};

function runShortcutId(id: ShortcutId): void {
  shortcutHandlers[id]?.(new KeyboardEvent("keydown"));
}

function colorPaletteCommands(): PaletteCommand[] {
  const current = uiSettings.palette;
  return listPalettes().map(({ id, label }) => ({
    id: `palette.${id}`,
    label: id === current ? `Color Palette: ${label} (current)` : `Color Palette: ${label}`,
    run: () => {
      void choosePalette(id);
    },
  }));
}

refreshPaletteCommands = () => {
  setPaletteCommands([
    ...defaultPaletteCommands(runShortcutId),
    {
      id: "connection.reconnect",
      label: "Connection: Reconnect",
      run: () => connect(),
    },
    {
      id: "connection.disconnect",
      label: "Connection: Disconnect",
      run: () => disconnect(),
    },
    {
      id: "palette.pick",
      label: "Preferences: Color Palette…",
      run: () => openPaletteWithQuery("Color Palette"),
    },
    {
      id: "settings.openDefaults.palette",
      label: "Preferences: Open Default Settings",
      run: () => {
        void openDefaultSettingsFile();
      },
    },
    ...colorPaletteCommands(),
    {
      id: "tabs.pinActive",
      label: "Tabs: Pin / Unpin Active",
      run: () => toggleActiveTabPinned(),
    },
    {
      id: "tabs.closeOther",
      label: "Tabs: Close Other Tabs",
      run: () => closeOtherTabs(),
    },
    {
      id: "tabs.closeAllEditors",
      label: "Tabs: Close All Editors",
      run: () => closeAllEditors(),
    },
    {
      id: "tabs.closeAllTerminals",
      label: "Tabs: Close All Terminals",
      run: () => closeAllTerminals(),
    },
    {
      id: "tabs.closeOtherTerminals",
      label: "Tabs: Close Other Terminals",
      run: () => closeOtherTerminals(),
    },
  ]);
};

installShortcuts(shortcutHandlers, {
  getContext: () => {
    const active = tabs[activeTabIndex];
    const surface =
      active?.kind === "terminal" ? "terminal" : active?.kind === "editor" ? "editor" : "none";
    const fileExplorerFocused = !!(
      treeEl &&
      document.activeElement &&
      treeEl.contains(document.activeElement)
    );
    return { surface, fileExplorerFocused };
  },
});
refreshPaletteCommands();
setGotoFileHandler((path) => {
  void openEditorTab(path, {
    preview: false,
    cwd: lastTerminalCwd || undefined,
  });
});
// OS theme flips while preference is "system" → restyle open panes (primer only).
onResolvedThemeChange((resolved) => {
  if (uiSettings.palette !== "primer") {
    // Named Fresh packs pin appearance; re-assert after applyTheme notifications.
    applyPalette(uiSettings.palette);
    restyleOpenPanes(getResolvedTheme(), true);
    requestAnimationFrame(fitActiveLeaves);
    return;
  }
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
updateStatusRight();
setStatusLeft("disconnected");
syncDocumentTitle();

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

wsUrl = defaultWsUrl();
preferredSessionId = localStorage.getItem(SESSION_KEY) || "";

const tokenFromUrl = consumeTokenQueryParam();
authToken = tokenFromUrl || loadCachedAuthToken() || "";

// Auto-connect from a fresh `?token=` link, or after reload when the token
// was cached (URL is stripped after first use).
const shouldAutoConnect = !!authToken;

setupSidebarResizer();
renderAll();

if (shouldAutoConnect) {
  connect();
}

}
