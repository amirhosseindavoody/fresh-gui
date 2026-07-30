/** Lightweight right-click / context menu + name prompt. */

export type ContextMenuItem =
  | {
      kind: "item";
      label: string;
      hint?: string;
      disabled?: boolean;
      /** Destructive styling (e.g. delete). */
      variant?: "default" | "destructive";
      run: () => void | Promise<void>;
    }
  | { kind: "separator" };

let root: HTMLElement | null = null;
let promptRoot: HTMLElement | null = null;

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
    btn.dataset.slot = "menu-item";
    if (item.variant === "destructive") btn.dataset.variant = "destructive";
    btn.disabled = !!item.disabled;

    const label = document.createElement("span");
    label.className = "ctx-label";
    label.textContent = item.label;
    btn.appendChild(label);
    if (item.hint) {
      const hint = document.createElement("span");
      hint.className = "ctx-hint";
      hint.textContent = item.hint;
      btn.appendChild(hint);
    }

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

  const first = menu.querySelector<HTMLButtonElement>("button.ctx-item:not(:disabled)");
  first?.focus({ preventScroll: true });
}

/** Open a menu below an anchor control (toolbar / tab-bar buttons). */
export function openContextMenuForAnchor(
  anchor: HTMLElement,
  items: ContextMenuItem[],
  opts: { align?: "start" | "end" } = {},
): void {
  const r = anchor.getBoundingClientRect();
  openContextMenu(r.left, r.bottom + 4, items);
  if (!root || root.hidden) return;
  const m = root.getBoundingClientRect();
  const pad = 6;
  let left = opts.align === "end" ? r.right - m.width : r.left;
  left = Math.max(pad, Math.min(left, window.innerWidth - m.width - pad));
  const top = Math.min(r.bottom + 4, window.innerHeight - m.height - pad);
  root.style.left = `${left}px`;
  root.style.top = `${Math.max(pad, top)}px`;
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

function ensurePrompt(): {
  root: HTMLElement;
  title: HTMLElement;
  input: HTMLInputElement;
  cancel: HTMLButtonElement;
  ok: HTMLButtonElement;
} {
  if (promptRoot) {
    return {
      root: promptRoot,
      title: promptRoot.querySelector(".name-prompt-title")!,
      input: promptRoot.querySelector(".name-prompt-input")!,
      cancel: promptRoot.querySelector('[data-action="cancel"]')!,
      ok: promptRoot.querySelector('[data-action="ok"]')!,
    };
  }
  promptRoot = document.createElement("div");
  promptRoot.className = "name-prompt";
  promptRoot.hidden = true;
  promptRoot.innerHTML = `
    <div class="name-prompt-backdrop" data-close="1"></div>
    <div class="name-prompt-panel" role="dialog" aria-modal="true">
      <div class="name-prompt-title"></div>
      <input class="name-prompt-input" type="text" spellcheck="false" autocomplete="off" />
      <div class="name-prompt-actions">
        <button type="button" class="name-prompt-btn" data-action="cancel">Cancel</button>
        <button type="button" class="name-prompt-btn primary" data-action="ok">Create</button>
      </div>
    </div>
  `;
  document.body.appendChild(promptRoot);
  return {
    root: promptRoot,
    title: promptRoot.querySelector(".name-prompt-title")!,
    input: promptRoot.querySelector(".name-prompt-input")!,
    cancel: promptRoot.querySelector('[data-action="cancel"]')!,
    ok: promptRoot.querySelector('[data-action="ok"]')!,
  };
}

export type NamePromptOptions = {
  title: string;
  initial?: string;
  placeholder?: string;
  confirmLabel?: string;
};

/** Modal name entry used by New File / New Folder. Resolves `null` on cancel. */
export function promptName(opts: NamePromptOptions): Promise<string | null> {
  closeContextMenu();
  const { root, title, input, cancel, ok } = ensurePrompt();
  title.textContent = opts.title;
  input.value = opts.initial ?? "";
  input.placeholder = opts.placeholder ?? "";
  ok.textContent = opts.confirmLabel ?? "Create";
  root.hidden = false;

  return new Promise((resolve) => {
    const finish = (value: string | null) => {
      root.hidden = true;
      root.removeEventListener("click", onClick);
      input.removeEventListener("keydown", onKey);
      cancel.removeEventListener("click", onCancel);
      ok.removeEventListener("click", onOk);
      resolve(value);
    };
    const onCancel = () => finish(null);
    const onOk = () => {
      const name = input.value.trim();
      finish(name || null);
    };
    const onClick = (ev: Event) => {
      const t = ev.target as HTMLElement;
      if (t.dataset.close === "1") finish(null);
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") {
        ev.preventDefault();
        finish(null);
      } else if (ev.key === "Enter") {
        ev.preventDefault();
        onOk();
      }
    };
    root.addEventListener("click", onClick);
    input.addEventListener("keydown", onKey);
    cancel.addEventListener("click", onCancel);
    ok.addEventListener("click", onOk);
    requestAnimationFrame(() => {
      input.focus();
      input.select();
    });
  });
}
