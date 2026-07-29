import { EditorState, Compartment } from "@codemirror/state";
import { EditorView, keymap, lineNumbers, highlightActiveLine } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap, indentWithTab } from "@codemirror/commands";
import {
  HighlightStyle,
  StreamLanguage,
  defaultHighlightStyle,
  syntaxHighlighting,
} from "@codemirror/language";
import { tags as t } from "@lezer/highlight";
import { oneDark } from "@codemirror/theme-one-dark";
import { search, openSearchPanel, searchKeymap } from "@codemirror/search";
import { rust } from "@codemirror/lang-rust";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { markdown } from "@codemirror/lang-markdown";
import { cpp } from "@codemirror/lang-cpp";
import { json } from "@codemirror/lang-json";
import { yaml } from "@codemirror/lang-yaml";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import type { Extension } from "@codemirror/state";
import type { ResolvedTheme } from "./theme";
import { logLanguage } from "./log-lang";

const fontSizeCompartment = new Compartment();
const themeCompartment = new Compartment();
const highlightCompartment = new Compartment();

/** Light-mode syntax colors tuned for contrast on the app light surface. */
const lightHighlightStyle = HighlightStyle.define([
  { tag: t.keyword, color: "#cf222e" },
  { tag: t.controlKeyword, color: "#cf222e" },
  { tag: t.operatorKeyword, color: "#cf222e" },
  { tag: t.definitionKeyword, color: "#cf222e" },
  { tag: t.moduleKeyword, color: "#cf222e" },
  { tag: t.comment, color: "#6e7781", fontStyle: "italic" },
  { tag: t.docComment, color: "#6e7781", fontStyle: "italic" },
  { tag: t.string, color: "#0a3069" },
  { tag: t.character, color: "#0a3069" },
  { tag: t.special(t.string), color: "#0a3069" },
  { tag: t.number, color: "#0550ae" },
  { tag: t.bool, color: "#0550ae" },
  { tag: t.null, color: "#0550ae" },
  { tag: t.regexp, color: "#0a3069" },
  { tag: t.escape, color: "#0a3069" },
  { tag: t.variableName, color: "#1f2328" },
  { tag: t.definition(t.variableName), color: "#953800" },
  { tag: t.function(t.variableName), color: "#0550ae" },
  { tag: t.function(t.propertyName), color: "#0550ae" },
  { tag: t.propertyName, color: "#0550ae" },
  { tag: t.typeName, color: "#116329" },
  { tag: t.className, color: "#116329" },
  { tag: t.namespace, color: "#116329" },
  { tag: t.macroName, color: "#953800" },
  { tag: t.labelName, color: "#0550ae" },
  { tag: t.attributeName, color: "#0550ae" },
  { tag: t.attributeValue, color: "#0a3069" },
  { tag: t.heading, color: "#0550ae", fontWeight: "bold" },
  { tag: t.heading1, color: "#0550ae", fontWeight: "bold" },
  { tag: t.heading2, color: "#0550ae", fontWeight: "bold" },
  { tag: t.heading3, color: "#0550ae", fontWeight: "bold" },
  { tag: t.url, color: "#0969da", textDecoration: "underline" },
  { tag: t.link, color: "#0969da" },
  { tag: t.emphasis, fontStyle: "italic" },
  { tag: t.strong, fontWeight: "bold" },
  { tag: t.strikethrough, textDecoration: "line-through" },
  { tag: t.meta, color: "#656d76" },
  { tag: t.invalid, color: "#cf222e" },
  { tag: t.tagName, color: "#116329" },
  { tag: t.angleBracket, color: "#1f2328" },
  { tag: t.operator, color: "#1f2328" },
  { tag: t.punctuation, color: "#656d76" },
  { tag: t.bracket, color: "#1f2328" },
  { tag: t.monospace, color: "#1f2328" },
  { tag: t.contentSeparator, color: "#656d76" },
  { tag: t.processingInstruction, color: "#656d76" },
]);

function langForPath(path: string): Extension | null {
  const lower = path.toLowerCase();
  const base = lower.includes("/") ? lower.slice(lower.lastIndexOf("/") + 1) : lower;

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
  if (lower.endsWith(".py") || lower.endsWith(".pyi") || lower.endsWith(".pyw")) return python();
  if (lower.endsWith(".md") || lower.endsWith(".markdown") || lower.endsWith(".mdx")) {
    return markdown();
  }
  if (
    lower.endsWith(".c") ||
    lower.endsWith(".h") ||
    lower.endsWith(".cc") ||
    lower.endsWith(".cpp") ||
    lower.endsWith(".cxx") ||
    lower.endsWith(".hpp") ||
    lower.endsWith(".hxx") ||
    lower.endsWith(".hh") ||
    lower.endsWith(".inl")
  ) {
    return cpp();
  }
  if (lower.endsWith(".json")) return json();
  if (lower.endsWith(".jsonc") || lower.endsWith(".json5")) {
    return javascript({ typescript: false });
  }
  if (lower.endsWith(".yaml") || lower.endsWith(".yml")) return yaml();
  if (lower.endsWith(".toml")) return StreamLanguage.define(toml);
  if (
    lower.endsWith(".log") ||
    lower.endsWith(".out") ||
    lower.endsWith(".err") ||
    base === "logfile" ||
    base.endsWith(".log.txt")
  ) {
    return logLanguage;
  }
  return null;
}

function editorThemeExtension(theme: ResolvedTheme): Extension {
  return theme === "dark" ? oneDark : [];
}

function editorHighlightExtension(theme: ResolvedTheme): Extension {
  // oneDark ships its own highlight style; light gets an explicit high-contrast set.
  return theme === "light" ? syntaxHighlighting(lightHighlightStyle) : [];
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
    highlightCompartment.of(editorHighlightExtension(theme)),
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
    effects: [
      themeCompartment.reconfigure(editorThemeExtension(theme)),
      highlightCompartment.reconfigure(editorHighlightExtension(theme)),
    ],
  });
}
