# fresh-gui Host UI

Product UI is the **native GPUI host** (`crates/fresh-gui-app`, `pixi run gui`, release asset `fresh-gui-client-*`). Architecture and protocol: [DESIGN.md](./DESIGN.md). User overview: [README.md](../README.md).

Zed / VS Code-like chrome: activity bar, collapsible explorer, unified terminal + editor tabs, status bar, command palette. Ribbons are dense (30px title bar, 36px activity rail, 26px explorer header, 24px tab strip, 22px tree rows and status bar). The new-terminal **+** sits immediately after the last tab. Connection is silent (`--backend` printed Local access URL with `?token=`, or `ws://…/ws` + `FRESH_GUI_TOKEN`). Download the Linux or Windows client, add an SSH remote, and connect. The daemon does not serve a page.

A later browser host would be this same GPUI UI via WebAssembly. That is not in this tree. Sections 1–9 below are notes from the removed Vite/React shell (CodeMirror, xterm). They are not a supported host and the source is gone. Current behavior is §10.

## 1. Goals

- **Terminal is the hero.** First paint after connect feels like a workspace of shells, not a settings form with a terminal bolted on.
- **Editor and explorer are peers:** same tab chrome, shared focus model, shared shortcuts.
- **Remote-aware chrome.** Connection, session, and capability state are first-class (unlike Terax’s local monolith).
- **Dense, calm, premium.** Intentional motion, clear hierarchy, no decorative clutter.
- **Stay dense.** Native chrome is gpui-component; the Vite path uses React + shadcn. Editor-core and PTY stay Fresh / xterm or VTE.

## 2. Borrow from Terax vs diverge

Inspired by Terax’s public UI / [TERAX.md](https://github.com/crynta/terax-ai/blob/main/TERAX.md). We borrow **structure, feel, and the React 19 + Tailwind + shadcn chrome stack**, not feature parity or Terax’s local monolith.

| Terax idea | In fresh-gui |
|------------|--------------|
| Three-zone shell: sidebar · main · optional right rail | Yes — right rail reserved (width 0) until a real panel exists |
| Unified tab bar over content stacks | Yes — tabs are `terminal` \| `editor` |
| Tabs stay mounted / hidden, not destroyed | Yes — keeps remote PTYs and xterm buffers alive |
| Recursive pane tree (row/col splits, max ~4 leaves) | Yes — per-tab `PaneNode`, max 4 leaves |
| Collapsible explorer + activity-style sidebar | Yes — Explorer + Settings (opens `config.json`) |
| Status bar | Yes — remote root / session / connection / capabilities |
| Header search that adapts to terminal vs editor | Yes — find bar / CodeMirror search |
| Sliding “pill” active-tab indicator | Yes |
| CodeMirror 6 + xterm WebGL | Yes |
| Theme engine + CSS variables | Yes — tokens + xterm/editor follow resolved theme |
| AI chat rail, agent diffs, composer | **Not shipped** — Terax parity is a non-goal; terminal/ACP agent direction is design-only ([COPILOT.md](./COPILOT.md)); right rail stays reserved |
| Source control / git graph | Not present |
| Web preview / markdown tabs | Markdown WYSIWYG helper exists for editor tabs (toggle with `Mod+Shift+V`); not a separate tab kind |
| Spaces / multi-project switcher | Not present (ADE sessions cover reconnect) |
| React 19 + Tailwind + shadcn | Yes — chrome in `src/app` + `src/components/ui` (Button, Tabs, DropdownMenu, ContextMenu, Separator, … like Terax); ADE controller remains imperative (`src/ade/bootstrap.ts`) |
| Large trees via React reconciliation | **No** — explorer stays `VirtualTree` (windowed rows) |

## 3. Information architecture

### Regions

```
┌────┬─────────────────────────────────────────────────────────┬───────────┐
│ A  │  Tab bar  [ term ] [ term ] [ file.rs • ]  [+]  [split] │  (rail)   │
│ c  ├─────────────────────────────────────────────────────────┤  reserved │
│ t  │                                                         │  closed   │
│ i  │              Active stack (terminal | editor)           │  by       │
│ v  │              + nested pane tree when split              │  default  │
│ i  │                                                         │           │
│ t  │                                                         │           │
│ y  │                                                         │           │
│    │                                                         │           │
├────┴─────────────────────────────────────────────────────────┴───────────┤
│  Status: online · session abc123 · editor · fs · watch                  │
└──────────────────────────────────────────────────────────────────────────┘
```

| Region | Role |
|--------|------|
| **Activity / sidebar** | Activity bar (Explorer + Settings). Sidebar hosts the file tree; collapsible; width persisted in layout blob / `localStorage`. |
| **Tab bar** | All document surfaces. New tab = new PTY by default. Double-click file opens/focuses an **editor tab**. |
| **Main stack** | One visible tab; inactive tabs hidden (`visibility` / `content-visibility`), not unmounted. |
| **Right rail** | Reserved width 0. |
| **Status bar** | Connection / session / capability pills; left side shows live status messages. |

### First viewport

After a successful attach:

1. Focus lands in the active terminal pane.
2. No connection form — auth comes from `?token=` / tab `sessionStorage`.
3. No marketing copy, stats strips, or floating badges on the workspace.

Before the first successful connect, the main stack shows a calm empty state (“Open the printed Local access URL to connect”).

### Settled UI choices

| Topic | Behavior |
|-------|----------|
| **Connect UX** | Silent auto-connect from printed `?token=` URL (cached in `sessionStorage` for reload). Status bar shows connection; Disconnect / Reconnect via command palette. |
| **Default keybindings** | Match Terax pane/tab shortcuts (see §6). |
| **File tree icons** | Lightweight inline SVG icons + typed color tones (`src/icons.ts`); VS Code–style indent guides and chevrons. |
| **UI framework** | React 19 + Tailwind v4 + shadcn/ui for shell chrome. Imperative ADE controller (`bootstrapAde`) owns protocol, xterm, CodeMirror, and the virtualized tree — attached to stable DOM ids from the React shell. |
| **Large trees** | Lazy one-level `fs_list` + `VirtualTree` row virtualization. Backend `fs_watch` skips noisy trees when installing watches. |
| **Editor authority UX** | Normal file-tab chrome (path, dirty `•`, save). Remote/Fresh authority surfaces on connection state, save errors, and conflicts — not permanent “remote buffer” badging. |

## 4. Tab and pane model

```ts
type Tab =
  | { kind: "terminal"; id: string; title: string; paneTree: PaneNode }
  | { kind: "editor"; id: string; bufferId: string; path: string; dirty: boolean; preview?: boolean };

type PaneNode =
  | { type: "leaf"; id: string /* pty id */; cwd?: string }
  | { type: "split"; direction: "row" | "col"; children: PaneNode[]; sizes?: number[] };
```

**Behaviors**

- **Preview editor tabs:** single-click / first open may be preview; double-click or edit pins. Next preview replaces the unpinned tab.
- **Inherited cwd:** new terminal tabs/splits prefer active leaf cwd (OSC 7) or last terminal cwd (not the editor file’s parent).
- **Explorer root:** follows the active terminal leaf cwd when reachable under the FS sandbox (`VirtualTree.setViewRoot`). `fs_authorize` expands the sandbox when cwd leaves `--root`.
- **Max leaves per terminal tab:** 4.
- **Layout persistence:** tab list + `paneTree` (leaf `cwd` stamped) + `activeLeafId` + sidebar + per view-root explorer expanded/scroll in `layout_set` / localStorage (`version: 4`). Rust `SessionStore` is the source of truth for the layout string while the daemon is up; reopening `/?token=` reattaches and restores. Multi-tab terminal layouts partition live PTYs (tabs whose leaves ⊆ live set); leftover PTYs become single-pane tabs. Editor tabs are reopened by path on attach. Explorer expanded dirs + scroll are kept per cwd when switching terminals (Fresh `FileExplorerState`-shaped snapshots in the blob — not Fresh workspace disk files). Persistence ends when the session daemon is closed from the CLI.
- **Close pane vs close tab:** last leaf closes the tab; closing a tab closes remote PTYs / buffers for that tab.

## 5. Visual language

### Direction

- **Dense ADE**, not dashboard. Theme defaults to **system** (`prefers-color-scheme`); override to light or dark in settings (`data-theme` is always the resolved value).
- **One accent** for focus/active. Avoid purple-glow / multi-shadow AI aesthetics.
- **Typography:** mono for terminal + paths; UI chrome uses a paired sans. Defaults are IBM Plex (overridable).
- **Surfaces:** subtle elevation via border + fill shifts, not card grids.
- **Icons:** SVG folder/file icons with extension color tones; indent guides + chevrons.
- **Menus:** flat elevated rows matching shadcn DropdownMenu / ContextMenu (`.ctx-menu` / `.ctx-item`); ADE uses an imperative menu so editor/terminal roots are not remounted. React primitives live in `components/ui/context-menu.tsx` and `dropdown-menu.tsx`.
- **Tabs:** Terax-style strip — left **menu** for the active tab’s actions (Find / split / save / preview / wrap / pin / bulk close), compact tab triggers with a shared sliding pill and hover close (`.tab-close`), drag-to-reorder within the pinned or unpinned group, a dedicated pinned strip when any tab is pinned, right **+** to add a terminal or new file. Type-specific chrome stays in the menu so switching tabs does not jump the action strip.

### Tokens

Primer-inspired dark/light surfaces live in `src/tokens.css` (palette `primer`). Named packs in `src/palettes.ts` reuse Fresh editor theme colors (`vendor/fresh/.../themes/*.json`) mapped onto the same CSS variables for chrome, xterm (`xtermThemeFromCss`), and CodeMirror. shadcn semantic tokens bridge onto Primer. Tailwind `@theme` in `styles.css` exposes ADE and shadcn utilities.

```jsonc
"ui": {
  "theme": "system",       // system | light | dark (primer adaptive mode)
  "palette": "primer",     // primer | nord | dracula | solarized-dark | …
  "fontWeight": 400,       // UI chrome 100–900
  "monoFontWeight": 400,   // terminal + editor
  "fontFamily": "",        // empty → IBM Plex Sans
  "monoFontFamily": "",    // empty → IBM Plex Mono
  "editorMinimap": false,  // document map; off = chunk never loaded
  "editorLineWrap": true   // soft-wrap (Fresh editor.line_wrap); Alt+Z toggles
}
```

Pick a palette from the activity bar swatch, or **Mod+Shift+P** → “Color Palette…”. Choices apply immediately and are written to `config.json` when connected. Saving `fontWeight` / `monoFontWeight` updates CSS variables and open terminals/editors immediately (no reload).

Motion: tab pill slide, sidebar collapse, panel focus / menu appear. No perpetual ambient animation.

## 6. Interaction map

Defaults follow [Terax `shortcuts.ts`](https://github.com/crynta/terax-ai/blob/main/src/modules/shortcuts/shortcuts.ts). `Mod` = `Cmd` on macOS, `Ctrl` elsewhere.

| Action | Shortcut | Notes |
|--------|----------|-------|
| New terminal tab | `Mod+T` | Inherits cwd when known; also **+** → New Terminal |
| New file | — | **+** → New File… (prompts under active context) |
| Close tab or pane | `Mod+W` | Last leaf closes tab; confirm if editor dirty |
| Close other / all editors / terminals | — | Tab actions menu + command palette |
| Next / prev tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` | Ctrl even on macOS (Terax) |
| Jump to tab 1–9 | `Mod+1`…`Mod+9` | |
| Split pane right | `Mod+D` | Terminal tabs only |
| Split pane down | `Mod+Shift+D` | |
| Focus next / prev pane | `Mod+]` / `Mod+[` | |
| Swap pane | `Mod+Alt+Arrow` | |
| Save buffer | `Mod+S` | Editor tabs; also saves settings `config.json` |
| Toggle markdown WYSIWYG | `Mod+Shift+V` | Markdown editor tabs — editable rendered view (toolbar + formatting shortcuts); DOM is live while open, markdown syncs to CodeMirror on save/show-source |
| Toggle editor line wrap | `Alt+Z` | Soft wrap (Fresh `line_wrap`); also command palette |
| Toggle sidebar | `Mod+B` (and `Mod+Shift+B`) | |
| Find | `Mod+F` | Terminal buffer or editor search |
| Copy (terminal) | `Mod+C` | Copies when text is selected; otherwise interrupt |
| Paste (terminal) | `Mod+V` | System clipboard → PTY (bracketed paste when supported) |
| Go to File | `Mod+P` | Path `path[:line[:col]]` |
| Command palette | `Mod+Shift+P` | |
| Open settings | `Mod+,` | Opens backend `config.json` in an editor tab |
| Connect / disconnect | — | Auto-connect via `?token=` / `sessionStorage`; Disconnect / Reconnect in command palette |

**Context menus**

- **File tree:** Open in Terminal, New File…, New Folder…, Cut, Copy, Paste (in-app path clipboard + sandboxed `fs_create` / `fs_copy` / `fs_move`), Delete… (`fs_delete`, permanent), then Copy Path / Relative Path / File Name. Explorer header **↑** re-roots to the parent folder; when a terminal tab is focused it also sends `cd ..`.
- **Tabs:** Pin / Unpin, path copies when known, plus Close. Tab actions menu and command palette also offer Close Other Tabs / Close All Editors / Close All Terminals / Close Other Terminals.

**Terminal mouse:** drag to select (xterm). If a TUI enables DEC mouse reporting, hold **Shift** while dragging to select instead. Copy clears the selection so the next `Mod+C` is SIGINT (Fresh / VS Code policy).

**Windows note:** `Ctrl+D` is also shell EOF. Shortcut handling for split wins when the host owns the key; users who need raw EOF can remap later.

Tree: expand/collapse, open file (preview), pin on edit, keyboard nav. Dirty editors show `•` in the tab label. Theme, palette, fonts, and optional editor minimap live in `config.json` (not a settings modal). Shell scripts (`.sh` / shebang) get CodeMirror legacy shell highlighting. Terminal / editor **Ctrl/Cmd+click** on a path opens the file (Fresh `path_link` on the backend; host hover uses a mirrored detector).

Connection errors and auth failures use inline strip status; never modal loops.

## 7. Module inventory

| Module (`ui/src`) | Role |
|-------------------|------|
| `app/App.tsx` + `components/ui/*` | React shell chrome (activity, tab actions, status); shadcn Button / Separator / Tabs / DropdownMenu / ContextMenu |
| `ade/bootstrap.ts` | ADE controller: protocol, tabs, panes, tree wiring |
| `panes.ts` | Per-tab recursive pane trees |
| `terminal.ts` | xterm + WebGL + OSC 7 + clipboard |
| `tree.ts` | Virtualized explorer + context menu hook |
| `editor.ts` | CodeMirror 6 editor tabs |
| `markdown-preview.ts` / `markdown-wysiwyg.ts` | Markdown render (GFM / KaTeX / Mermaid) + contenteditable WYSIWYG with toolbar; DOM is authoritative while preview is open, serializes to CodeMirror on flush |
| `settings.ts` + backend `config.json` | Theme / fonts / shell / explorer prefs |
| `context-menu.ts` | Imperative flat menus + name prompt (visual parity with shadcn ContextMenu) |
| `palette.ts` / `shortcuts.ts` | Command palette and shortcut registry |
| `tokens.css` / `palettes.ts` / `styles.css` | Theme tokens and chrome CSS |
| `protocol.ts` | ADE message shapes |

### Quality bar

- Inactive terminal tabs remain mounted and continue receiving `pty_data`.
- Fit/resize only the visible leaves; debounce resize storms.
- Tree refresh stays silent (no “Loading…” flash); preserve expand/selection.
- Large directories: expand stays lazy (one `fs_list` per opened dir); viewport is virtualized. Backend recursive `fs_watch` skips ignored trees; the host still filters noisy `fs_changed` events.
- Save conflicts: revision CAS on the wire; surface failures in the status bar — never silent last-writer-wins. Day-to-day editor chrome stays local-feeling.

## 8. Stack

| Layer | Choice |
|-------|--------|
| Host chrome | React 19 + Tailwind v4 + shadcn/ui |
| ADE / editors | Imperative TypeScript modules (`terminal.ts`, `editor.ts`, `tree.ts`, `palette.ts`, …) |
| Pane trees | Hand-rolled DOM under the terminal stack |
| Package manager | Bun (`bun.lock`) via Pixi tasks |

Hybrid model: React mounts the shell; `bootstrapAde()` binds once to stable DOM ids. Explorer virtualization stays non-React.

## 9. Non-goals (UI)

- Feature parity with Terax AI chrome, agent diffs marketplace, or theme marketplace.
- Replacing Fresh’s TUI or embedding Fresh `--web` as the host chrome.
- Pixel-perfect Terax clone (different product: remote ADE, GPL).
- Marketing landing page inside the app window.

Agent panels (right rail) may appear later per [COPILOT.md](./COPILOT.md); that is not Terax parity.

## 10. Native GPUI host (v1)

`crates/fresh-gui-app/src/gui/` is the primary renderer. It reuses `fresh-gui-client` + ADE; it does **not** link Fresh.

Ribbon metrics live in `workspace.rs` and override gpui-component’s medium defaults (32px icon buttons, 32px tabs, `py_2` rails), which read sparse next to VS Code / Zed:

| Container | Size |
|-----------|------|
| Title bar | 30px (`TitleBar` height); label is `text_sm` with a 4px gap |
| Activity rail | 36px wide, `py_1`, no gap; explorer and settings are small ghost icon buttons |
| Explorer header | 26px, `text_xs`, xsmall collapse button |
| Explorer rows | 22px, `text_sm`, 8px indent, no tree padding |
| Tab strip | `TabBar::small()` (24px). The **+** is an xsmall ghost button in `last_empty_space`, so it follows the rightmost tab |
| Status bar | 22px, `py_0`, `gap_1` |

`TabBar` only mounts `last_empty_space` when a suffix or overflow menu is set, and a real suffix is pinned after the flex-1 scroller (the far-right **+** in earlier builds). The suffix here is a zero-width anchor; colors stay on the active theme (`tab_bar`, `sidebar`, `status_bar`, `border`).

| Native v1 | Status |
|-----------|--------|
| Connect / auth / session (`?token=` URL or `--token`) | Yes |
| SSH target add + auto-install + tunnel (`remote connect`) | Yes (CLI before the window; title bar shows the destination) |
| Activity bar + collapsible explorer (`fs_list`) | Yes |
| Unified terminal + editor tabs | Yes (one pane per tab; no splits) |
| Status bar (connection, session, capabilities) | Yes |
| Command palette + Go to File | Yes |
| PTY I/O (VTE grid, OSC 7 tab title/cwd) | Yes (plain text rows; prompt color/escape sequences may show literally; no WebGL xterm, mouse select, or clipboard chords yet) |
| Editor open / edit / save (gpui `Editor` view of Fresh snapshots) | Yes |
| Pane splits, layout v4 restore, markdown WYSIWYG, minimap | Not in v1 |
| Context menus, find, palettes / typography packs, tab pin/reorder | Not in v1 |
| Ctrl/Cmd+click path_link | Not in v1 |

Packaged hosts: `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` and `fresh-gui-client-*-x86_64-pc-windows-msvc.zip`. Linux still needs X11 or Wayland, fontconfig, FreeType, and wgpu/Vulkan. `pixi.toml` platform for the daemon package is `linux-64`; `pixi run gui` is the from-source host.

## 11. References

- [DESIGN.md](./DESIGN.md) — architecture, protocol, Fresh coupling overview.
- [FRESH.md](./FRESH.md) — how the daemon embeds Fresh editor libraries.
- [SECURITY.md](./SECURITY.md) — token + SSH tunnel access model.
- [COPILOT.md](./COPILOT.md) — Copilot CLI / ACP design (issue #49).
- [Terax](https://github.com/crynta/terax-ai) — layout, tabs, terminal/editor stacks, polish bar.
