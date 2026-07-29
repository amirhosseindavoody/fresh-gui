/** Minimal command palette stub (UI-2). */

import { SHORTCUTS, type ShortcutId } from "./shortcuts";

export type PaletteCommand = {
  id: string;
  label: string;
  shortcutId?: ShortcutId;
  run: () => void;
};

let root: HTMLElement | null = null;
let listEl: HTMLElement | null = null;
let filterInput: HTMLInputElement | null = null;
let commands: PaletteCommand[] = [];
let filtered: PaletteCommand[] = [];
let active = 0;
let open = false;

function ensureDom(): void {
  if (root) return;
  root = document.createElement("div");
  root.className = "palette";
  root.hidden = true;
  root.innerHTML = `
    <div class="palette-backdrop" data-close="1"></div>
    <div class="palette-panel" role="dialog" aria-label="Command palette">
      <input class="palette-input" type="text" placeholder="Type a command…" spellcheck="false" />
      <div class="palette-list" role="listbox"></div>
      <div class="palette-hint">Esc to close</div>
    </div>
  `;
  document.body.appendChild(root);
  filterInput = root.querySelector(".palette-input");
  listEl = root.querySelector(".palette-list");

  root.addEventListener("click", (ev) => {
    const t = ev.target as HTMLElement;
    if (t.dataset.close === "1") closePalette();
  });
  filterInput?.addEventListener("input", () => {
    applyFilter();
    renderList();
  });
  filterInput?.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closePalette();
    } else if (ev.key === "ArrowDown") {
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

function runActive(): void {
  const cmd = filtered[active];
  if (!cmd) return;
  closePalette();
  cmd.run();
}

export function setPaletteCommands(next: PaletteCommand[]): void {
  commands = next;
}

export function openPalette(): void {
  ensureDom();
  if (!root || !filterInput) return;
  open = true;
  root.hidden = false;
  filterInput.value = "";
  applyFilter();
  renderList();
  filterInput.focus();
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
