import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { SearchAddon } from "@xterm/addon-search";
import type { UiSettings } from "./settings";
import { parseOsc7 } from "./osc7";
import { xtermThemeFromCss } from "./theme";
import { copyToClipboard } from "./context-menu";

export { parseOsc7 };

export interface TermBundle {
  term: Terminal;
  fit: FitAddon;
  webgl: WebglAddon | null;
  search: SearchAddon;
  el: HTMLElement;
  cwd?: string;
  /** Incomplete OSC tail across PTY frames. */
  oscCarry: { buf: string };
}

export type CreateTerminalOpts = {
  settings?: UiSettings;
  onCwd?: (cwd: string) => void;
  /** Fired after a successful selection copy (Fresh-style status). */
  onCopied?: (text: string) => void;
  onPasteFailed?: (message: string) => void;
};

/** Apply the current CSS token palette to an open xterm instance. */
export function applyTerminalTheme(bundle: TermBundle): void {
  bundle.term.options.theme = xtermThemeFromCss();
}

function isModKey(ev: KeyboardEvent): boolean {
  return (ev.ctrlKey || ev.metaKey) && !ev.altKey;
}

/**
 * Fresh-inspired clipboard policy on xterm.js:
 * - Mouse drag selects text (xterm built-in; Shift+drag if the PTY app
 *   enabled DEC mouse reporting).
 * - Ctrl/Cmd+C copies when there is a selection, then clears it so the next
 *   Ctrl+C is SIGINT (VS Code / Fresh “copy then resume”).
 * - Ctrl/Cmd+V pastes from the system clipboard via `term.paste` (bracketed
 *   paste when the shell supports it).
 */
function installClipboardKeys(term: Terminal, opts: CreateTerminalOpts): void {
  term.attachCustomKeyEventHandler((ev) => {
    if (ev.type !== "keydown" || !isModKey(ev) || ev.shiftKey) return true;
    const key = ev.key.length === 1 ? ev.key.toLowerCase() : ev.key;

    if (key === "c") {
      if (!term.hasSelection()) return true; // let SIGINT reach the PTY
      const text = term.getSelection();
      if (!text) return true;
      ev.preventDefault();
      ev.stopPropagation();
      void copyToClipboard(text).then((ok) => {
        if (ok) {
          term.clearSelection();
          opts.onCopied?.(text);
        }
      });
      return false;
    }

    if (key === "v") {
      ev.preventDefault();
      ev.stopPropagation();
      void navigator.clipboard
        .readText()
        .then((text) => {
          if (text) term.paste(text);
        })
        .catch(() => {
          opts.onPasteFailed?.("clipboard paste blocked by the browser");
        });
      return false;
    }

    return true;
  });
}

export function createTerminal(opts: CreateTerminalOpts = {}): TermBundle {
  const settings = opts.settings;
  const fontSize = settings?.terminalFontSize ?? 14;
  const preferWebgl = settings?.webgl !== false;

  const term = new Terminal({
    cursorBlink: true,
    fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
    fontSize,
    theme: xtermThemeFromCss(),
    // Word select on double-click / right-click (Fresh scrollback double-click).
    rightClickSelectsWord: true,
    // Keep enough scrollback for mouse selection after output scrolls.
    scrollback: 5000,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  const search = new SearchAddon();
  term.loadAddon(search);

  const el = document.createElement("div");
  el.className = "xterm-host";
  term.open(el);

  installClipboardKeys(term, opts);

  // OSC 7 → cwd (xterm parser; also scanned from raw PTY bytes in main.ts)
  try {
    term.parser.registerOscHandler(7, (data) => {
      const cwd = parseOsc7(data);
      if (cwd) opts.onCwd?.(cwd);
      return true;
    });
  } catch {
    /* older xterm */
  }

  let webgl: WebglAddon | null = null;
  if (preferWebgl) {
    try {
      webgl = new WebglAddon();
      webgl.onContextLoss(() => {
        try {
          webgl?.dispose();
        } catch {
          /* ignore */
        }
        webgl = null;
      });
      term.loadAddon(webgl);
    } catch {
      webgl = null;
    }
  }

  return { term, fit, webgl, search, el, oscCarry: { buf: "" } };
}

export function disposeTerminal(bundle: TermBundle): void {
  try {
    bundle.webgl?.dispose();
  } catch {
    /* ignore */
  }
  try {
    bundle.term.dispose();
  } catch {
    /* ignore */
  }
}

export function applyTerminalFontSize(bundle: TermBundle, size: number): void {
  bundle.term.options.fontSize = size;
  try {
    bundle.fit.fit();
  } catch {
    /* ignore */
  }
}
