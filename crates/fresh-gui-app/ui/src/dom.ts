export function $(id: string): HTMLElement {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el;
}

export function $input(id: string): HTMLInputElement {
  const el = $(id);
  if (!(el instanceof HTMLInputElement)) throw new Error(`#${id} is not an input`);
  return el;
}

export function $button(id: string): HTMLButtonElement {
  const el = $(id);
  if (!(el instanceof HTMLButtonElement)) throw new Error(`#${id} is not a button`);
  return el;
}

export function escapeHtml(s: string): string {
  return String(s)
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;");
}

export function b64encode(str: string): string {
  const bytes = new TextEncoder().encode(str);
  let s = "";
  bytes.forEach((b) => {
    s += String.fromCharCode(b);
  });
  return btoa(s);
}

export function b64decode(b64: string): Uint8Array {
  const bin = atob(b64);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

export function basename(path: string): string {
  const parts = path.split(/[/\\]/).filter(Boolean);
  return parts[parts.length - 1] || path || "untitled";
}

/** Normalize separators and strip trailing slashes (keep `/` as root). */
export function normalizePath(path: string): string {
  const s = String(path || "").replace(/\\/g, "/").replace(/\/+$/, "");
  return s || "/";
}

/** Path of `abs` relative to `root`, or `abs` when outside root. */
export function relativePath(abs: string, root: string): string {
  const a = normalizePath(abs);
  const r = normalizePath(root);
  if (!root || r === "/" || r === ".") return a.replace(/^\//, "") || a;
  if (a === r) return ".";
  if (a.startsWith(r + "/")) return a.slice(r.length + 1);
  return a;
}
