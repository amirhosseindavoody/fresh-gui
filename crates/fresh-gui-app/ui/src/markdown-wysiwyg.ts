/**
 * WYSIWYG editing for the markdown preview surface.
 *
 * Fresh ships Typora-style Compose/Page View (`markdown_compose` plugin), but
 * fresh-gui links Fresh with only `runtime` (plugins off) and the host owns
 * interactive editing in CodeMirror / this preview DOM — so editing happens
 * here and serializes back to markdown for the CodeMirror → ADE save path.
 */

import TurndownService from "turndown";
import { gfm } from "turndown-plugin-gfm";
import { updateMarkdownPreview } from "./markdown-preview";

export type MarkdownWysiwygHandle = {
  /** Re-render from markdown source (resets caret). */
  refresh: (source: string) => void;
  focus: () => void;
  /** Flush debounced edits into `onChange` immediately. */
  flush: () => void;
  destroy: () => void;
};

export type MarkdownWysiwygOptions = {
  /** Called with markdown after local edits (debounced). */
  onChange: (markdown: string) => void;
};

const SYNC_MS = 220;

function escapeAttr(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/"/g, "&quot;").replace(/</g, "&lt;");
}

function createTurndown(): TurndownService {
  const td = new TurndownService({
    headingStyle: "atx",
    codeBlockStyle: "fenced",
    bulletListMarker: "-",
    emDelimiter: "*",
    strongDelimiter: "**",
    hr: "---",
    fence: "```",
  });
  td.use(gfm);

  td.addRule("mdMath", {
    filter: (node) =>
      node.nodeName === "SPAN" && (node as HTMLElement).classList.contains("md-math"),
    replacement: (_content, node) => {
      const el = node as HTMLElement;
      const tex = el.getAttribute("data-md-math") ?? "";
      if (!tex) return "";
      return el.getAttribute("data-md-display") === "1" ? `\n\n$$\n${tex}\n$$\n\n` : `$${tex}$`;
    },
  });

  td.addRule("mdMathError", {
    filter: (node) =>
      node.nodeName === "CODE" && (node as HTMLElement).classList.contains("md-math-error"),
    replacement: (_content, node) => {
      const tex = (node.textContent ?? "").trim();
      return tex ? `$${tex}$` : "";
    },
  });

  td.addRule("mermaid", {
    filter: (node) =>
      node.nodeName === "PRE" && (node as HTMLElement).classList.contains("mermaid"),
    replacement: (_content, node) => {
      const src = (node as HTMLElement).getAttribute("data-md-source") ?? "";
      return `\n\n\`\`\`mermaid\n${src}\n\`\`\`\n\n`;
    },
  });

  return td;
}

const turndown = createTurndown();

/** Serialize a preview DOM subtree to GFM markdown. */
export function htmlToMarkdown(root: HTMLElement): string {
  const clone = root.cloneNode(true) as HTMLElement;
  // Drop mermaid error chrome; keep data-md-source via the mermaid rule.
  for (const err of clone.querySelectorAll(".md-mermaid-error")) {
    err.classList.remove("md-mermaid-error");
    err.removeAttribute("title");
  }
  const md = turndown.turndown(clone);
  return md.replace(/\n{3,}/g, "\n\n").trimEnd() + (md.trim() ? "\n" : "");
}

function exec(command: string, value?: string): void {
  // execCommand remains the practical API for lightweight contenteditable toolbars.
  document.execCommand(command, false, value);
}

function selectionInside(root: HTMLElement): boolean {
  const sel = window.getSelection();
  if (!sel || sel.rangeCount === 0) return false;
  const node = sel.anchorNode;
  return !!node && root.contains(node);
}

function focusPreview(previewEl: HTMLElement): void {
  previewEl.focus();
}

function wrapInline(tag: "code" | "strong" | "em" | "del"): void {
  const sel = window.getSelection();
  if (!sel || sel.rangeCount === 0 || sel.isCollapsed) {
    if (tag === "code") exec("insertHTML", "<code>\u200b</code>");
    else if (tag === "strong") exec("bold");
    else if (tag === "em") exec("italic");
    else exec("strikeThrough");
    return;
  }
  const text = sel.toString();
  const safe = text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
  exec("insertHTML", `<${tag}>${safe}</${tag}>`);
}

function promptLink(): void {
  const sel = window.getSelection();
  const selected = sel?.toString() ?? "";
  const href = window.prompt("Link URL", "https://");
  if (!href) return;
  if (selected) {
    exec("createLink", href);
    return;
  }
  const label = window.prompt("Link text", href) ?? href;
  const safeLabel = label.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  const safeHref = escapeAttr(href);
  exec("insertHTML", `<a href="${safeHref}">${safeLabel}</a>`);
}

function editMathNode(
  el: HTMLElement,
  previewEl: HTMLElement,
  commit: (markdown: string) => void,
  prepareInteractive: () => void,
): void {
  const display = el.getAttribute("data-md-display") === "1";
  const current = el.getAttribute("data-md-math") ?? "";
  const next = window.prompt(display ? "Edit display math (TeX)" : "Edit inline math (TeX)", current);
  if (next == null) return;
  el.setAttribute("data-md-math", next.trim());
  const md = htmlToMarkdown(previewEl);
  commit(md);
  updateMarkdownPreview(previewEl, md);
  prepareInteractive();
}

function editMermaidNode(
  el: HTMLElement,
  previewEl: HTMLElement,
  commit: (markdown: string) => void,
  prepareInteractive: () => void,
): void {
  const current = el.getAttribute("data-md-source") ?? "";
  const next = window.prompt("Edit Mermaid diagram source", current);
  if (next == null) return;
  el.setAttribute("data-md-source", next);
  const md = htmlToMarkdown(previewEl);
  commit(md);
  updateMarkdownPreview(previewEl, md);
  prepareInteractive();
}

type ToolDef =
  | { kind: "sep" }
  | {
      kind: "btn";
      label: string;
      title: string;
      run: () => void;
    };

function buildToolbar(previewEl: HTMLElement, after: () => void): HTMLElement {
  const bar = document.createElement("div");
  bar.className = "md-toolbar";
  bar.setAttribute("role", "toolbar");
  bar.setAttribute("aria-label", "Markdown formatting");

  const run = (fn: () => void) => {
    focusPreview(previewEl);
    fn();
    after();
  };

  const tools: ToolDef[] = [
    { kind: "btn", label: "B", title: "Bold (Mod+B)", run: () => exec("bold") },
    { kind: "btn", label: "I", title: "Italic (Mod+I)", run: () => exec("italic") },
    { kind: "btn", label: "S", title: "Strikethrough", run: () => exec("strikeThrough") },
    { kind: "btn", label: "</>", title: "Inline code", run: () => wrapInline("code") },
    { kind: "sep" },
    { kind: "btn", label: "H1", title: "Heading 1", run: () => exec("formatBlock", "H1") },
    { kind: "btn", label: "H2", title: "Heading 2", run: () => exec("formatBlock", "H2") },
    { kind: "btn", label: "H3", title: "Heading 3", run: () => exec("formatBlock", "H3") },
    { kind: "btn", label: "P", title: "Paragraph", run: () => exec("formatBlock", "P") },
    { kind: "sep" },
    {
      kind: "btn",
      label: "•",
      title: "Bullet list",
      run: () => exec("insertUnorderedList"),
    },
    {
      kind: "btn",
      label: "1.",
      title: "Numbered list",
      run: () => exec("insertOrderedList"),
    },
    { kind: "btn", label: ">", title: "Quote", run: () => exec("formatBlock", "BLOCKQUOTE") },
    {
      kind: "btn",
      label: "{ }",
      title: "Code block",
      run: () => exec("formatBlock", "PRE"),
    },
    { kind: "sep" },
    { kind: "btn", label: "Link", title: "Insert link (Mod+K)", run: () => promptLink() },
    { kind: "btn", label: "HR", title: "Horizontal rule", run: () => exec("insertHorizontalRule") },
  ];

  for (const tool of tools) {
    if (tool.kind === "sep") {
      const sep = document.createElement("span");
      sep.className = "md-toolbar-sep";
      sep.setAttribute("aria-hidden", "true");
      bar.appendChild(sep);
      continue;
    }
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "md-toolbar-btn";
    btn.title = tool.title;
    btn.setAttribute("aria-label", tool.title);
    btn.textContent = tool.label;
    btn.addEventListener("mousedown", (e) => {
      // Keep selection in the preview.
      e.preventDefault();
    });
    btn.addEventListener("click", () => run(tool.run));
    bar.appendChild(btn);
  }

  return bar;
}

/**
 * Wrap a preview element with a formatting toolbar and enable contenteditable
 * sync back to markdown via `onChange`.
 */
export function mountMarkdownWysiwyg(
  previewEl: HTMLElement,
  opts: MarkdownWysiwygOptions,
): MarkdownWysiwygHandle {
  const parent = previewEl.parentElement;
  if (!parent) {
    throw new Error("markdown preview element has no parent");
  }

  const shell = document.createElement("div");
  shell.className = "md-wysiwyg";
  parent.insertBefore(shell, previewEl);

  let timer: ReturnType<typeof setTimeout> | null = null;
  let destroyed = false;
  let applyingRemote = false;
  let lastEmitted = "";

  const emitNow = (): void => {
    if (destroyed || applyingRemote) return;
    const md = htmlToMarkdown(previewEl);
    if (md === lastEmitted) return;
    lastEmitted = md;
    opts.onChange(md);
  };

  const commitMarkdown = (md: string): void => {
    if (destroyed) return;
    if (md === lastEmitted) return;
    lastEmitted = md;
    opts.onChange(md);
  };

  const scheduleSync = (): void => {
    if (destroyed || applyingRemote) return;
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      emitNow();
    }, SYNC_MS);
  };

  const toolbar = buildToolbar(previewEl, scheduleSync);
  shell.appendChild(toolbar);
  shell.appendChild(previewEl);

  previewEl.contentEditable = "true";
  previewEl.spellcheck = true;
  previewEl.tabIndex = 0;
  previewEl.setAttribute("role", "textbox");
  previewEl.setAttribute("aria-multiline", "true");
  previewEl.setAttribute("aria-label", "Markdown WYSIWYG editor");
  previewEl.classList.add("md-preview-editable");

  const prepareInteractive = (): void => {
    for (const input of previewEl.querySelectorAll('input[type="checkbox"]')) {
      if (input instanceof HTMLInputElement) {
        input.disabled = false;
        input.contentEditable = "false";
      }
    }
    for (const math of previewEl.querySelectorAll(".md-math")) {
      if (math instanceof HTMLElement) {
        math.contentEditable = "false";
        math.title = "Double-click to edit TeX";
      }
    }
    for (const pre of previewEl.querySelectorAll("pre.mermaid")) {
      if (pre instanceof HTMLElement) {
        pre.contentEditable = "false";
        pre.title = "Double-click to edit Mermaid source";
      }
    }
  };

  const onInput = (): void => scheduleSync();
  const onCheckbox = (e: Event): void => {
    const t = e.target;
    if (!(t instanceof HTMLInputElement) || t.type !== "checkbox") return;
    scheduleSync();
  };

  const onClick = (e: MouseEvent): void => {
    const t = e.target;
    if (!(t instanceof Element)) return;
    const anchor = t.closest("a");
    if (anchor && previewEl.contains(anchor)) {
      // Edit mode: don't navigate; Mod/Ctrl-click still opens.
      if (!(e.metaKey || e.ctrlKey)) {
        e.preventDefault();
      }
    }
  };

  const onDblClick = (e: MouseEvent): void => {
    const t = e.target;
    if (!(t instanceof Element)) return;
    const math = t.closest(".md-math");
    if (math instanceof HTMLElement && previewEl.contains(math)) {
      e.preventDefault();
      applyingRemote = true;
      editMathNode(math, previewEl, commitMarkdown, prepareInteractive);
      applyingRemote = false;
      return;
    }
    const mermaid = t.closest("pre.mermaid");
    if (mermaid instanceof HTMLElement && previewEl.contains(mermaid)) {
      e.preventDefault();
      applyingRemote = true;
      editMermaidNode(mermaid, previewEl, commitMarkdown, prepareInteractive);
      applyingRemote = false;
    }
  };

  const onKeyDown = (e: KeyboardEvent): void => {
    if (!selectionInside(previewEl) && e.target !== previewEl) return;
    const mod = e.metaKey || e.ctrlKey;
    if (!mod) return;
    const key = e.key.toLowerCase();
    if (key === "b") {
      e.preventDefault();
      e.stopPropagation();
      exec("bold");
      scheduleSync();
    } else if (key === "i") {
      e.preventDefault();
      e.stopPropagation();
      exec("italic");
      scheduleSync();
    } else if (key === "k") {
      e.preventDefault();
      e.stopPropagation();
      promptLink();
      scheduleSync();
    } else if (key === "e" && e.shiftKey) {
      e.preventDefault();
      e.stopPropagation();
      wrapInline("code");
      scheduleSync();
    }
  };

  previewEl.addEventListener("input", onInput);
  previewEl.addEventListener("change", onCheckbox);
  previewEl.addEventListener("click", onClick);
  previewEl.addEventListener("dblclick", onDblClick);
  previewEl.addEventListener("keydown", onKeyDown);

  return {
    refresh(source: string) {
      applyingRemote = true;
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      updateMarkdownPreview(previewEl, source);
      prepareInteractive();
      lastEmitted = htmlToMarkdown(previewEl);
      applyingRemote = false;
    },
    focus() {
      focusPreview(previewEl);
    },
    flush() {
      if (timer) {
        clearTimeout(timer);
        timer = null;
      }
      emitNow();
    },
    destroy() {
      destroyed = true;
      if (timer) clearTimeout(timer);
      previewEl.removeEventListener("input", onInput);
      previewEl.removeEventListener("change", onCheckbox);
      previewEl.removeEventListener("click", onClick);
      previewEl.removeEventListener("dblclick", onDblClick);
      previewEl.removeEventListener("keydown", onKeyDown);
      previewEl.contentEditable = "false";
      previewEl.classList.remove("md-preview-editable");
      previewEl.removeAttribute("aria-multiline");
      previewEl.setAttribute("role", "document");
      previewEl.setAttribute("aria-label", "Markdown preview");
      // Unwrap shell: move preview back, remove toolbar/shell.
      if (shell.parentElement) {
        shell.parentElement.insertBefore(previewEl, shell);
        shell.remove();
      }
    },
  };
}
