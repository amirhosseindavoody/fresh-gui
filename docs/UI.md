# fresh-gui Host UI

Product UI for the **local host** ADE shell (`crates/fresh-gui-app/ui`). Architecture and protocol: [DESIGN.md](./DESIGN.md). User overview: [README.md](../README.md).

The shipped UI includes a status bar, unified terminal/editor tabs, CodeMirror 6, xterm WebGL, per-tab pane trees, shortcuts + command palette, virtualized explorer, OSC 7 cwd sync, find, activity bar, system/light/dark theme with named palettes and typography via `config.json`, and path/file context menus. Connection is silent (`?token=` / `sessionStorage` auto-connect); there is no top connect form.

## 1. Goals

- **Terminal is the hero.** First paint after connect feels like a workspace of shells, not a settings form with a terminal bolted on.
- **Editor and explorer are peers:** same tab chrome, shared focus model, shared shortcuts.
- **Remote-aware chrome.** Connection, session, and capability state are first-class (unlike Terax’s local monolith).
- **Dense, calm, premium.** Intentional motion, clear hierarchy, no decorative clutter.
- **Stay dense.** React + shadcn own chrome; editor-core and PTY stay Fresh / xterm / CodeMirror.

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
| AI chat rail, agent diffs, composer | **No** — explicit non-goal |
| Source control / git graph | Not present |
| Web preview / markdown tabs | Markdown preview helper exists for editor; not a separate tab kind |
| Spaces / multi-project switcher | Not present (ADE sessions cover reconnect) |
| Custom window controls | Windows Tauri only — follow Tauri defaults |
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
- **Layout persistence:** tab list + `paneTree` + `activeLeafId` + sidebar width in `layout_set` / localStorage (`version: 2`). Pane-tree restore across `session_attach` only when reattached PTYs match persisted leaf ids 1:1; otherwise one terminal tab per PTY. Editor tabs are not restored across reattach.
- **Close pane vs close tab:** last leaf closes the tab; closing a tab closes remote PTYs / buffers for that tab.

## 5. Visual language

### Direction

- **Dense ADE**, not dashboard. Theme defaults to **system** (`prefers-color-scheme`); override to light or dark in settings (`data-theme` is always the resolved value).
- **One accent** for focus/active. Avoid purple-glow / multi-shadow AI aesthetics.
- **Typography:** mono for terminal + paths; UI chrome uses a paired sans. Defaults are IBM Plex (overridable).
- **Surfaces:** subtle elevation via border + fill shifts, not card grids.
- **Icons:** SVG folder/file icons with extension color tones; indent guides + chevrons.
- **Menus:** flat elevated rows matching shadcn DropdownMenu / ContextMenu (`.ctx-menu` / `.ctx-item`); ADE uses an imperative menu so editor/terminal roots are not remounted. React primitives live in `components/ui/context-menu.tsx` and `dropdown-menu.tsx`.
- **Tabs:** Terax-style strip — left **menu** for the active tab’s actions (Find / split / save / preview / wrap), compact tab triggers with a shared sliding pill and hover close (`.tab-close`), right **+** to add a terminal or new file. Type-specific chrome stays in the menu so switching tabs does not jump the action strip.

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

Pick a palette from the activity bar swatch, or **Mod+Shift+P** → “Color Palette…”. Choices apply immediately and are written to `config.json` when connected.

Motion: tab pill slide, sidebar collapse, panel focus / menu appear. No perpetual ambient animation.

## 6. Interaction map

Defaults follow [Terax `shortcuts.ts`](https://github.com/crynta/terax-ai/blob/main/src/modules/shortcuts/shortcuts.ts). `Mod` = `Cmd` on macOS, `Ctrl` elsewhere.

| Action | Shortcut | Notes |
|--------|----------|-------|
| New terminal tab | `Mod+T` | Inherits cwd when known; also **+** → New Terminal |
| New file | — | **+** → New File… (prompts under active context) |
| Close tab or pane | `Mod+W` | Last leaf closes tab; confirm if editor dirty |
| Next / prev tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` | Ctrl even on macOS (Terax) |
| Jump to tab 1–9 | `Mod+1`…`Mod+9` | |
| Split pane right | `Mod+D` | Terminal tabs only |
| Split pane down | `Mod+Shift+D` | |
| Focus next / prev pane | `Mod+]` / `Mod+[` | |
| Swap pane | `Mod+Alt+Arrow` | |
| Save buffer | `Mod+S` | Editor tabs; also saves settings `config.json` |
| Toggle markdown preview | `Mod+Shift+V` | Markdown editor tabs |
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
- **Tabs:** path copies when known, plus Close.

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

- Feature parity with Terax AI, agents, or theme marketplace.
- Replacing Fresh’s TUI or embedding Fresh `--web` as the host chrome.
- Pixel-perfect Terax clone (different product: remote ADE, GPL, no AI).
- Marketing landing page inside the app window.

## 10. References

- [DESIGN.md](./DESIGN.md) — architecture, protocol, Fresh coupling overview.
- [FRESH.md](./FRESH.md) — how the daemon embeds Fresh editor libraries.
- [SECURITY.md](./SECURITY.md) — token + SSH tunnel access model.
- [WINDOWS.md](./WINDOWS.md) — Tauri packaging.
- [Terax](https://github.com/crynta/terax-ai) — layout, tabs, terminal/editor stacks, polish bar.
