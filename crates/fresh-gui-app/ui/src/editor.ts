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
import { search, openSearchPanel, searchKeymap } from "@codemirror/search";
import { rust } from "@codemirror/lang-rust";
import { javascript } from "@codemirror/lang-javascript";
import { python } from "@codemirror/lang-python";
import { markdown } from "@codemirror/lang-markdown";
import { cpp } from "@codemirror/lang-cpp";
import { json } from "@codemirror/lang-json";
import { yaml } from "@codemirror/lang-yaml";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { shell } from "@codemirror/legacy-modes/mode/shell";
import type { Extension } from "@codemirror/state";
import type { ResolvedTheme } from "./theme";
import { logLanguage } from "./log-lang";
import { detectLinkAt } from "./path-link";

const fontSizeCompartment = new Compartment();
const themeCompartment = new Compartment();
const highlightCompartment = new Compartment();
/** Empty when minimap is off — the `@replit/codemirror-minimap` chunk is never loaded. */
const minimapCompartment = new Compartment();
/** Soft wrap (Fresh `editor.line_wrap` / CodeMirror `EditorView.lineWrapping`). */
const lineWrapCompartment = new Compartment();

const SHELL_LANG = StreamLanguage.define(shell);

/** Common shell config basenames (Fresh also maps these via shebang / grammar). */
const SHELL_BASENAMES = new Set([
  ".bashrc",
  ".bash_profile",
  ".bash_login",
  ".bash_logout",
  ".zshrc",
  ".zprofile",
  ".zlogin",
  ".zshenv",
  ".profile",
  "profile",
]);

function isShellLanguageHint(language?: string | null): boolean {
  if (!language) return false;
  const lower = language.trim().toLowerCase();
  return (
    lower === "bash" ||
    lower === "shell" ||
    lower === "sh" ||
    lower === "zsh" ||
    lower === "fish" ||
    lower === "ksh" ||
    lower === "shellscript"
  );
}

/** True when the first line is a shell shebang (`#!/bin/bash`, `#!/usr/bin/env zsh`, …). */
export function hasShellShebang(text: string): boolean {
  const nl = text.indexOf("\n");
  const first = (nl === -1 ? text : text.slice(0, nl)).trim();
  if (!first.startsWith("#!")) return false;
  return /\b(?:ba|da|a|k|mk|pd)?sh\b|\bzsh\b|\bfish\b/.test(first);
}

/**
 * Editor chrome + syntax highlight from CSS tokens (`tokens.css`).
 * Colors use `var(--*)` so `data-theme` swaps update without rebundling themes.
 */
function tokenEditorTheme(fontSize: number, fontWeight = 400): Extension {
  return EditorView.theme({
    "&": {
      height: "100%",
      fontSize: `${fontSize}px`,
      fontWeight: String(fontWeight),
      backgroundColor: "var(--editor-bg)",
      color: "var(--editor-fg)",
    },
    ".cm-scroller": {
      overflow: "auto",
      fontFamily: "var(--font-mono)",
      fontWeight: "var(--font-mono-weight)",
      backgroundColor: "var(--editor-bg)",
    },
    ".cm-content": { caretColor: "var(--editor-cursor)" },
    "&.cm-focused .cm-cursor": { borderLeftColor: "var(--editor-cursor)" },
    ".cm-gutters": {
      backgroundColor: "var(--editor-gutter-bg)",
      color: "var(--editor-gutter-fg)",
      borderRight: "1px solid var(--border)",
      fontWeight: "var(--font-mono-weight)",
    },
    ".cm-activeLine": { backgroundColor: "var(--editor-active-line)" },
    ".cm-activeLineGutter": { backgroundColor: "var(--editor-active-line)" },
    ".cm-selectionBackground, &.cm-focused .cm-selectionBackground": {
      backgroundColor: "var(--editor-selection) !important",
    },
    ".cm-selectionMatch": { backgroundColor: "var(--editor-matching-bracket)" },
    "&.cm-focused .cm-matchingBracket": {
      backgroundColor: "var(--editor-matching-bracket)",
      outline: "1px solid var(--border-strong)",
    },
    ".cm-path-link": {
      textDecoration: "underline",
      textUnderlineOffset: "2px",
      cursor: "pointer",
    },
  });
}

function tokenHighlightStyle(): Extension {
  return syntaxHighlighting(
    HighlightStyle.define([
      { tag: t.keyword, color: "var(--syn-keyword)" },
      { tag: t.controlKeyword, color: "var(--syn-control)" },
      { tag: t.operatorKeyword, color: "var(--syn-operator)" },
      { tag: t.definitionKeyword, color: "var(--syn-keyword)" },
      { tag: t.moduleKeyword, color: "var(--syn-keyword)" },
      { tag: t.comment, color: "var(--syn-comment)", fontStyle: "italic" },
      { tag: t.docComment, color: "var(--syn-comment)", fontStyle: "italic" },
      { tag: t.string, color: "var(--syn-string)" },
      { tag: t.character, color: "var(--syn-string)" },
      { tag: t.special(t.string), color: "var(--syn-string)" },
      { tag: t.number, color: "var(--syn-number)" },
      { tag: t.bool, color: "var(--syn-bool)" },
      { tag: t.null, color: "var(--syn-bool)" },
      { tag: t.regexp, color: "var(--syn-string)" },
      { tag: t.escape, color: "var(--syn-string)" },
      { tag: t.variableName, color: "var(--syn-variable)" },
      { tag: t.definition(t.variableName), color: "var(--syn-type)" },
      { tag: t.function(t.variableName), color: "var(--syn-function)" },
      { tag: t.function(t.propertyName), color: "var(--syn-function)" },
      { tag: t.propertyName, color: "var(--syn-property)" },
      { tag: t.typeName, color: "var(--syn-type)" },
      { tag: t.className, color: "var(--syn-type)" },
      { tag: t.namespace, color: "var(--syn-type)" },
      { tag: t.macroName, color: "var(--syn-type)" },
      { tag: t.labelName, color: "var(--syn-property)" },
      { tag: t.attributeName, color: "var(--syn-attribute)" },
      { tag: t.attributeValue, color: "var(--syn-string)" },
      { tag: t.heading, color: "var(--syn-heading)", fontWeight: "bold" },
      { tag: t.heading1, color: "var(--syn-heading)", fontWeight: "bold" },
      { tag: t.heading2, color: "var(--syn-heading)", fontWeight: "bold" },
      { tag: t.heading3, color: "var(--syn-heading)", fontWeight: "bold" },
      { tag: t.url, color: "var(--syn-link)", textDecoration: "underline" },
      { tag: t.link, color: "var(--syn-link)" },
      { tag: t.emphasis, fontStyle: "italic" },
      { tag: t.strong, fontWeight: "bold" },
      { tag: t.strikethrough, textDecoration: "line-through" },
      { tag: t.meta, color: "var(--syn-meta)" },
      { tag: t.invalid, color: "var(--syn-invalid)" },
      { tag: t.tagName, color: "var(--syn-tag)" },
      { tag: t.angleBracket, color: "var(--syn-punctuation)" },
      { tag: t.operator, color: "var(--syn-operator)" },
      { tag: t.punctuation, color: "var(--syn-meta)" },
      { tag: t.bracket, color: "var(--syn-punctuation)" },
      { tag: t.monospace, color: "var(--syn-variable)" },
      { tag: t.contentSeparator, color: "var(--syn-meta)" },
      { tag: t.processingInstruction, color: "var(--syn-meta)" },
    ]),
  );
}

function langForPath(
  path: string,
  opts: { text?: string; language?: string | null } = {},
): Extension | null {
  const lower = path.toLowerCase();
  const base = lower.includes("/") ? lower.slice(lower.lastIndexOf("/") + 1) : lower;

  if (isShellLanguageHint(opts.language)) return SHELL_LANG;
  if (
    lower.endsWith(".sh") ||
    lower.endsWith(".bash") ||
    lower.endsWith(".zsh") ||
    lower.endsWith(".fish") ||
    lower.endsWith(".ksh") ||
    lower.endsWith(".bats") ||
    SHELL_BASENAMES.has(base) ||
    (opts.text != null && hasShellShebang(opts.text))
  ) {
    return SHELL_LANG;
  }

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

export type EditorPathLinkHandler = (info: {
  path: string;
  line?: number;
  column?: number;
}) => void;

function pathLinkClickHandler(onPathLink?: EditorPathLinkHandler): Extension {
  return EditorView.domEventHandlers({
    click(event, view) {
      if (!(event.ctrlKey || event.metaKey) || event.altKey || event.shiftKey || !onPathLink) {
        return false;
      }
      const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
      if (pos == null) return false;
      const line = view.state.doc.lineAt(pos);
      const col = pos - line.from;
      const link = detectLinkAt(line.text, col);
      if (!link) return false;
      event.preventDefault();
      onPathLink({ path: link.path, line: link.line, column: link.column });
      return true;
    },
  });
}

/** Jump the cursor to 1-based line/column and scroll it into view. */
export function revealEditorLocation(
  view: EditorView,
  line?: number | null,
  column?: number | null,
): void {
  if (line == null || line < 1) return;
  const doc = view.state.doc;
  const lineNo = Math.min(line, doc.lines);
  const lineObj = doc.line(lineNo);
  const col = Math.max(0, (column ?? 1) - 1);
  const pos = Math.min(lineObj.from + col, lineObj.to);
  view.dispatch({
    selection: { anchor: pos, head: pos },
    effects: EditorView.scrollIntoView(pos, { y: "center" }),
  });
  view.focus();
}

/** Replace the full document text (used by markdown WYSIWYG → source sync). */
export function setEditorDocument(view: EditorView, text: string): void {
  const cur = view.state.doc.toString();
  if (cur === text) return;
  view.dispatch({
    changes: { from: 0, to: view.state.doc.length, insert: text },
  });
}

export function createEditorView(
  parent: HTMLElement,
  text: string,
  path: string,
  onDocChange: () => void,
  opts: {
    fontSize?: number;
    fontWeight?: number;
    theme?: ResolvedTheme;
    language?: string | null;
    /** When true, dynamically load and mount the document map (minimap). */
    minimap?: boolean;
    /**
     * Soft-wrap long lines (Fresh `editor.line_wrap`). Default matches Fresh: on.
     */
    lineWrap?: boolean;
    onPathLink?: EditorPathLinkHandler;
  } = {},
): EditorView {
  const fontSize = opts.fontSize ?? 14;
  const fontWeight = opts.fontWeight ?? 400;
  const lineWrap = opts.lineWrap !== false;
  const lang = langForPath(path, { text, language: opts.language });
  const extensions: Extension[] = [
    lineNumbers(),
    highlightActiveLine(),
    history(),
    search(),
    keymap.of([...defaultKeymap, ...historyKeymap, ...searchKeymap, indentWithTab]),
    syntaxHighlighting(defaultHighlightStyle, { fallback: true }),
    fontSizeCompartment.of(tokenEditorTheme(fontSize, fontWeight)),
    themeCompartment.of([]),
    highlightCompartment.of(tokenHighlightStyle()),
    // Always present so we can enable later without recreating the editor;
    // stays empty (no work) until `applyEditorMinimap(view, true)`.
    minimapCompartment.of([]),
    lineWrapCompartment.of(lineWrap ? EditorView.lineWrapping : []),
    EditorView.updateListener.of((update) => {
      if (update.docChanged) onDocChange();
    }),
    pathLinkClickHandler(opts.onPathLink),
  ];
  if (lang) extensions.push(lang);
  void opts.theme;

  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc: text,
      extensions,
    }),
  });
  if (opts.minimap) {
    void applyEditorMinimap(view, true);
  }
  return view;
}

/**
 * Toggle soft wrap (Fresh `ToggleLineWrap` / CodeMirror `lineWrapping`).
 */
export function applyEditorLineWrap(view: EditorView, enabled: boolean): void {
  view.dispatch({
    effects: lineWrapCompartment.reconfigure(enabled ? EditorView.lineWrapping : []),
  });
}

/**
 * Toggle the VS Code–style document map. Enabling dynamic-imports the minimap
 * chunk; disabling clears the compartment so the plugin DOM is torn down.
 */
export async function applyEditorMinimap(view: EditorView, enabled: boolean): Promise<void> {
  if (!enabled) {
    view.dispatch({
      effects: minimapCompartment.reconfigure([]),
    });
    return;
  }
  const { createMinimapExtension } = await import("./editor-minimap");
  // View may have been destroyed while the chunk was loading.
  if (!view.dom.isConnected) return;
  view.dispatch({
    effects: minimapCompartment.reconfigure(createMinimapExtension()),
  });
}

export function openEditorSearch(view: EditorView): void {
  openSearchPanel(view);
}

export function applyEditorFontSize(
  view: EditorView,
  fontSize: number,
  fontWeight = 400,
): void {
  view.dispatch({
    effects: fontSizeCompartment.reconfigure(tokenEditorTheme(fontSize, fontWeight)),
  });
}

/** Theme colors follow `data-theme` via CSS vars; keep API for restyle hooks. */
export function applyEditorTheme(view: EditorView, _theme: ResolvedTheme): void {
  view.dispatch({
    effects: [
      themeCompartment.reconfigure([]),
      highlightCompartment.reconfigure(tokenHighlightStyle()),
    ],
  });
}
