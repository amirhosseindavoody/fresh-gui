/* Phase 1 host UI: connect dialog + single xterm tab over ADE WebSocket. */

const PROTOCOL_VERSION = "0.1.0";

const $ = (id) => document.getElementById(id);
const statusEl = $("status");
const connectBtn = $("connect");
const disconnectBtn = $("disconnect");

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

function setStatus(text) {
  statusEl.textContent = text;
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

function openPty() {
  const dims = fit.proposeDimensions() || { cols: term.cols, rows: term.rows };
  send({
    type: "pty_open",
    cols: dims.cols || term.cols,
    rows: dims.rows || term.rows,
  });
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
        capabilities: ["ping", "pty"],
      });
      {
        const token = $("token").value;
        if (token) {
          send({ type: "auth", token });
        } else {
          authed = true;
          openPty();
        }
      }
      setStatus(`hello from ${msg.implementation}`);
      break;
    case "auth_ok":
      authed = true;
      openPty();
      setStatus("authenticated");
      break;
    case "auth_error":
      setStatus(`auth failed: ${msg.message}`);
      term.writeln(`\r\n\x1b[31mauth failed: ${msg.message}\x1b[0m`);
      break;
    case "pty_opened":
      ptyId = msg.id;
      setStatus(`pty ${ptyId}`);
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
    case "error":
      setStatus(`${msg.code}: ${msg.message}`);
      term.writeln(`\r\n\x1b[31m${msg.code}: ${msg.message}\x1b[0m`);
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
