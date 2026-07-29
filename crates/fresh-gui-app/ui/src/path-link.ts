/**
 * File-path link detection for terminal / editor Ctrl+click.
 *
 * Mirrors Fresh `services/terminal/path_link.rs` (`detect_link_at`) so hover
 * underlines stay snappy in the browser. Actual open still goes through the
 * backend, which re-runs Fresh's detector / resolver as the source of truth.
 */

export type DetectedLink = {
  /** Character range within the source line (path + numeric suffix). */
  start: number;
  end: number;
  path: string;
  /** 1-based line from `:line` / `:line:col` / `(line,col)`. */
  line?: number;
  /** 1-based column. */
  column?: number;
};

function isOpener(c: string): boolean {
  return '"\'`()[]{}<>'.includes(c);
}

function isTrailingPunct(c: string): boolean {
  return ".,;!?)]}>\"'`".includes(c);
}

function parenthesizedIsPath(inner: string): boolean {
  const s = inner.trim();
  return (
    s.includes("/") ||
    s.includes("\\") ||
    s.includes("~") ||
    (s.includes(".") && /[a-zA-Z]/.test(s))
  );
}

function looksLikePath(path: string): boolean {
  if (!path) return false;
  if (path.includes("/") || path.includes("\\") || path.startsWith("~") || path.startsWith(".")) {
    return true;
  }
  return path.includes(".");
}

function rfindDigitRun(chars: string[], end: number): number {
  let i = end;
  while (i > 0 && /[0-9]/.test(chars[i - 1]!)) i -= 1;
  return i;
}

function parseLineColPair(inner: string): { line: number; column?: number } | null {
  const s = inner.trim();
  const comma = s.indexOf(",");
  if (comma >= 0) {
    const line = Number.parseInt(s.slice(0, comma).trim(), 10);
    const col = Number.parseInt(s.slice(comma + 1).trim(), 10);
    if (!Number.isFinite(line) || !Number.isFinite(col)) return null;
    return { line, column: col };
  }
  const line = Number.parseInt(s, 10);
  if (!Number.isFinite(line)) return null;
  return { line };
}

/** Returns `[pathLen, suffixEnd, line?, column?]`. */
function splitLocationSuffix(
  token: string,
): [number, number, number | undefined, number | undefined] {
  const chars = [...token];
  const full = chars.length;
  let end = full;
  if (end > 0 && chars[end - 1] === ":") end -= 1;

  if (end > 0 && chars[end - 1] === ")") {
    let open = -1;
    for (let i = end - 2; i >= 0; i--) {
      if (chars[i] === "(") {
        open = i;
        break;
      }
    }
    if (open > 0) {
      const inner = chars.slice(open + 1, end - 1).join("");
      const pair = parseLineColPair(inner);
      if (pair) return [open, end, pair.line, pair.column];
    }
  }

  const num1Start = rfindDigitRun(chars, end);
  if (num1Start === end || num1Start === 0 || chars[num1Start - 1] !== ":") {
    return [full, full, undefined, undefined];
  }
  const n1 = Number.parseInt(chars.slice(num1Start, end).join(""), 10);
  if (!Number.isFinite(n1)) return [full, full, undefined, undefined];
  const colon1 = num1Start - 1;

  if (colon1 > 0) {
    const num2End = colon1;
    const num2Start = rfindDigitRun(chars, num2End);
    if (num2Start < num2End && num2Start > 0 && chars[num2Start - 1] === ":") {
      const n2 = Number.parseInt(chars.slice(num2Start, num2End).join(""), 10);
      if (Number.isFinite(n2)) return [num2Start - 1, end, n2, n1];
    }
  }
  return [colon1, end, n1, undefined];
}

/** Detect a file-path link at character offset `col` within `line` (Fresh semantics). */
export function detectLinkAt(line: string, col: number): DetectedLink | null {
  const chars = [...line];
  const n = chars.length;
  if (n === 0) return null;

  let anchor = Math.min(col, n - 1);
  if (/\s/.test(chars[anchor]!)) {
    if (anchor === 0 || /\s/.test(chars[anchor - 1]!)) return null;
    anchor -= 1;
  }

  let start = anchor;
  while (start > 0 && !/\s/.test(chars[start - 1]!)) start -= 1;
  let end = anchor + 1;
  while (end < n && !/\s/.test(chars[end]!)) end += 1;

  for (let i = start; i < end; i++) {
    if (chars[i] === "(") {
      for (let j = end - 1; j > i; j--) {
        if (chars[j] === ")") {
          if (i + 1 < j) {
            const inner = chars.slice(i + 1, j).join("");
            if (parenthesizedIsPath(inner)) {
              start = i + 1;
              end = j;
            }
          }
          break;
        }
      }
      break;
    }
  }

  while (start < end && isOpener(chars[start]!)) start += 1;
  if (start >= end) return null;

  const token = chars.slice(start, end).join("");
  const [pathLen, suffixEnd, lineNo, colNo] = splitLocationSuffix(token);
  let pathEnd = start + pathLen;
  if (lineNo === undefined) {
    while (pathEnd > start && isTrailingPunct(chars[pathEnd - 1]!)) pathEnd -= 1;
  }
  if (pathEnd <= start) return null;

  const path = chars.slice(start, pathEnd).join("");
  if (!looksLikePath(path)) return null;

  const linkEnd = lineNo !== undefined ? start + suffixEnd : pathEnd;
  return {
    start,
    end: linkEnd,
    path,
    line: lineNo,
    column: colNo,
  };
}

/** Scan a whole line for non-overlapping path links (for xterm link provider). */
export function detectLinksInLine(line: string): DetectedLink[] {
  const out: DetectedLink[] = [];
  let i = 0;
  const chars = [...line];
  while (i < chars.length) {
    if (/\s/.test(chars[i]!)) {
      i += 1;
      continue;
    }
    const link = detectLinkAt(line, i);
    if (link && link.start <= i && link.end > i) {
      out.push(link);
      i = link.end;
      continue;
    }
    i += 1;
  }
  return out;
}
