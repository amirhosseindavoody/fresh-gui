import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import { SearchAddon } from "@xterm/addon-search";
import type { UiSettings } from "./settings";
import { parseOsc7 } from "./osc7";

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

export function createTerminal(
  opts: {
    settings?: UiSettings;
    onCwd?: (cwd: string) => void;
  } = {},
): TermBundle {
  const settings = opts.settings;
  const fontSize = settings?.terminalFontSize ?? 14;
  const preferWebgl = settings?.webgl !== false;

  const dark = (settings?.theme ?? "dark") !== "light";
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
    fontSize,
    theme: dark
      ? {
          background: "#010409",
          foreground: "#e6edf3",
          cursor: "#3fb950",
          selectionBackground: "#3fb95055",
        }
      : {
          background: "#f6f8fa",
          foreground: "#1f2328",
          cursor: "#1a7f37",
          selectionBackground: "#1a7f3755",
        },
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  const search = new SearchAddon();
  term.loadAddon(search);

  const el = document.createElement("div");
  el.className = "xterm-host";
  term.open(el);

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
