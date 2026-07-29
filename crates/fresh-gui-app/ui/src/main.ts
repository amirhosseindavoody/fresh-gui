/* Phase 3b/3c host UI: sessions, tabs/splits, tree+watch, CodeMirror edit/save, thin scene. */
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import CodeMirror from "codemirror";
import type { Editor } from "codemirror";
import "@xterm/xterm/css/xterm.css";
import "codemirror/lib/codemirror.css";
import "codemirror/theme/material-darker.css";
import "codemirror/mode/rust/rust.js";
import "codemirror/mode/javascript/javascript.js";
import "codemirror/mode/python/python.js";
import "codemirror/mode/markdown/markdown.js";
import "./styles.css";
import {
  PROTOCOL_VERSION,
  type ClientMessage,
  type FsEntry,
  type ServerMessage,
} from "./protocol";

const SESSION_KEY = "fresh-gui.sessionId";
const LAYOUT_KEY = "fresh-gui.layout";

function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}
function $input(id: string): HTMLInputElement {
  const el = $(id);
  if (!(el instanceof HTMLInputElement)) throw new Error(`#${id} is not an input`);
  return el;
}
function $button(id: string): HTMLButtonElement {
  const el = $(id);
  if (!(el instanceof HTMLButtonElement)) throw new Error(`#${id} is not a button`);
  return el;
}

const statusEl = $("status");
const connectBtn = $button("connect");
const disconnectBtn = $button("disconnect");
const treeEl = $("tree");
const tabsEl = $("tabs");
const panesEl = $("panes");
const editorPanel = $("editor-panel");
const editorHost = $("editor-host");
const editorPathEl = $("editor-path");
const editorSaveBtn = $button("editor-save");

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

interface Tab {
  id: string;
  title: string;
  term: Terminal;
  fit: FitAddon;
  el: HTMLElement;
}

type SplitMode = "horizontal" | "vertical";

let ws: WebSocket | null = null;
let sessionId: string | null = null;
let reqSeq = 0;
let hasEditor = false;
const pendingFs = new Map<string, Pending<{ path: string; entries: FsEntry[] }>>();
const pendingEditor = new Map<string, PendingEditor>();
const pendingEdit = new Map<string, Pending<number>>();
const pendingSave = new Map<string, Pending<{ path: string; rev: number }>>();
let selectedPath: string | null = null;
let openBufferId: string | null = null;
let openBufferRev = 0;
let openBufferPath: string | null = null;
let editorDirty = false;
let suppressCmChange = false;
let cm: Editor | null = null;
let watchId: string | null = null;
let treeRefreshTimer: ReturnType<typeof setTimeout> | null = null;
let treeLoading = false;
let treeLoaded = false;
let treeNeedsRefresh = false;
const expandedPaths = new Set<string>();
/** Suppress auto-refresh briefly after the user clicks the tree. */
let treeQuietUntil = 0;

/** Paths under these directory names are ignored for tree auto-refresh. */
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
let activeTab = 0;
let splitMode: SplitMode | null = null;
/** second pane tab index when split */
let splitTab = 1;
/** Apply this split once a newly opened PTY arrives (when splitting with <2 tabs). */
let pendingSplit: SplitMode | null = null;

function setStatus(text: string): void {
  statusEl.textContent = text;
  statusEl.title = text;
}

function b64encode(str: string): string {
  const bytes = new TextEncoder().encode(str);
  let s = "";
  bytes.forEach((b) => {
    s += String.fromCharCode(b);
  });
  return btoa(s);
}

function b64decode(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
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

function modeForPath(path: string | null | undefined): string | null {
  const lower = (path || "").toLowerCase();
  if (lower.endsWith(".rs")) return "rust";
  if (lower.endsWith(".js") || lower.endsWith(".ts") || lower.endsWith(".mjs")) return "javascript";
  if (lower.endsWith(".py")) return "python";
  if (lower.endsWith(".md")) return "markdown";
  return null;
}

function ensureCodeMirror(): Editor {
  if (cm) return cm;
  cm = CodeMirror(editorHost, {
    value: "",
    theme: "material-darker",
    lineNumbers: true,
    indentUnit: 4,
    lineWrapping: true,
  });
  cm.on("change", () => {
    if (suppressCmChange || !openBufferId) return;
    editorDirty = true;
    updateEditorChrome();
  });
  return cm;
}

function updateEditorChrome(): void {
  const name = openBufferPath || "untitled";
  editorPathEl.textContent = editorDirty ? `${name} •` : name;
  editorPathEl.classList.toggle("dirty", editorDirty);
  editorSaveBtn.disabled = !openBufferId || !editorDirty;
}

function showEditor(path: string, text: string, bufferId: string, rev: number): void {
  openBufferId = bufferId;
  openBufferRev = rev || 0;
  openBufferPath = path || "untitled";
  editorDirty = false;
  const editor = ensureCodeMirror();
  suppressCmChange = true;
  editor.setValue(text ?? "");
  editor.setOption("mode", modeForPath(path));
  suppressCmChange = false;
  editorPanel.classList.add("visible");
  updateEditorChrome();
  requestAnimationFrame(() => {
    editor.refresh();
    for (const tab of tabs) {
      try {
        tab.fit.fit();
      } catch {
        /* ignore */
      }
    }
  });
}

function hideEditor(): void {
  if (openBufferId) send({ type: "editor_close", buffer_id: openBufferId });
  openBufferId = null;
  openBufferRev = 0;
  openBufferPath = null;
  editorDirty = false;
  if (cm) {
    suppressCmChange = true;
    cm.setValue("");
    suppressCmChange = false;
  }
  editorPanel.classList.remove("visible");
  updateEditorChrome();
  editorPathEl.textContent = "No file open";
  requestAnimationFrame(() => {
    for (const tab of tabs) {
      try {
        tab.fit.fit();
      } catch {
        /* ignore */
      }
    }
  });
}

async function saveOpenBuffer(): Promise<void> {
  if (!openBufferId || !cm || !editorDirty) return;
  const text = cm.getValue();
  setStatus(`saving ${openBufferPath}…`);
  try {
    const revAfterEdit = await bufferEdit(openBufferId, openBufferRev, text);
    openBufferRev = revAfterEdit;
    const saved = await bufferSave(openBufferId, openBufferRev);
    openBufferRev = saved.rev;
    editorDirty = false;
    updateEditorChrome();
    setStatus(`saved ${saved.path}`);
  } catch (err) {
    setStatus(`save error: ${err instanceof Error ? err.message : String(err)}`);
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
    // Retry after the quiet window so we don't drop real updates forever.
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

function escapeHtml(s: string): string {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function persistLayout(): void {
  const layout = {
    split: splitMode,
    activeTab,
    splitTab,
    tabs: tabs.map((t) => ({ id: t.id, title: t.title })),
  };
  const json = JSON.stringify(layout);
  localStorage.setItem(LAYOUT_KEY, json);
  if (sessionId) send({ type: "layout_set", layout: json });
}

function makeTerm(): { term: Terminal; fit: FitAddon } {
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
    fontSize: 14,
    theme: {
      background: "#010409",
      foreground: "#e6edf3",
      cursor: "#3fb950",
    },
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  return { term, fit };
}

function openPtyForDims(cols: number, rows: number): void {
  send({ type: "pty_open", cols, rows });
}

function createTab(ptyId: string, title?: string, opts: { silentActivate?: boolean } = {}): Tab {
  const { term, fit } = makeTerm();
  const host = document.createElement("div");
  host.className = "xterm-host";
  host.dataset.ptyId = ptyId;
  term.open(host);
  term.onData((data) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_data", id: ptyId, data: b64encode(data) });
  });
  term.onResize(({ cols, rows }) => {
    if (!ws || ws.readyState !== WebSocket.OPEN) return;
    send({ type: "pty_resize", id: ptyId, cols, rows });
  });
  const tab = {
    id: ptyId,
    title: title || `sh ${tabs.length + 1}`,
    term,
    fit,
    el: host,
  };
  tabs.push(tab);
  if (!opts.silentActivate) activeTab = tabs.length - 1;
  renderTabs();
  renderPanes();
  persistLayout();
  return tab;
}

function closeTab(index: number): void {
  const tab = tabs[index];
  if (!tab) return;
  send({ type: "pty_close", id: tab.id });
  try {
    tab.term.dispose();
  } catch {
        /* ignore */
      }
  tabs.splice(index, 1);
  if (activeTab >= tabs.length) activeTab = Math.max(0, tabs.length - 1);
  if (splitTab >= tabs.length) splitTab = Math.max(0, tabs.length - 1);
  if (tabs.length < 2) splitMode = null;
  renderTabs();
  renderPanes();
  persistLayout();
}

function renderTabs(): void {
  tabsEl.innerHTML = "";
  tabs.forEach((tab, i) => {
    const el = document.createElement("div");
    el.className = "tab" + (i === activeTab ? " active" : "");
    el.innerHTML = `<span>${escapeHtml(tab.title)}</span>`;
    const x = document.createElement("button");
    x.className = "x";
    x.type = "button";
    x.textContent = "×";
    x.addEventListener("click", (ev) => {
      ev.stopPropagation();
      closeTab(i);
    });
    el.appendChild(x);
    el.addEventListener("click", () => {
      activeTab = i;
      if (splitMode && splitTab === activeTab) {
        splitTab = (activeTab + 1) % tabs.length;
      }
      renderTabs();
      renderPanes();
      persistLayout();
      tab.term.focus();
    });
    tabsEl.appendChild(el);
  });
}

function renderPanes(): void {
  panesEl.innerHTML = "";
  panesEl.className =
    splitMode === "horizontal"
      ? "split-horizontal"
      : splitMode === "vertical"
        ? "split-vertical"
        : "no-split";

  const indices = splitMode && tabs.length >= 2 ? [activeTab, splitTab] : [activeTab];
  for (const idx of indices) {
    const tab = tabs[idx];
    if (!tab) continue;
    const pane = document.createElement("div");
    pane.className = "pane";
    const label = document.createElement("div");
    label.className = "pane-label";
    label.textContent = tab.title;
    pane.appendChild(label);
    pane.appendChild(tab.el);
    panesEl.appendChild(pane);
    requestAnimationFrame(() => {
      try {
        tab.fit.fit();
      } catch {
        /* ignore */
      }
    });
  }
}

function clearTerminals(): void {
  for (const tab of tabs) {
    try {
      tab.term.dispose();
    } catch {
        /* ignore */
      }
  }
  tabs = [];
  activeTab = 0;
  splitTab = 1;
  splitMode = null;
  tabsEl.innerHTML = "";
  panesEl.innerHTML = "";
  panesEl.className = "no-split";
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
    if (keepExpanded.size) {
      await restoreExpanded(children, keepExpanded);
    }
    if (!silent) {
      setStatus(`session ${sessionId || "?"} · ${listed.path}`);
    }
  } catch (err) {
    if (!silent || !treeLoaded) {
      treeEl.innerHTML = `<div style="padding:0.75rem;color:#f85149">${escapeHtml(String(err))}</div>`;
    }
  } finally {
    treeLoading = false;
    if (treeNeedsRefresh) {
      treeNeedsRefresh = false;
      scheduleTreeRefresh();
    }
  }
}

/** Re-expand directories that were open before a silent tree rebuild. */
async function restoreExpanded(container: HTMLElement, paths: Set<string>): Promise<void> {
  const rows = [...container.querySelectorAll(":scope > .tree-item")].filter(
    (el): el is HTMLElement => el instanceof HTMLElement,
  );
  for (const row of rows) {
    const childBox = row.nextElementSibling;
    if (!(childBox instanceof HTMLElement) || !childBox.classList.contains("tree-children")) {
      continue;
    }
    const path = row.dataset.path;
    if (!path || !paths.has(path)) continue;
    const twist = row.querySelector(".twist") as HTMLElement | null;
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

    row.addEventListener("click", async (ev) => {
      ev.stopPropagation();
      noteTreeInteraction();
      document.querySelectorAll(".tree-item.selected").forEach((el) => {
        el.classList.remove("selected");
      });
      row.classList.add("selected");
      selectedPath = entry.path;
      setStatus(`${entry.kind}: ${entry.path}`);
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
            setStatus(`fs error: ${err instanceof Error ? err.message : String(err)}`);
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
      }
    });
    if (entry.kind === "file") {
      row.addEventListener("dblclick", async (ev) => {
        ev.stopPropagation();
        noteTreeInteraction();
        if (!hasEditor) {
          setStatus("backend has no editor capability");
          return;
        }
        setStatus(`opening ${entry.path}…`);
        try {
          const opened = await editorOpen(entry.path, false);
          showEditor(opened.path, opened.text, opened.buffer_id, opened.rev);
          setStatus(`opened ${opened.path}`);
        } catch (err) {
          setStatus(`editor error: ${err instanceof Error ? err.message : String(err)}`);
        }
      });
    }
    container.append(row, childBox);
  }
}

function afterSessionReady(): void {
  $button("new-tab").disabled = false;
  $button("split-h").disabled = false;
  $button("split-v").disabled = false;
  $button("split-off").disabled = false;
  loadRoot().then(() => startFsWatch()).catch(() => startFsWatch());
}

function onMessage(raw: string): void {
  let msg: ServerMessage;
  try {
    msg = JSON.parse(raw) as ServerMessage;
  } catch {
    setStatus("bad json from backend");
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
        else {
          beginSession();
        }
      }
      setStatus(
        `hello from ${msg.implementation}${hasEditor ? " · editor" : " · no editor"}`,
      );
      break;
    case "auth_ok":
      beginSession();
      setStatus("authenticated");
      break;
    case "auth_error":
      setStatus(`auth failed: ${msg.message}`);
      break;
    case "session_created":
      sessionId = msg.session_id;
      $input("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      afterSessionReady();
      openPtyForDims(80, 24);
      setStatus(`session ${sessionId}`);
      break;
    case "session_attached":
      sessionId = msg.session_id;
      $input("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      clearTerminals();
      if (typeof msg.layout === "string" && msg.layout) {
        try {
          const layout = JSON.parse(msg.layout) as {
            split?: SplitMode | null;
            activeTab?: number;
            splitTab?: number;
          };
          splitMode = layout.split || null;
          activeTab = layout.activeTab || 0;
          splitTab = layout.splitTab ?? 1;
        } catch {
          /* ignore bad layout */
        }
      }
      for (const p of msg.ptys || []) {
        createTab(p.id, `sh ${tabs.length + 1}`, { silentActivate: true });
      }
      if (tabs.length === 0) openPtyForDims(80, 24);
      else {
        activeTab = Math.min(activeTab, tabs.length - 1);
        renderTabs();
        renderPanes();
      }
      afterSessionReady();
      setStatus(`reattached ${sessionId} (${(msg.ptys || []).length} ptys)`);
      break;
    case "pty_opened":
      createTab(msg.id, `sh ${tabs.length + 1}`);
      {
        const tab = tabs[tabs.length - 1];
        if (pendingSplit && tabs.length >= 2) {
          splitMode = pendingSplit;
          splitTab = tabs.length - 1;
          pendingSplit = null;
          renderPanes();
          persistLayout();
          setStatus(`split ${splitMode}`);
        }
        requestAnimationFrame(() => {
          try {
            tab.fit.fit();
            tab.term.focus();
          } catch {
        /* ignore */
      }
        });
      }
      break;
    case "pty_data": {
      const tab = tabs.find((t) => t.id === msg.id);
      if (tab) tab.term.write(b64decode(msg.data));
      break;
    }
    case "pty_closed": {
      const idx = tabs.findIndex((t) => t.id === msg.id);
      if (idx >= 0) {
        try {
          tabs[idx].term.dispose();
        } catch {
        /* ignore */
      }
        tabs.splice(idx, 1);
        if (activeTab >= tabs.length) activeTab = Math.max(0, tabs.length - 1);
        renderTabs();
        renderPanes();
      }
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
      break;
    case "fs_changed": {
      const paths = Array.isArray(msg.paths) ? msg.paths : [];
      if (paths.length && paths.every(pathIsNoisy)) break;
      scheduleTreeRefresh();
      break;
    }
    case "error":
      for (const [id, pending] of pendingFs) {
        if (msg.message && msg.message.startsWith(id)) {
          pendingFs.delete(id);
          pending.reject(new Error(`${msg.code}: ${msg.message}`));
        }
      }
      for (const [id, pending] of pendingEditor) {
        if (msg.message && msg.message.startsWith(id)) {
          pendingEditor.delete(id);
          pending.reject(new Error(`${msg.code}: ${msg.message}`));
        }
      }
      for (const [id, pending] of pendingEdit) {
        if (msg.message && msg.message.startsWith(id)) {
          pendingEdit.delete(id);
          pending.reject(new Error(`${msg.code}: ${msg.message}`));
        }
      }
      for (const [id, pending] of pendingSave) {
        if (msg.message && msg.message.startsWith(id)) {
          pendingSave.delete(id);
          pending.reject(new Error(`${msg.code}: ${msg.message}`));
        }
      }
      if (msg.code === "session_attach_failed") {
        localStorage.removeItem(SESSION_KEY);
        $input("session").value = "";
        setStatus(`${msg.code}: ${msg.message}; creating new session…`);
        const layout = localStorage.getItem(LAYOUT_KEY);
        send({ type: "session_create", layout: layout || undefined });
        break;
      }
      setStatus(`${msg.code}: ${msg.message}`);
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
    const layout = localStorage.getItem(LAYOUT_KEY);
    send({ type: "session_create", layout: layout || undefined });
  }
}

function disconnect(): void {
  if (watchId) send({ type: "fs_unwatch", watch_id: watchId });
  watchId = null;
  if (openBufferId) send({ type: "editor_close", buffer_id: openBufferId });
  if (ws) {
    try {
      ws.close();
    } catch {
        /* ignore */
      }
  }
  ws = null;
  sessionId = null;
  hasEditor = false;
  pendingEditor.clear();
  pendingEdit.clear();
  pendingSave.clear();
  openBufferId = null;
  hideEditor();
  clearTerminals();
  clearTree();
  treeEl.innerHTML =
    '<div class="tree-empty" style="padding:0.75rem;color:var(--muted)">Connect to load remote tree</div>';
  connectBtn.disabled = false;
  disconnectBtn.disabled = true;
  $button("new-tab").disabled = true;
  $button("split-h").disabled = true;
  $button("split-v").disabled = true;
  $button("split-off").disabled = true;
  setStatus("disconnected (session kept on backend if created)");
}

function connect(): void {
  disconnect();
  const url = $input("url").value.trim();
  setStatus(`connecting ${url}…`);
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    connectBtn.disabled = true;
    disconnectBtn.disabled = false;
    setStatus("socket open, waiting for hello…");
  });
  ws.addEventListener("message", (ev) => {
    if (typeof ev.data === "string") onMessage(ev.data);
  });
  ws.addEventListener("close", () => {
    connectBtn.disabled = false;
    disconnectBtn.disabled = true;
    $button("new-tab").disabled = true;
    setStatus("disconnected (session kept on backend if created)");
  });
  ws.addEventListener("error", () => setStatus("websocket error"));
}

connectBtn.addEventListener("click", connect);
disconnectBtn.addEventListener("click", disconnect);
$button("editor-close").addEventListener("click", hideEditor);
editorSaveBtn.addEventListener("click", () => {
  saveOpenBuffer();
});
window.addEventListener("keydown", (ev) => {
  if ((ev.ctrlKey || ev.metaKey) && ev.key === "s") {
    if (openBufferId && editorDirty) {
      ev.preventDefault();
      saveOpenBuffer();
    }
  }
});

$button("new-tab").addEventListener("click", () => {
  const dims = tabs[activeTab]?.fit.proposeDimensions?.() || { cols: 80, rows: 24 };
  openPtyForDims(dims.cols || 80, dims.rows || 24);
});

function requestSplit(mode: SplitMode): void {
  if (tabs.length < 1) {
    setStatus("open a shell first");
    return;
  }
  if (tabs.length < 2) {
    pendingSplit = mode;
    setStatus(`opening second shell for ${mode} split…`);
    const dims = tabs[activeTab]?.fit.proposeDimensions?.() || { cols: 80, rows: 24 };
    openPtyForDims(dims.cols || 80, dims.rows || 24);
    return;
  }
  splitMode = mode;
  if (splitTab === activeTab || splitTab >= tabs.length) {
    splitTab = (activeTab + 1) % tabs.length;
  }
  renderPanes();
  persistLayout();
  setStatus(`split ${mode}`);
}

$button("split-h").addEventListener("click", () => requestSplit("horizontal"));
$button("split-v").addEventListener("click", () => requestSplit("vertical"));

$button("split-off").addEventListener("click", () => {
  splitMode = null;
  pendingSplit = null;
  renderPanes();
  persistLayout();
  setStatus("no split");
});

window.addEventListener("resize", () => {
  for (const tab of tabs) {
    try {
      tab.fit.fit();
    } catch {
        /* ignore */
      }
  }
});

const savedSession = localStorage.getItem(SESSION_KEY);
if (savedSession) $input("session").value = savedSession;
