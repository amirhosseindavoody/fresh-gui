/** Lightweight right-click / context menu. */

export type ContextMenuItem =
  | { kind: "item"; label: string; disabled?: boolean; run: () => void | Promise<void> }
  | { kind: "separator" };

let root: HTMLElement | null = null;

function ensureRoot(): HTMLElement {
  if (root) return root;
  root = document.createElement("div");
  root.className = "ctx-menu";
  root.hidden = true;
  root.setAttribute("role", "menu");
  document.body.appendChild(root);

  const dismiss = (ev: Event) => {
    if (!root || root.hidden) return;
    if (ev.type === "keydown" && (ev as KeyboardEvent).key !== "Escape") return;
    if (ev.type === "pointerdown" && root.contains(ev.target as Node)) return;
    closeContextMenu();
  };
  window.addEventListener("pointerdown", dismiss, true);
  window.addEventListener("keydown", dismiss, true);
  window.addEventListener("blur", () => closeContextMenu());
  window.addEventListener("resize", () => closeContextMenu());
  return root;
}

export function closeContextMenu(): void {
  if (!root) return;
  root.hidden = true;
  root.replaceChildren();
}

export function openContextMenu(clientX: number, clientY: number, items: ContextMenuItem[]): void {
  const menu = ensureRoot();
  menu.replaceChildren();
  for (const item of items) {
    if (item.kind === "separator") {
      const sep = document.createElement("div");
      sep.className = "ctx-sep";
      sep.setAttribute("role", "separator");
      menu.appendChild(sep);
      continue;
    }
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "ctx-item";
    btn.setAttribute("role", "menuitem");
    btn.textContent = item.label;
    btn.disabled = !!item.disabled;
    btn.addEventListener("click", (ev) => {
      ev.preventDefault();
      ev.stopPropagation();
      closeContextMenu();
      void item.run();
    });
    menu.appendChild(btn);
  }

  menu.hidden = false;
  // Position after layout so we can clamp to the viewport.
  const pad = 6;
  const rect = menu.getBoundingClientRect();
  const x = Math.min(clientX, window.innerWidth - rect.width - pad);
  const y = Math.min(clientY, window.innerHeight - rect.height - pad);
  menu.style.left = `${Math.max(pad, x)}px`;
  menu.style.top = `${Math.max(pad, y)}px`;
}

export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.cssText = "position:fixed;left:-9999px;top:0";
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand("copy");
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}
