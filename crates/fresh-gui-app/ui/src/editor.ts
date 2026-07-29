import { EditorState } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import { syntaxHighlighting, defaultHighlightStyle } from "@codemirror/language";
import { oneDark } from "@codemirror/theme-one-dark";
import { rust } from "@codemirror/lang-rust";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { markdown } from "@codemirror/lang-markdown";
import type { Extension } from "@codemirror/state";

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

export function createEditorView(
  parent: HTMLElement,
  text: string,
  path: string,
  onDocChange: () => void,
): EditorView {
  const lang = langForPath(path);
  const extensions: Extension[] = [
    lineNumbers(),
    highlightActiveLine(),
    history(),
    keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    oneDark,
    EditorView.updateListener.of((update) => {
      if (update.docChanged) onDocChange();
    }),
    EditorView.theme({
      "&": { height: "100%" },
      ".cm-scroller": { overflow: "auto" },
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
