/** OSC 7 cwd extraction (shell integration) — Terax-compatible. */

const OSC7_RE = /\x1b\]7;([^\x07\x1b]*)(?:\x07|\x1b\\)/g;

/** Parse OSC 7 payload (`file://host/path` or raw path) → absolute path. */
export function parseOsc7(data: string): string | null {
  const m = data.match(/^file:\/\/[^/]*(\/.*)$/);
  if (m) {
    let path = m[1];
    try {
      path = decodeURIComponent(path);
    } catch {
      /* keep */
    }
    return path || null;
  }
  // Fallback: raw path
  let s = data.trim();
  if (s.startsWith("file://")) {
    s = s.slice("file://".length);
    const slash = s.indexOf("/");
    if (slash >= 0) s = s.slice(slash);
  }
  try {
    s = decodeURIComponent(s);
  } catch {
    /* keep */
  }
  return s.startsWith("/") ? s : null;
}

/**
 * Scan a decoded PTY chunk for OSC 7, keeping a short carry buffer for
 * sequences split across WebSocket frames.
 */
export function feedOsc7Chunk(carry: { buf: string }, chunk: string): string | null {
  const s = carry.buf + chunk;
  let last: string | null = null;
  let lastEnd = 0;
  OSC7_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = OSC7_RE.exec(s)) !== null) {
    const cwd = parseOsc7(m[1]);
    if (cwd) last = cwd;
    lastEnd = m.index + m[0].length;
  }
  // Keep only an incomplete OSC tail after the last complete match.
  const rest = s.slice(lastEnd);
  const keep = 512;
  const tail = rest.slice(Math.max(0, rest.length - keep));
  const start = tail.lastIndexOf("\x1b]");
  carry.buf = start >= 0 ? tail.slice(start) : "";
  return last;
}
