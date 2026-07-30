# fresh-gui Host UI

Product UI for the **local host** ADE shell. Architecture and protocol: [DESIGN.md](./DESIGN.md). Overview for users: [README.md](../README.md).

**Status:** UI-1–UI-3 are in use in `crates/fresh-gui-app/ui` — status bar, unified terminal/editor tabs, CodeMirror 6, xterm WebGL, pane trees, shortcuts + palette, virtualized tree, OSC 7 cwd, find, activity bar, system/light/dark theme + named color palettes + typography via `config.json`, path context menus. Connection is silent (`?token=` / sessionStorage auto-connect); no top connect form.

This doc keeps the IA, visual language, and interaction map for ongoing polish. It is not a backlog of unfinished MVP chrome.

## 1. Goals

- **Terminal is the hero.** First paint after connect should feel like a workspace of shells, not a settings form with a terminal bolted on.
- **Editor and explorer are peers**, not afterthoughts: same tab chrome, shared focus model, shared shortcuts.
- **Remote-aware chrome.** Connection, session, and capability state are first-class (unlike Terax’s local monolith).
- **Dense, calm, premium.** Terax-level polish bar: intentional motion, clear hierarchy, no decorative clutter.
- **Stay dense.** React + shadcn own chrome; editor-core and PTY stay Fresh / xterm / CodeMirror.

## 2. Borrow from Terax vs diverge

Inspired by Terax’s public UI / [TERAX.md](https://github.com/crynta/terax-ai/blob/main/TERAX.md) module layout. We borrow **structure, feel, and the React 19 + Tailwind + shadcn chrome stack**, not feature parity or Terax’s local monolith.

| Terax idea | Borrow? | fresh-gui note |
|------------|---------|----------------|
| Three-zone shell: sidebar · main · optional right rail | **Yes** | Right rail reserved; empty until a real panel exists |
| Unified tab bar over content stacks | **Yes** | Tabs are `terminal` \| `editor` (more kinds later) |
| Tabs stay mounted / hidden, not destroyed | **Yes** | Keeps remote PTYs and xterm buffers alive |
| Recursive pane tree (row/col splits, max ~4 leaves) | **Yes (UI-2)** | Replace today’s “two-pane only” split |
| Collapsible explorer + activity-style sidebar | **Yes** | Explorer + Settings (opens `config.json`) |
| Status bar (cwd, branch, indicators) | **Yes** | Show remote root / session / connection |
| Header search that adapts to terminal vs editor | **Yes (UI-3)** | Find bar for terminal / editor |
| Sliding “pill” active-tab indicator | **Yes** | Cheap polish, high perceived quality |
| CodeMirror 6 + language registry | **Yes (UI-1)** | |
| xterm WebGL renderer | **Yes (UI-1)** | |
| Theme engine + CSS variables | **Yes** | Tokens + xterm/editor follow resolved theme |
| AI chat rail, agent diffs, composer | **No (MVP)** | Explicit non-goal in DESIGN §2 |
| Source control / git graph | **Later** | Needs protocol + UX design of its own |
| Web preview / markdown tabs | **Later** | Optional tab kinds after editor tabs land |
| Spaces / multi-project switcher | **Later** | Map to ADE sessions when useful |
| Custom window controls | **Windows only** | Follow Tauri; don’t invent chrome early |
| React 19 + Tailwind + shadcn | **Yes** | Host chrome in `src/app` + `src/components/ui`; ADE controller still imperative (`src/ade/bootstrap.ts`) |
| Large trees via React reconciliation | **No** | Explorer stays `VirtualTree` (windowed rows), not a React list of 10k nodes |

## 3. Information architecture

### 3.1 Regions (target)

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
| **Activity / sidebar** | Activity bar (Explorer + Settings). Sidebar hosts the file tree; collapsible; width persisted in layout blob. |
| **Tab bar** | All document surfaces. New tab = new PTY by default. Double-click file opens/focuses an **editor tab** (not a permanent bottom pane). |
| **Main stack** | One visible tab; inactive tabs hidden (`visibility` / `content-visibility`), not unmounted. |
| **Right rail** | Reserved width 0. Do not build AI chrome here for MVP. |
| **Status bar** | Connection / session / capability pills, dirty count; left side shows live status messages. |

### 3.2 First viewport rules

After a successful attach:

1. Focus lands in the active terminal pane.
2. No connection form in the chrome — auth comes from `?token=` / tab `sessionStorage`.
3. No marketing copy, stats strips, or floating badges on the workspace.

Before the first successful connect, the main stack shows a calm empty state (“Open the printed Local access URL to connect”).

### 3.3 Resolved UI decisions

| Topic | Decision |
|-------|----------|
| **Connect UX** | Silent auto-connect from printed `?token=` URL (cached in `sessionStorage` for reload). Status bar shows connection; Disconnect / Reconnect via command palette. No top connect form. |
| **Default keybindings** | Match **Terax** pane/tab shortcuts (see §6). Remappable later via settings. |
| **File tree icons** | Lightweight inline SVG icons + typed color tones (`src/icons.ts`); VS Code–style indent guides and chevrons in the virtualized tree. No full Material icon pack. |
| **UI framework** | **React 19 + Tailwind v4 + shadcn/ui** for shell chrome (activity, tabs actions, status). Imperative ADE controller (`bootstrapAde`) still owns protocol, xterm, CodeMirror, and the virtualized tree — attached to stable DOM ids from the React shell. |
| **Large trees (~10k files)** | Host concern: keep lazy one-level `fs_list`; **row virtualization** (`VirtualTree`) + watch discipline. Backend `fs_watch` skips `.git` / `target` / `node_modules` / … when installing recursive watches (and runs install off the WS task) so PTY I/O is not stalled. Do not replace the explorer with a naive React row list. |
| **Editor authority UX** | **Quiet day-to-day:** normal file-tab chrome (path, dirty `•`, save). Surface remote/Fresh authority only on **connection state**, **save errors**, and **conflicts** — not permanent “remote buffer” badging. |

## 4. Tab and pane model

Mirror Terax’s tagged-union tabs, scoped to what the protocol already supports.

```ts
type Tab =
  | { kind: "terminal"; id: string; title: string; paneTree: PaneNode }
  | { kind: "editor"; id: string; bufferId: string; path: string; dirty: boolean; preview?: boolean };

type PaneNode =
  | { type: "leaf"; id: string /* pty id */; cwd?: string }
  | { type: "split"; direction: "row" | "col"; children: PaneNode[]; sizes?: number[] };
```

**Behaviors (Terax-aligned)**

- **Preview editor tabs:** single-click / first open may be preview; double-click or edit pins. Next preview replaces the unpinned tab.
- **Inherited cwd:** new terminal tabs/splits prefer active leaf cwd (OSC 7) or last terminal cwd (not the editor file’s parent — Terax-aligned).
- **Explorer root:** follows the active terminal leaf cwd when it lies under the backend FS sandbox (`VirtualTree.setViewRoot`).
- **Max leaves per terminal tab:** 4 (renderer cost + clarity).
- **Layout persistence:** serialize tab list + `paneTree` + sidebar width into existing `layout_set` / localStorage (extend today’s JSON blob).
- **Close pane vs close tab:** last leaf closes the tab; closing a tab closes remote PTYs / buffers for that tab.

**Migration from today**

| Today | Target |
|-------|--------|
| Flat `tabs[]` of PTYs only | **UI-1:** unified `terminal` \| `editor` tabs |
| Global H/V “two shells” split | **UI-2 ✅:** per-tab recursive `PaneNode` splits (max 4 leaves) |
| Editor docked under terminals | **UI-1:** `editor` tabs in the same bar |
| Connect fields always large | **Removed:** silent auto-connect; no top form |

## 5. Visual language

### 5.1 Direction

- **Dense ADE**, not dashboard. Theme defaults to **system** (`prefers-color-scheme`); override to light or dark in settings (`data-theme` is always the resolved value).
- **One accent** for focus/active (connection healthy, active tab, primary button). Avoid purple-glow / multi-shadow AI aesthetics.
- **Typography:** keep expressive mono for terminal + paths (`IBM Plex Mono` or similar); UI chrome uses a paired sans. No Inter/Roboto/Arial defaults as the brand face.
- **Surfaces:** subtle elevation via border + slight fill shifts, not card grids. Resizable gutters like Terax (`react-resizable-panels` or CSS equivalent).
- **Icons:** Lightweight SVG folder/file icons with extension color tones in the explorer; indent guides + chevrons (VS Code–like). Full Material icon packs remain optional later.

### 5.2 Tokens

Primer-inspired dark/light surfaces live in `src/tokens.css` (palette `primer`). Named packs in `src/palettes.ts` reuse Fresh editor theme colors (`vendor/fresh/.../themes/*.json`) mapped onto the same CSS variables for chrome, xterm (`xtermThemeFromCss`), and CodeMirror. shadcn semantic tokens (`--background`, `--primary`, …) bridge onto Primer. Tailwind `@theme` in `styles.css` exposes both ADE utilities (`bg-bg`, …) and shadcn utilities (`bg-primary`, `text-muted-foreground`, …). IBM Plex remains the default UI font (overridable via `fontFamily` / `monoFontFamily`).

```jsonc
"ui": {
  "theme": "system",       // system | light | dark (primer adaptive mode)
  "palette": "primer",     // primer | nord | dracula | solarized-dark | …
  "fontWeight": 400,       // UI chrome 100–900
  "monoFontWeight": 400,   // terminal + editor
  "fontFamily": "",        // empty → IBM Plex Sans
  "monoFontFamily": "",    // empty → IBM Plex Mono
  "editorMinimap": false   // document map; off = chunk never loaded
}
```

Pick a palette from the activity bar swatch, or **Mod+Shift+P** → “Color Palette…”. Choices apply immediately and are written to `config.json` when connected.

Motion: 2–3 intentional uses — tab pill slide, sidebar collapse, panel focus ring. No perpetual ambient animation.

## 6. Interaction map

Defaults follow [Terax `shortcuts.ts`](https://github.com/crynta/terax-ai/blob/main/src/modules/shortcuts/shortcuts.ts). `Mod` = `Cmd` on macOS, `Ctrl` elsewhere.

| Action | Shortcut | Notes |
|--------|----------|-------|
| New terminal tab | `Mod+T` | Inherits cwd when known |
| Close tab or pane | `Mod+W` | Last leaf closes tab; confirm if editor dirty |
| Next / prev tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` | Terax uses Ctrl even on macOS |
| Jump to tab 1–9 | `Mod+1`…`Mod+9` | |
| Split pane right | `Mod+D` | Terminal tabs only (iTerm-like; same as Terax) |
| Split pane down | `Mod+Shift+D` | |
| Focus next / prev pane | `Mod+]` / `Mod+[` | |
| Swap pane | `Mod+Alt+Arrow` | |
| Save buffer | `Mod+S` | Editor tabs; also saves settings `config.json` |
| Toggle sidebar | `Mod+B` (and `Mod+Shift+B`) | |
| Find | `Mod+F` | Terminal buffer or editor search |
| Copy (terminal) | `Mod+C` | Copies when text is selected (mouse drag); otherwise sends interrupt |
| Paste (terminal) | `Mod+V` | System clipboard → PTY (bracketed paste when supported) |
| Command palette | `Mod+P` | |
| Open settings | `Mod+,` | Opens backend `config.json` in an editor tab; missing default keys are added (existing values kept) |
| Connect / disconnect | — | Auto-connect via `?token=` / `sessionStorage`; Disconnect / Reconnect in command palette. Disconnect keeps remote session |

**Context menus:** right-click a tab or file-tree row to copy absolute path, relative path (vs workspace root), or file name; tabs also offer Close.

**Terminal mouse:** drag to select (xterm). If a TUI enables DEC mouse reporting, hold **Shift** while dragging to select instead. Copy clears the selection so the next `Mod+C` is SIGINT (Fresh / VS Code policy).

**Windows note:** `Ctrl+D` is also shell EOF. Prefer Terax behavior: shortcut wins when the host handles it for split; users who need raw EOF can remap. Do not silently switch to VS Code `\` bindings.

Tree: expand/collapse, open file (preview), pin on edit, keyboard nav. Dirty editors show `•` in the tab label. Theme mode, color palette, fonts, and optional editor document map (`ui.editorMinimap`) live in `config.json` (not a settings modal). Shell scripts (`.sh` / shebang) get CodeMirror legacy shell highlighting.

Connection errors and auth failures use inline strip status; never modal loops.

## 7. Component inventory

### 7.1 Current → target

| Current (`ui/src`) | Role |
|--------------------|------|
| Connection inputs in `main.ts` | Removed — silent auto-connect + status bar |
| `#tabs` + split buttons | Unified terminal / editor tabs |
| `#panes` / `panes.ts` | Per-tab recursive pane tree |
| `terminal.ts` | xterm + WebGL + OSC 7 |
| `tree.ts` | Virtualized explorer + context menu hook |
| `#editor-stack` / `editor.ts` | CodeMirror 6 editor tabs |
| `settings.ts` + backend `config.json` | Theme / fonts / shell (no modal) |
| `context-menu.ts` | Tab + tree path actions |
| `tokens.css` + region CSS | Shared chrome / terminal tokens |
| `protocol.ts` | ADE message shapes |

### 7.2 Shell quality bar (from Terax, adapted)

- Inactive terminal tabs remain mounted and continue receiving `pty_data`.
- Fit/resize only the visible leaves; debounce resize storms.
- Tree refresh stays silent (no “Loading…” flash); preserve expand/selection (already started).
- Large directories: expand stays **lazy** (one `fs_list` per opened dir). When a visible folder can expose thousands of rows, **virtualize** the tree viewport (mount only visible rows). Backend recursive `fs_watch` skips ignored trees before registering inotify watches; the host still applies Debounced/`WATCH_IGNORE`-style filtering so residual `fs_changed` events do not rebuild the world.
- Save conflicts: when backend grows mtime/CAS errors, show overwrite UI — never silent last-writer-wins (Terax editor lesson). Day-to-day editor chrome stays local-feeling; remote authority appears in the status bar and on save or conflict failures only.

## 8. Stack decision

| Option | Status |
|--------|--------|
| **A. Vite + TypeScript modules only** | Superseded for chrome. Still used for ADE modules (`terminal.ts`, `editor.ts`, `tree.ts`, `palette.ts`, …). |
| **B. Preact/React for panes only** | Not needed; pane trees remain hand-rolled DOM under the terminal stack. |
| **C. React 19 + Tailwind + shadcn** (current) | **Chosen** for host chrome (Terax-aligned look). Hybrid: React mounts the shell; `bootstrapAde()` binds once. Explorer virtualization stays non-React. Revert path: drop back to static `index.html` + modules if perf is unacceptable. |

## 9. Implementation phases

### Phase UI-0 — Design lock ✅ (this doc)

- Agree IA, tab model, non-goals; link from DESIGN.md.

### Phase UI-1 — Chrome + unified surfaces ✅

- Status bar (connection / session / capabilities).
- Tokenized theme; restyle tree / tabs / panes without new features.
- Editor becomes tabs (remove permanent bottom dock).
- CodeMirror 6 + xterm WebGL.
- Tab pill animation; sidebar collapse + persisted width.
- Dirty / preview tab affordances.

### Phase UI-2 — Pane tree + keyboard ✅

- Recursive splits (max 4 leaves/tab, `src/panes.ts`); replaced the old global two-pane mode — each terminal tab now owns its own `PaneNode` tree and a `leaves: Map<ptyId, TermBundle>`.
- Shortcut registry (`src/shortcuts.ts`) wired via `installShortcuts`; **Go to File** (`Mod+P`) pastes/types a path (`path[:line[:col]]`); **command palette** (`Mod+Shift+P`, `src/palette.ts`) lists shortcuts and runs them by id.
- Terminal / editor **Ctrl/Cmd+click** on a path opens the file (Fresh `path_link` detection on the backend; host hover uses a mirrored detector). Relative paths resolve against the terminal OSC 7 cwd.
- Tree keyboard navigation (arrow keys, Enter) via the virtualized tree.
- Explorer: **virtualized rows** (`src/tree.ts` `VirtualTree`) — lazy one-level `fs_list` + windowed row rendering, so expanded views stay responsive with large trees.
- Richer layout blob (`version: 2`) in `layout_set` / localStorage, including each terminal tab's `paneTree` and `activeLeafId`.
- Simplifications: pane-tree restore across `session_attach` only recreates the exact multi-pane layout when the reattached ptys match a persisted tab's leaf ids 1:1; otherwise it falls back to one terminal tab per pty. Editor tabs are not restored across reattach (server-side buffers aren't rehydrated by path yet).

### Phase UI-3 — Depth ✅

- OSC 7 cwd → tab titles + inherited cwd for new tabs/splits (backend bash/zsh hooks in `pty.rs`; client `TermBundle.cwd` + `pty_open.cwd`).
- Explorer re-roots to the active terminal leaf cwd (Terax `explorerRoot`); `fs_authorize` expands the FS sandbox when cwd leaves `--root` (e.g. `~/csv-utils`). Editor tabs keep the last terminal cwd.
- Header find bar for terminal buffer (`@xterm/addon-search`, `Mod+F`); editor uses CodeMirror search panel.
- Activity bar with Explorer (toggles sidebar) + Settings entry; explorer remains the only sidebar view until SCM exists.
- Lightweight SVG file/folder icons + indent guides in the virtualized tree (`src/icons.ts`, `src/tree.ts`) — no heavy icon pack.
- Skipped `fs_list` pagination for now — row virtualization covers large trees; revisit only if a single directory listing becomes a protocol bottleneck.
- Theme preference (`system` / `light` / `dark`) + color **palette** (`primer` or Fresh theme names: `nord`, `dracula`, …) via `config.json`; typography (`fontWeight`, `monoFontWeight`, optional font families). Primer-inspired CSS tokens (`data-theme` resolved); named palettes map Fresh theme colors onto the same vars for chrome / xterm / CodeMirror. React 19 + Tailwind v4 + shadcn for chrome; IBM Plex via `@fontsource` (overridable). Settings live in backend `config.json`.
- Path context menus on tabs and the file tree (copy absolute / relative / name).
- Terminal mouse selection + `Mod+C` / `Mod+V` clipboard (Fresh policy: copy when selected, else interrupt).

### Out of UI phases (stay in DESIGN Phase 4+)

- Code signing, auto-update for the Windows host.
- AI rail, git graph, web preview, spaces — separate design spikes if/when wanted.

## 10. Non-goals (UI)

- Feature parity with Terax AI, agents, or theme marketplace.
- Replacing Fresh’s TUI or embedding Fresh `--web` as the host chrome.
- Pixel-perfect Terax clone (different product: remote ADE, GPL, no AI).
- Marketing landing page inside the app window.

## 11. Open questions

None currently — resolved items live in §3.3.

## 12. References

- [DESIGN.md](./DESIGN.md) — product architecture, protocol, phases.
- [WINDOWS.md](./WINDOWS.md) — Tauri packaging.
- [Terax](https://github.com/crynta/terax-ai) — layout, tabs, terminal/editor stacks, polish bar.
- Terax `TERAX.md` — module boundaries worth mirroring in naming only.
