/** Command palette + Go to File (Ctrl+P / Ctrl+Shift+P), VS Code–style. */

import { SHORTCUTS, type ShortcutId } from "./shortcuts";

export type PaletteCommand = {
  id: string;
  label: string;
  shortcutId?: ShortcutId;
  run: () => void;
};

export type PaletteMode = "commands" | "gotoFile";

let root: HTMLElement | null = null;
let listEl: HTMLElement | null = null;
let filterInput: HTMLInputElement | null = null;
let hintEl: HTMLElement | null = null;
let titleEl: HTMLElement | null = null;
let commands: PaletteCommand[] = [];
let filtered: PaletteCommand[] = [];
let active = 0;
let open = false;
let mode: PaletteMode = "commands";
let onGotoFile: ((path: string) => void) | null = null;

function ensureDom(): void {
  if (root) return;
  root = document.createElement("div");
  root.className = "palette";
  root.hidden = true;
  root.innerHTML = `
    <div class="palette-backdrop" data-close="1"></div>
    <div class="palette-panel" role="dialog" aria-label="Command palette">
      <div class="palette-title"></div>
      <input class="palette-input" type="text" spellcheck="false" />
      <div class="palette-list" role="listbox"></div>
      <div class="palette-hint">Esc to close</div>
    </div>
  `;
  document.body.appendChild(root);
  filterInput = root.querySelector(".palette-input");
  listEl = root.querySelector(".palette-list");
  hintEl = root.querySelector(".palette-hint");
  titleEl = root.querySelector(".palette-title");

  root.addEventListener("click", (ev) => {
    const t = ev.target as HTMLElement;
    if (t.dataset.close === "1") closePalette();
  });
  filterInput?.addEventListener("input", () => {
    if (mode === "commands") {
      applyFilter();
      renderList();
    } else {
      renderGotoHint();
    }
  });
  filterInput?.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closePalette();
      return;
    }
    if (mode === "gotoFile") {
      if (ev.key === "Enter") {
        ev.preventDefault();
        submitGotoFile();
      }
      return;
    }
    if (ev.key === "ArrowDown") {
      ev.preventDefault();
      active = Math.min(active + 1, Math.max(0, filtered.length - 1));
      renderList();
    } else if (ev.key === "ArrowUp") {
      ev.preventDefault();
      active = Math.max(active - 1, 0);
      renderList();
    } else if (ev.key === "Enter") {
      ev.preventDefault();
      runActive();
    }
  });
}

function applyFilter(): void {
  const q = (filterInput?.value || "").trim().toLowerCase();
  filtered = !q
    ? commands.slice()
    : commands.filter((c) => c.label.toLowerCase().includes(q) || c.id.includes(q));
  active = 0;
}

function renderList(): void {
  if (!listEl) return;
  listEl.innerHTML = "";
  listEl.hidden = false;
  filtered.forEach((cmd, i) => {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "palette-item" + (i === active ? " active" : "");
    row.setAttribute("role", "option");
    row.textContent = cmd.label;
    row.addEventListener("click", () => {
      active = i;
      runActive();
    });
    listEl!.appendChild(row);
  });
  if (filtered.length === 0) {
    const empty = document.createElement("div");
    empty.className = "palette-empty";
    empty.textContent = "No matching commands";
    listEl.appendChild(empty);
  }
}

function renderGotoHint(): void {
  if (!listEl) return;
  listEl.innerHTML = "";
  listEl.hidden = false;
  const empty = document.createElement("div");
  empty.className = "palette-empty";
  const q = (filterInput?.value || "").trim();
  empty.textContent = q
    ? `Open “${q}” (Enter) — path[:line[:col]]`
    : "Paste or type a path, optionally path:line:col";
  listEl.appendChild(empty);
}

function runActive(): void {
  const cmd = filtered[active];
  if (!cmd) return;
  closePalette();
  cmd.run();
}

function submitGotoFile(): void {
  const path = (filterInput?.value || "").trim();
  if (!path) return;
  const handler = onGotoFile;
  closePalette();
  handler?.(path);
}

function show(nextMode: PaletteMode): void {
  ensureDom();
  if (!root || !filterInput || !titleEl || !hintEl) return;
  mode = nextMode;
  open = true;
  root.hidden = false;
  filterInput.value = "";
  const panel = root.querySelector(".palette-panel");
  if (nextMode === "gotoFile") {
    titleEl.textContent = "Go to File";
    filterInput.placeholder = "path/to/file[:line[:col]]";
    hintEl.textContent = "Enter to open · Esc to close";
    panel?.setAttribute("aria-label", "Go to File");
    renderGotoHint();
  } else {
    titleEl.textContent = "Command Palette";
    filterInput.placeholder = "Type a command…";
    hintEl.textContent = "Esc to close";
    panel?.setAttribute("aria-label", "Command palette");
    applyFilter();
    renderList();
  }
  filterInput.focus();
}

export function setPaletteCommands(next: PaletteCommand[]): void {
  commands = next;
}

export function setGotoFileHandler(handler: (path: string) => void): void {
  onGotoFile = handler;
}

export function openPalette(): void {
  show("commands");
}

export function openGotoFile(): void {
  show("gotoFile");
}

export function closePalette(): void {
  if (!root) return;
  open = false;
  root.hidden = true;
}

export function isPaletteOpen(): boolean {
  return open;
}

export function defaultPaletteCommands(runShortcut: (id: ShortcutId) => void): PaletteCommand[] {
  return SHORTCUTS.map((s) => ({
    id: s.id,
    label: s.label,
    shortcutId: s.id,
    run: () => runShortcut(s.id),
  }));
}
