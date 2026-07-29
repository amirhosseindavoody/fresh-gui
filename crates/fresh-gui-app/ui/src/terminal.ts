import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";

export interface TermBundle {
  term: Terminal;
  fit: FitAddon;
  webgl: WebglAddon | null;
  el: HTMLElement;
}

export function createTerminal(): TermBundle {
  const term = new Terminal({
    cursorBlink: true,
    fontFamily: '"IBM Plex Mono", ui-monospace, monospace',
    fontSize: 14,
    theme: {
      background: "#010409",
      foreground: "#e6edf3",
      cursor: "#3fb950",
      selectionBackground: "#3fb95055",
    },
  });
  const fit = new FitAddon();
  term.loadAddon(fit);

  const el = document.createElement("div");
  el.className = "xterm-host";
  term.open(el);

  let webgl: WebglAddon | null = null;
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

  return { term, fit, webgl, el };
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
