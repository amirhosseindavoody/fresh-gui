import { marked, Renderer } from "marked";
import DOMPurify from "dompurify";
import katex from "katex";
import "katex/dist/katex.min.css";

type MathSlot = { tex: string; display: boolean };

const renderer = new Renderer();
const defaultCode = renderer.code.bind(renderer);

renderer.code = (token) => {
  if ((token.lang ?? "").trim().toLowerCase() === "mermaid") {
    return `<pre class="mermaid" data-md-source="${escapeAttr(token.text)}" contenteditable="false">${escapeHtml(token.text)}</pre>\n`;
  }
  return defaultCode(token);
};

marked.setOptions({
  gfm: true,
  breaks: false,
  renderer,
});

let mermaidTheme: "dark" | "default" | null = null;
let mermaidModule: typeof import("mermaid") | null = null;

export function isMarkdownPath(path: string): boolean {
  const lower = path.toLowerCase();
  return lower.endsWith(".md") || lower.endsWith(".markdown") || lower.endsWith(".mdx");
}

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function escapeAttr(s: string): string {
  return escapeHtml(s).replace(/'/g, "&#39;");
}

function resolvePreviewTheme(): "dark" | "light" {
  return document.documentElement.getAttribute("data-theme") === "light" ? "light" : "dark";
}

/**
 * Protect fenced/inline code, then replace TeX with placeholders so `$` in code
 * is not treated as math.
 */
function extractMath(source: string): { text: string; math: MathSlot[] } {
  const math: MathSlot[] = [];
  const protectedChunks: string[] = [];

  const protect = (chunk: string): string => {
    const i = protectedChunks.length;
    protectedChunks.push(chunk);
    return `@@@@FRESH_CODE_${i}@@@@`;
  };

  let text = source.replace(/(```|~~~)([^\n]*)\n([\s\S]*?)\n\1/g, (m) => protect(m));
  text = text.replace(/`[^`\n]+`/g, (m) => protect(m));

  const pushMath = (tex: string, display: boolean): string => {
    const i = math.length;
    math.push({ tex: tex.trim(), display });
    return display ? `\n\n@@@@FRESH_MATH_${i}@@@@\n\n` : `@@@@FRESH_MATH_${i}@@@@`;
  };

  text = text.replace(/\$\$([\s\S]+?)\$\$/g, (_m, tex: string) => pushMath(tex, true));
  text = text.replace(/\\\[([\s\S]+?)\\\]/g, (_m, tex: string) => pushMath(tex, true));
  text = text.replace(/\\\(([\s\S]+?)\\\)/g, (_m, tex: string) => pushMath(tex, false));
  // $...$ but not $$; require non-space boundaries to reduce currency false positives.
  text = text.replace(
    /(?<!\$)\$(?!\$)([^\s$](?:[^$\n]*[^\s$])?)\$(?!\$)/g,
    (_m, tex: string) => pushMath(tex, false),
  );

  text = text.replace(/@@@@FRESH_CODE_(\d+)@@@@/g, (_m, i: string) => protectedChunks[Number(i)] ?? "");
  return { text, math };
}

function injectMathHtml(html: string, math: MathSlot[]): string {
  return html.replace(/@@@@FRESH_MATH_(\d+)@@@@/g, (_m, i: string) => {
    const slot = math[Number(i)];
    if (!slot) return "";
    const display = slot.display ? "1" : "0";
    const cls = slot.display ? "md-math md-math-display" : "md-math";
    try {
      const rendered = katex.renderToString(slot.tex, {
        displayMode: slot.display,
        throwOnError: false,
        output: "html",
        strict: "ignore",
      });
      // Stamp TeX so WYSIWYG can round-trip without parsing KaTeX HTML.
      return `<span class="${cls}" data-md-math="${escapeAttr(slot.tex)}" data-md-display="${display}" contenteditable="false">${rendered}</span>`;
    } catch {
      return `<code class="md-math-error" data-md-math="${escapeAttr(slot.tex)}">${escapeHtml(slot.tex)}</code>`;
    }
  });
}

/** Parse markdown to sanitized HTML (math placeholders already expanded to KaTeX). */
export function renderMarkdownHtml(source: string): string {
  const { text, math } = extractMath(source);
  const raw = marked.parse(text, { async: false }) as string;
  const safe = DOMPurify.sanitize(raw, {
    USE_PROFILES: { html: true },
    ADD_ATTR: ["target", "rel", "class", "contenteditable", "data-md-math", "data-md-display", "data-md-source"],
  });
  return injectMathHtml(safe, math);
}

async function ensureMermaid(): Promise<typeof import("mermaid").default> {
  if (!mermaidModule) {
    mermaidModule = await import("mermaid");
  }
  const api = mermaidModule.default;
  const next = resolvePreviewTheme() === "dark" ? "dark" : "default";
  if (mermaidTheme !== next) {
    api.initialize({
      startOnLoad: false,
      securityLevel: "strict",
      theme: next,
      fontFamily: "var(--font-ui)",
    });
    mermaidTheme = next;
  }
  return api;
}

async function renderMermaidBlocks(root: HTMLElement): Promise<void> {
  const nodes = [...root.querySelectorAll<HTMLElement>("pre.mermaid")];
  if (nodes.length === 0) return;
  for (const node of nodes) {
    // Preserve fence body for WYSIWYG → markdown serialization after SVG replace.
    if (!node.dataset.mdSource) {
      node.dataset.mdSource = node.textContent ?? "";
    }
    node.contentEditable = "false";
  }
  try {
    const api = await ensureMermaid();
    // Mermaid mutates nodes in place; ignore individual diagram failures.
    await api.run({ nodes });
  } catch (err) {
    for (const node of nodes) {
      if (node.querySelector("svg")) continue;
      node.classList.add("md-mermaid-error");
      node.setAttribute(
        "title",
        err instanceof Error ? err.message : "Mermaid render failed",
      );
    }
  }
}

/** Refresh preview DOM from markdown source; external links open in a new tab. */
export function updateMarkdownPreview(el: HTMLElement, source: string): void {
  el.innerHTML = renderMarkdownHtml(source);
  for (const a of el.querySelectorAll("a[href]")) {
    if (!(a instanceof HTMLAnchorElement)) continue;
    const href = a.getAttribute("href") ?? "";
    if (/^https?:\/\//i.test(href) || href.startsWith("//")) {
      a.target = "_blank";
      a.rel = "noopener noreferrer";
    }
  }
  void renderMermaidBlocks(el);
}
