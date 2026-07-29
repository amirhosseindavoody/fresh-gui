import { EditorState, Compartment } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { syntaxHighlighting, defaultHighlightStyle } from "@codemirror/language";
import { oneDark } from "@codemirror/theme-one-dark";
import { search, openSearchPanel, searchKeymap } from "@codemirror/search";
import { rust } from "@codemirror/lang-rust";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { markdown } from "@codemirror/lang-markdown";
import type { Extension } from "@codemirror/state";
import type { ResolvedTheme } from "./theme";

const fontSizeCompartment = new Compartment();
const themeCompartment = new Compartment();

function langForPath(path: string): Extension | null {
  const lower = path.toLowerCase();
  if (lower.endsWith(".rs")) return rust();
  if (
    lower.endsWith(".js") ||
    lower.endsWith(".ts") ||
    lower.endsWith(".mjs") ||
    lower.endsWith(".cjs") ||
    lower.endsWith(".jsx") ||
    lower.endsWith(".tsx")
  ) {
    return javascript({ typescript: lower.endsWith(".ts") || lower.endsWith(".tsx") });
  }
  if (lower.endsWith(".py")) return python();
  if (lower.endsWith(".md") || lower.endsWith(".markdown")) return markdown();
  return null;
}

function editorThemeExtension(theme: ResolvedTheme): Extension {
  return theme === "dark" ? oneDark : [];
}

export function createEditorView(
  parent: HTMLElement,
  text: string,
  path: string,
  onDocChange: () => void,
  opts: { fontSize?: number; theme?: ResolvedTheme } = {},
): EditorView {
  const fontSize = opts.fontSize ?? 14;
  const theme = opts.theme ?? "dark";
  const lang = langForPath(path);
  const extensions: Extension[] = [
    lineNumbers(),
    highlightActiveLine(),
    history(),
    search(),
    keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab]),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    fontSizeCompartment.of(EditorView.theme({
      "&": { height: "100%", fontSize: `${fontSize}px` },
      ".cm-scroller": { overflow: "auto", fontFamily: "var(--font-mono)" },
    })),
    themeCompartment.of(editorThemeExtension(theme)),
    EditorView.updateListener.of((update) => {
      if (update.docChanged) onDocChange();
    }),
  ];
  if (lang) extensions.push(lang);

  return new EditorView({
    parent,
    state: EditorState.create({
      doc: text,
      extensions,
    }),
  });
}

export function openEditorSearch(view: EditorView): void {
  openSearchPanel(view);
}

export function applyEditorFontSize(view: EditorView, fontSize: number): void {
  view.dispatch({
    effects: fontSizeCompartment.reconfigure(
      EditorView.theme({
        "&": { height: "100%", fontSize: `${fontSize}px` },
        ".cm-scroller": { overflow: "auto", fontFamily: "var(--font-mono)" },
      }),
    ),
  });
}

export function applyEditorTheme(view: EditorView, theme: ResolvedTheme): void {
  view.dispatch({
    effects: themeCompartment.reconfigure(editorThemeExtension(theme)),
  });
}
