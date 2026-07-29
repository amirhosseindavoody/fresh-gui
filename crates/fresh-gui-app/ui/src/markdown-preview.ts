import { marked } from "marked";
import DOMPurify from "dompurify";

marked.setOptions({
  gfm: true,
  breaks: false,
});

export function isMarkdownPath(path: string): boolean {
  const lower = path.toLowerCase();
  return lower.endsWith(".md") || lower.endsWith(".markdown") || lower.endsWith(".mdx");
}

/** Parse markdown to sanitized HTML for the preview pane. */
export function renderMarkdownHtml(source: string): string {
  const raw = marked.parse(source, { async: false }) as string;
  return DOMPurify.sanitize(raw, {
    USE_PROFILES: { html: true },
    ADD_ATTR: ["target", "rel"],
  });
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
}
