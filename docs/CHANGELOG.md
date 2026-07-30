# Changelog

## 2026-07-30

### Settings and shortkeys cleanup (#61)

- Embedded default settings catalog at `crates/fresh-gui/defaults/config.json` (JSONC with field comments), compiled into the binary via `include_str!` (Fresh keymap pattern).
- `config.json` gains a `shortkeys` list: `{ action, shortkey, when }` (Fresh-style action + context; chord as `shortkey` with `Mod` = Cmd/Ctrl). Effective bindings ship in `Hello.shortkeys` (protocol `0.5.0`).
- Command palette **Preferences: Open Default Settings** opens a temporary catalog file that is deleted when the tab closes; save is rejected (copy into user config to customize).
- Host `shortcuts.ts` loads bindings from Hello / saved config and honors `when` (`global` | `terminal` | `editor` | `fileExplorer`). Terminal copy-vs-SIGINT stays selection-aware in the clipboard handler (not a when-clause), matching Fresh.

### Markdown WYSIWYG performance (#52)

- Stopped serializing HTML→markdown and rewriting the CodeMirror document on every keystroke; the preview DOM is the live buffer while WYSIWYG is open, and Turndown runs only on save / toggle-to-source / theme refresh.
- Dirty/pin chrome updates once on first edit instead of re-rendering the tab strip after each debounced sync.
- Disabled spellcheck on the preview (large KaTeX/Mermaid HTML was a major lag source) and removed the focus inset shadow that repainted the scroll surface.
- Serialization no longer `cloneNode`s Mermaid SVGs before Turndown.

### Session state persistence (#55)

- Rust `SessionStore` remains the source of truth for live session UI layout via `layout_set` / attach (host `localStorage` is a cache). Layout blob is now **v4**.
- Reattach / reload / `/?token=` restores **multiple** terminal tabs (and splits) by partitioning live PTY ids, reopens **editor** tabs by path, and stamps leaf **cwd** into the pane tree.
- Explorer expanded folders + scroll are kept **per view-root cwd** when switching terminals (and persisted in the layout blob). Semantics follow Fresh `FileExplorerState`; Fresh workspace disk files are not used on the ADE path.
- `ui.fontWeight` / chrome typography apply immediately on settings save (CSS vars on tabs, sidebar, tree, status) without a page reload; terminal glyph refresh also runs after mono weight changes.
- Explicit `fresh-gui close` still ends the daemon and clears session persistence.

### Markdown WYSIWYG (#52)

- Markdown preview mode (`Mod+Shift+V`) is now an editable WYSIWYG surface with a formatting toolbar (bold/italic/strike/code, headings, lists, quote, code block, link, HR).
- Edits stay in the preview DOM while WYSIWYG is open; GFM serialization (Turndown + GFM plugin) runs on save / show-source / theme refresh and then updates the CodeMirror buffer for ADE CAS.
- KaTeX and Mermaid blocks stay atomic in the preview; double-click edits their source. Task-list checkboxes are interactive.
- Keyboard shortcuts in the WYSIWYG surface: `Mod+B` / `Mod+I` / `Mod+K` / `Mod+Shift+E` (inline code).
- Fresh’s Compose/Page View plugin was checked first; it is not available on the ADE path (`runtime` only, plugins off, terminal-cell rendering), so the host preview owns this UX.
