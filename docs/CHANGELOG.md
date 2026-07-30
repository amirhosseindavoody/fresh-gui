# Changelog

## 2026-07-30

### Markdown WYSIWYG (#52)

- Markdown preview mode (`Mod+Shift+V`) is now an editable WYSIWYG surface with a formatting toolbar (bold/italic/strike/code, headings, lists, quote, code block, link, HR).
- Edits serialize to GFM markdown (Turndown + GFM plugin) and update the CodeMirror buffer so save / dirty / ADE CAS keep working.
- KaTeX and Mermaid blocks stay atomic in the preview; double-click edits their source. Task-list checkboxes are interactive.
- Keyboard shortcuts in the WYSIWYG surface: `Mod+B` / `Mod+I` / `Mod+K` / `Mod+Shift+E` (inline code).
- Fresh’s Compose/Page View plugin was checked first; it is not available on the ADE path (`runtime` only, plugins off, terminal-cell rendering), so the host preview owns this UX.
