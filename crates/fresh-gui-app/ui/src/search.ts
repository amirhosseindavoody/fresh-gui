/** Header find bar for terminal (xterm SearchAddon) or editor (CodeMirror search). */

export type SearchTarget =
  | { kind: "terminal"; findNext: (q: string, backwards?: boolean) => boolean; clear: () => void }
  | { kind: "editor"; open: () => void }
  | null;

let root: HTMLElement | null = null;
let input: HTMLInputElement | null = null;
let target: SearchTarget = null;

export function setSearchTarget(next: SearchTarget): void {
  target = next;
}

function ensureDom(): void {
  if (root) return;
  root = document.createElement("div");
  root.className = "find-bar";
  root.hidden = true;
  root.innerHTML = `
    <input class="find-input" type="search" placeholder="Find…" spellcheck="false" />
    <button type="button" class="find-prev" title="Previous">↑</button>
    <button type="button" class="find-next" title="Next">↓</button>
    <button type="button" class="find-close" title="Close">×</button>
  `;
  // Insert after tabs bar if possible
  const tabsBar = document.getElementById("tabs-bar");
  if (tabsBar?.parentElement) {
    tabsBar.insertAdjacentElement("afterend", root);
  } else {
    document.body.appendChild(root);
  }
  input = root.querySelector(".find-input");
  root.querySelector(".find-next")?.addEventListener("click", () => runFind(false));
  root.querySelector(".find-prev")?.addEventListener("click", () => runFind(true));
  root.querySelector(".find-close")?.addEventListener("click", () => closeFindBar());
  input?.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      ev.preventDefault();
      closeFindBar();
    } else if (ev.key === "Enter") {
      ev.preventDefault();
      runFind(ev.shiftKey);
    }
  });
}

function runFind(backwards: boolean): void {
  const q = input?.value ?? "";
  if (!target) return;
  if (target.kind === "terminal") {
    target.findNext(q, backwards);
  } else if (target.kind === "editor") {
    target.open();
  }
}

export function openFindBar(): void {
  ensureDom();
  if (!root) return;
  if (target?.kind === "editor") {
    target.open();
    return;
  }
  root.hidden = false;
  input?.focus();
  input?.select();
}

export function closeFindBar(): void {
  if (target?.kind === "terminal") target.clear();
  if (root) root.hidden = true;
}

export function isFindBarOpen(): boolean {
  return !!root && !root.hidden;
}
