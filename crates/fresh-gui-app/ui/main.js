/* Phase 1 + 1b host UI: xterm PTY + read-only remote file tree. */

const PROTOCOL_VERSION = "0.1.0";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const connectBtn = $("connect");
const disconnectBtn = $("disconnect");
const treeEl = $("tree");

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
term.open($("terminal"));
fit.fit();
window.addEventListener("resize", () => fit.fit());

let ws = null;
let ptyId = null;
let authed = false;
let reqSeq = 0;
/** @type {Map<string, {resolve: Function, reject: Function}>} */
const pendingFs = new Map();
let selectedPath = null;

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
  ws.send(JSON.stringify(msg));
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

function openPty() {
  const dims = fit.proposeDimensions() || { cols: term.cols, rows: term.rows };
  send({
    type: "pty_open",
    cols: dims.cols || term.cols,
    rows: dims.rows || term.rows,
  });
}

function kindIcon(kind) {
  if (kind === "dir") return "▸";
  if (kind === "symlink") return "↗";
  return "·";
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
    setStatus(`fs root ${listed.path}`);
  } catch (err) {
    treeEl.innerHTML = `<div style="padding:0.75rem;color:#f85149">${escapeHtml(String(err))}</div>`;
  }
}

function escapeHtml(s) {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

function renderEntries(container, entries) {
  for (const entry of entries) {
    const row = document.createElement("div");
    row.className = "tree-item";
    row.dataset.path = entry.path;
    row.dataset.kind = entry.kind;
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
        implementation: "fresh-gui-ui/0.1",
        capabilities: ["ping", "pty", "fs"],
      });
      {
        const token = $("token").value;
        if (token) {
          send({ type: "auth", token });
        } else {
          authed = true;
          openPty();
          loadRoot();
        }
      }
      setStatus(`hello from ${msg.implementation}`);
      break;
    case "auth_ok":
      authed = true;
      openPty();
      loadRoot();
      setStatus("authenticated");
      break;
    case "auth_error":
      setStatus(`auth failed: ${msg.message}`);
      term.writeln(`\r\n\x1b[31mauth failed: ${msg.message}\x1b[0m`);
      break;
    case "pty_opened":
      ptyId = msg.id;
      setStatus(selectedPath ? `${selectedPath} · pty ${ptyId}` : `pty ${ptyId}`);
      term.focus();
      break;
    case "pty_data":
      if (msg.id === ptyId) {
        term.write(b64decode(msg.data));
      }
      break;
    case "pty_closed":
      if (msg.id === ptyId) {
        setStatus(`pty closed (${msg.reason || "done"})`);
        ptyId = null;
      }
      break;
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
      setStatus(`${msg.code}: ${msg.message}`);
      if (!String(msg.code).startsWith("fs_")) {
        term.writeln(`\r\n\x1b[31m${msg.code}: ${msg.message}\x1b[0m`);
      }
      break;
    case "pong":
      break;
    default:
      break;
  }
}

function disconnect() {
  if (ws) {
    try {
      ws.close();
    } catch (_) {}
  }
  ws = null;
  ptyId = null;
  authed = false;
  clearTree();
  treeEl.innerHTML =
    '<div class="tree-empty" style="padding:0.75rem;color:var(--muted)">Connect to load remote tree</div>';
  connectBtn.disabled = false;
  disconnectBtn.disabled = true;
  setStatus("disconnected");
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
    setStatus("disconnected");
    ptyId = null;
  });
  ws.addEventListener("error", () => setStatus("websocket error"));
}

term.onData((data) => {
  if (!ws || ws.readyState !== WebSocket.OPEN || !ptyId) return;
  send({ type: "pty_data", id: ptyId, data: b64encode(data) });
});

term.onResize(({ cols, rows }) => {
  if (!ws || ws.readyState !== WebSocket.OPEN || !ptyId) return;
  send({ type: "pty_resize", id: ptyId, cols, rows });
});

connectBtn.addEventListener("click", connect);
disconnectBtn.addEventListener("click", disconnect);
