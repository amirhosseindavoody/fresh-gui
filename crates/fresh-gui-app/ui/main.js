/* Phase 2 host UI: sessions, multi-tab, splits, file tree. */

const PROTOCOL_VERSION = "0.2.0";
const SESSION_KEY = "fresh-gui.sessionId";
const LAYOUT_KEY = "fresh-gui.layout";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const connectBtn = $("connect");
const disconnectBtn = $("disconnect");
const treeEl = $("tree");
const tabsEl = $("tabs");
const panesEl = $("panes");

let ws = null;
let authed = false;
let sessionId = null;
let reqSeq = 0;
/** @type {Map<string, {resolve: Function, reject: Function}>} */
const pendingFs = new Map();
let selectedPath = null;

/** @type {{ id: string, title: string, term: any, fit: any, el: HTMLElement }[]} */
let tabs = [];
let activeTab = 0;
/** null | 'horizontal' | 'vertical' */
let splitMode = null;
/** second pane tab index when split */
let splitTab = 1;
/** Apply this split once a newly opened PTY arrives (when splitting with <2 tabs). */
let pendingSplit = null;

function setStatus(text) {
  statusEl.textContent = text;
  statusEl.title = text;
}

function b64encode(str) {
  const bytes = new TextEncoder().encode(str);
  let s = "";
  bytes.forEach((b) => {
    s += String.fromCharCode(b);
  });
  return btoa(s);
}

function b64decode(b64) {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

function send(msg) {
  if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(msg));
}

function nextRequestId() {
  reqSeq += 1;
  return `ui-${reqSeq}`;
}

function fsList(path) {
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

function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function persistLayout() {
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

function makeTerm() {
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
  const fit = new FitAddon.FitAddon();
  term.loadAddon(fit);
  return { term, fit };
}

function openPtyForDims(cols, rows) {
  send({ type: "pty_open", cols, rows });
}

function createTab(ptyId, title, opts = {}) {
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

function closeTab(index) {
  const tab = tabs[index];
  if (!tab) return;
  send({ type: "pty_close", id: tab.id });
  try {
    tab.term.dispose();
  } catch (_) {}
  tabs.splice(index, 1);
  if (activeTab >= tabs.length) activeTab = Math.max(0, tabs.length - 1);
  if (splitTab >= tabs.length) splitTab = Math.max(0, tabs.length - 1);
  if (tabs.length < 2) splitMode = null;
  renderTabs();
  renderPanes();
  persistLayout();
}

function renderTabs() {
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

function renderPanes() {
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
      } catch (_) {}
    });
  }
}

function clearTerminals() {
  for (const tab of tabs) {
    try {
      tab.term.dispose();
    } catch (_) {}
  }
  tabs = [];
  activeTab = 0;
  splitTab = 1;
  splitMode = null;
  tabsEl.innerHTML = "";
  panesEl.innerHTML = "";
  panesEl.className = "no-split";
}

function clearTree() {
  treeEl.innerHTML = "";
  pendingFs.clear();
  selectedPath = null;
}

async function loadRoot() {
  clearTree();
  treeEl.textContent = "Loading…";
  try {
    const listed = await fsList("");
    treeEl.innerHTML = "";
    const rootLabel = document.createElement("div");
    rootLabel.className = "tree-item";
    rootLabel.innerHTML = `<span class="twist"></span><span class="kind">⌂</span><span>${escapeHtml(listed.path)}</span>`;
    treeEl.appendChild(rootLabel);
    const children = document.createElement("div");
    children.className = "tree-children";
    treeEl.appendChild(children);
    renderEntries(children, listed.entries);
    setStatus(`session ${sessionId || "?"} · ${listed.path}`);
  } catch (err) {
    treeEl.innerHTML = `<div style="padding:0.75rem;color:#f85149">${escapeHtml(String(err))}</div>`;
  }
}

function kindIcon(kind) {
  if (kind === "dir") return "▸";
  if (kind === "symlink") return "↗";
  return "·";
}

function renderEntries(container, entries) {
  for (const entry of entries) {
    const row = document.createElement("div");
    row.className = "tree-item";
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
      document.querySelectorAll(".tree-item.selected").forEach((el) => el.classList.remove("selected"));
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
          } catch (err) {
            twist.textContent = "▸";
            setStatus(`fs error: ${err}`);
          }
        } else {
          childBox.hidden = !childBox.hidden;
          twist.textContent = childBox.hidden ? "▸" : "▾";
        }
      }
    });
    container.append(row, childBox);
  }
}

function afterSessionReady() {
  $("new-tab").disabled = false;
  $("split-h").disabled = false;
  $("split-v").disabled = false;
  $("split-off").disabled = false;
  loadRoot();
}

function onMessage(raw) {
  let msg;
  try {
    msg = JSON.parse(raw);
  } catch {
    setStatus("bad json from backend");
    return;
  }

  switch (msg.type) {
    case "hello":
      send({
        type: "hello",
        protocol_version: PROTOCOL_VERSION,
        role: "client",
        implementation: "fresh-gui-ui/0.2",
        capabilities: ["ping", "pty", "fs", "session"],
      });
      {
        const token = $("token").value;
        if (token) send({ type: "auth", token });
        else {
          authed = true;
          beginSession();
        }
      }
      setStatus(`hello from ${msg.implementation}`);
      break;
    case "auth_ok":
      authed = true;
      beginSession();
      setStatus("authenticated");
      break;
    case "auth_error":
      setStatus(`auth failed: ${msg.message}`);
      break;
    case "session_created":
      sessionId = msg.session_id;
      $("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      afterSessionReady();
      openPtyForDims(80, 24);
      setStatus(`session ${sessionId}`);
      break;
    case "session_attached":
      sessionId = msg.session_id;
      $("session").value = sessionId;
      localStorage.setItem(SESSION_KEY, sessionId);
      clearTerminals();
      if (msg.layout) {
        try {
          const layout = JSON.parse(msg.layout);
          splitMode = layout.split || null;
          activeTab = layout.activeTab || 0;
          splitTab = layout.splitTab ?? 1;
        } catch (_) {}
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
          } catch (_) {}
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
        } catch (_) {}
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
    case "error":
      for (const [id, pending] of pendingFs) {
        if (msg.message && msg.message.startsWith(id)) {
          pendingFs.delete(id);
          pending.reject(new Error(`${msg.code}: ${msg.message}`));
        }
      }
      if (msg.code === "session_attach_failed") {
        localStorage.removeItem(SESSION_KEY);
        $("session").value = "";
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

function beginSession() {
  const wanted = $("session").value.trim() || localStorage.getItem(SESSION_KEY) || "";
  if (wanted) {
    $("session").value = wanted;
    send({ type: "session_attach", session_id: wanted });
  } else {
    const layout = localStorage.getItem(LAYOUT_KEY);
    send({ type: "session_create", layout: layout || undefined });
  }
}

function disconnect() {
  if (ws) {
    try {
      ws.close();
    } catch (_) {}
  }
  ws = null;
  authed = false;
  sessionId = null;
  clearTerminals();
  clearTree();
  treeEl.innerHTML =
    '<div class="tree-empty" style="padding:0.75rem;color:var(--muted)">Connect to load remote tree</div>';
  connectBtn.disabled = false;
  disconnectBtn.disabled = true;
  $("new-tab").disabled = true;
  $("split-h").disabled = true;
  $("split-v").disabled = true;
  $("split-off").disabled = true;
  setStatus("disconnected (session kept on backend if created)");
}

function connect() {
  disconnect();
  const url = $("url").value.trim();
  setStatus(`connecting ${url}…`);
  ws = new WebSocket(url);
  ws.addEventListener("open", () => {
    connectBtn.disabled = true;
    disconnectBtn.disabled = false;
    setStatus("socket open, waiting for hello…");
  });
  ws.addEventListener("message", (ev) => onMessage(ev.data));
  ws.addEventListener("close", () => {
    connectBtn.disabled = false;
    disconnectBtn.disabled = true;
    $("new-tab").disabled = true;
    setStatus("disconnected (session kept on backend if created)");
  });
  ws.addEventListener("error", () => setStatus("websocket error"));
}

connectBtn.addEventListener("click", connect);
disconnectBtn.addEventListener("click", disconnect);

$("new-tab").addEventListener("click", () => {
  const dims = tabs[activeTab]?.fit.proposeDimensions?.() || { cols: 80, rows: 24 };
  openPtyForDims(dims.cols || 80, dims.rows || 24);
});

function requestSplit(mode) {
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

$("split-h").addEventListener("click", () => requestSplit("horizontal"));
$("split-v").addEventListener("click", () => requestSplit("vertical"));

$("split-off").addEventListener("click", () => {
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
    } catch (_) {}
  }
});

const savedSession = localStorage.getItem(SESSION_KEY);
if (savedSession) $("session").value = savedSession;
