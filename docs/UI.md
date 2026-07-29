# fresh-gui Host UI

Product UI for the **local host** ADE shell. Architecture and protocol live in [DESIGN.md](./DESIGN.md). This document defines **what the UI should become**, borrowing layout and interaction patterns from [Terax](https://github.com/crynta/terax-ai) while staying honest about our remote-backend split.

**Status:** UI-1 through UI-3 shipped in `crates/fresh-gui-app/ui` (connection strip, status bar, unified terminal/editor tabs, CodeMirror 6, xterm WebGL, per-tab recursive pane trees, shortcut registry + command palette, virtualized file tree, OSC 7 cwd, find bar, activity bar, light theme + settings). Architecture/protocol: [DESIGN.md](./DESIGN.md).

## 1. Goals

- **Terminal is the hero.** First paint after connect should feel like a workspace of shells, not a settings form with a terminal bolted on.
- **Editor and explorer are peers**, not afterthoughts: same tab chrome, shared focus model, shared shortcuts.
- **Remote-aware chrome.** Connection, session, and capability state are first-class (unlike Terax’s local monolith).
- **Dense, calm, premium.** Terax-level polish bar: intentional motion, clear hierarchy, no decorative clutter.
- **Stay light.** Prefer small CSS + typed modules over a second product stack until the shell IA is proven.

## 2. Borrow from Terax vs diverge

Inspired by Terax’s public UI / [TERAX.md](https://github.com/crynta/terax-ai/blob/main/TERAX.md) module layout. We borrow **structure and feel**, not feature parity or React-by-default.

| Terax idea | Borrow? | fresh-gui note |
|------------|---------|----------------|
| Three-zone shell: sidebar · main · optional right rail | **Yes** | Right rail reserved; empty until a real panel exists |
| Unified tab bar over content stacks | **Yes** | Tabs are `terminal` \| `editor` (more kinds later) |
| Tabs stay mounted / hidden, not destroyed | **Yes** | Keeps remote PTYs and xterm buffers alive |
| Recursive pane tree (row/col splits, max ~4 leaves) | **Yes (UI-2)** | Replace today’s “two-pane only” split |
| Collapsible explorer + activity-style sidebar | **Yes** | Start with explorer only; activity icons later |
| Status bar (cwd, branch, indicators) | **Yes** | Show remote root / session / connection |
| Header search that adapts to terminal vs editor | **Later** | After unified tabs |
| Sliding “pill” active-tab indicator | **Yes** | Cheap polish, high perceived quality |
| CodeMirror 6 + language registry | **Yes (UI-1)** | Move off CM5 |
| xterm WebGL renderer | **Yes (UI-1)** | Match Terax performance story |
| Theme engine + CSS variables | **Yes** | One token set driving chrome + terminal + editor |
| AI chat rail, agent diffs, composer | **No (MVP)** | Explicit non-goal in DESIGN §2 |
| Source control / git graph | **Later** | Needs protocol + UX design of its own |
| Web preview / markdown tabs | **Later** | Optional tab kinds after editor tabs land |
| Spaces / multi-project switcher | **Later** | Map to ADE sessions when useful |
| Custom window controls | **Windows only** | Follow Tauri; don’t invent chrome early |
| shadcn + Tailwind + React 19 | **No** | Stay Vite + TS; large trees → virtualization, not React |

## 3. Information architecture

### 3.1 Regions (target)

```
┌──────────────────────────────────────────────────────────────────────────┐
│  Title / connection strip (compact)     [session] [● connected] [⋯]     │
├────┬─────────────────────────────────────────────────────────┬───────────┤
│ A  │  Tab bar  [ term ] [ term ] [ file.rs • ]  [+]  [split] │  (rail)   │
│ c  ├─────────────────────────────────────────────────────────┤  reserved │
│ t  │                                                         │  closed   │
│ i  │              Active stack (terminal | editor)           │  by       │
│ v  │              + nested pane tree when split              │  default  │
│ i  │                                                         │           │
│ t  │                                                         │           │
│ y │                                                         │           │
│    │                                                         │           │
├────┴─────────────────────────────────────────────────────────┴───────────┤
│  Status: ~/proj · session abc123 · editor · fs_watch · latency optional │
└──────────────────────────────────────────────────────────────────────────┘
```

| Region | Role |
|--------|------|
| **Connection strip** | **Always on** (not a disconnect-only modal). Backend URL, auth, Connect/Disconnect stay in a top strip. After connect the strip stays visible but **compacts** to a quiet chip row (host + session id + status); expand inline to edit URL/token without leaving the workspace. |
| **Activity / sidebar** | Activity bar (Explorer + Settings). Sidebar hosts the file tree; collapsible; width persisted in layout blob. |
| **Tab bar** | All document surfaces. New tab = new PTY by default. Double-click file opens/focuses an **editor tab** (not a permanent bottom pane). |
| **Main stack** | One visible tab; inactive tabs hidden (`visibility` / `content-visibility`), not unmounted. |
| **Right rail** | Reserved width 0. Do not build AI chrome here for MVP. |
| **Status bar** | Remote cwd/root, session id (truncated), capability pills, dirty count, optional RTT. |

### 3.2 First viewport rules

After a successful attach:

1. Focus lands in the active terminal pane.
2. Connection strip stays present but compact — not a full form competing with the terminal.
3. No marketing copy, stats strips, or floating badges on the workspace.
4. Brand mark may appear small in the strip or about dialog — never as a hero competing with the terminal.

Before the first successful connect, the strip is expanded (URL / token / Connect) and the main stack shows a calm empty state (“Connect to a backend”) — not a SaaS landing page.

### 3.3 Resolved UI decisions

| Topic | Decision |
|-------|----------|
| **Connect UX** | Always-on strip (compact when connected; expand inline to edit). No modal-only connect flow. |
| **Default keybindings** | Match **Terax** pane/tab shortcuts (see §6). Remappable later via settings. |
| **File tree icons** | UI-1 text-only; **UI-3:** lightweight extension badges (`src/icons.ts`), not a full icon pack. |
| **UI framework** | **Stay on Vite + TypeScript modules.** Do not adopt Preact/React for large trees or pane polish. Revisit only if hand-rolled pane trees become unmaintainable. |
| **Large trees (~10k files)** | Host concern: keep lazy one-level `fs_list`; add **row virtualization** + watch discipline when polishing the explorer. Not a Fresh-editor problem; not a reason to add React. |
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
- **Inherited cwd:** new terminal tabs/splits prefer active leaf cwd (OSC 7) or editor file’s parent.
- **Max leaves per terminal tab:** 4 (renderer cost + clarity).
- **Layout persistence:** serialize tab list + `paneTree` + sidebar width into existing `layout_set` / localStorage (extend today’s JSON blob).
- **Close pane vs close tab:** last leaf closes the tab; closing a tab closes remote PTYs / buffers for that tab.

**Migration from today**

| Today | Target |
|-------|--------|
| Flat `tabs[]` of PTYs only | **UI-1:** unified `terminal` \| `editor` tabs |
| Global H/V “two shells” split | **UI-2 ✅:** per-tab recursive `PaneNode` splits (max 4 leaves) |
| Editor docked under terminals | **UI-1:** `editor` tabs in the same bar |
| Connect fields always large | **UI-1:** compact strip when connected |

## 5. Visual language

### 5.1 Direction

- **Dense ADE**, not dashboard. Dark-first default; light theme via settings (`data-theme`).
- **One accent** for focus/active (connection healthy, active tab, primary button). Avoid purple-glow / multi-shadow AI aesthetics.
- **Typography:** keep expressive mono for terminal + paths (`IBM Plex Mono` or similar); UI chrome uses a paired sans. No Inter/Roboto/Arial defaults as the brand face.
- **Surfaces:** subtle elevation via border + slight fill shifts, not card grids. Resizable gutters like Terax (`react-resizable-panels` or CSS equivalent).
- **Icons:** Lightweight extension badges in the explorer (UI-3). Full SVG icon packs remain optional later.

### 5.2 Tokens (illustrative)

Define CSS variables once; drive xterm theme + CodeMirror theme from the same source (Terax `theme` module pattern).

```css
:root {
  --bg: …;
  --bg-elevated: …;
  --panel: …;
  --border: …;
  --text: …;
  --muted: …;
  --accent: …;
  --danger: …;
  --warning: …;
  --focus-ring: …;
  --radius-sm: 4px;
  --radius-md: 8px;
  --font-ui: …;
  --font-mono: …;
  --tab-height: 36px;
  --statusbar-height: 28px;
}
```

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
| Save buffer | `Mod+S` | Already exists |
| Toggle sidebar | `Mod+B` (and `Mod+Shift+B`) | Terax: plain `Mod+B` may yield to focused terminal |
| Command palette | `Mod+P` | UI-2+ (Terax); find-in-files later `Mod+Shift+P` / content search |
| Connect / disconnect | — | Primary in strip; disconnect keeps remote session |

**Windows note:** `Ctrl+D` is also shell EOF. Prefer Terax behavior: shortcut wins when the host handles it for split; document that users who need raw EOF can remap. Do not silently switch to VS Code `\` bindings.

Tree: expand/collapse, open file (preview), pin on edit, keyboard nav (UI-2). Dirty editors show `•` in the tab label (Terax-style).

Connection errors and auth failures use inline strip status + optional toast; never modal loops.

## 7. Component inventory

### 7.1 Current → target

| Current (`ui/src`) | Target module |
|--------------------|---------------|
| Header inputs in `main.ts` | `chrome/ConnectionStrip` |
| `#tabs` + split buttons | `tabs/TabBar` + `tabs/model` |
| `#panes` two-up grid | `terminal/PaneTreeView` |
| xterm create/fit | `terminal/TerminalStack` (WebGL) |
| `#tree` | `explorer/FileTree` |
| `#editor-panel` dock | **Done (UI-1):** editor tabs in `#editor-stack` (CM6) |
| ad-hoc CSS | `styles/tokens.css` + region CSS |
| protocol types | keep `protocol.ts`; add layout types |

### 7.2 Shell quality bar (from Terax, adapted)

- Inactive terminal tabs remain mounted and continue receiving `pty_data`.
- Fit/resize only the visible leaves; debounce resize storms.
- Tree refresh stays silent (no “Loading…” flash); preserve expand/selection (already started).
- Large directories: expand stays **lazy** (one `fs_list` per opened dir). When a visible folder can expose thousands of rows, **virtualize** the tree viewport (mount only visible rows). Debounced/`WATCH_IGNORE`-style watch filtering stays required so `fs_changed` does not rebuild the world.
- Save conflicts: when backend grows mtime/CAS errors, show overwrite UI — never silent last-writer-wins (Terax editor lesson). Day-to-day editor chrome stays local-feeling; remote authority appears in the connection strip / status and on save or conflict failures only.

## 8. Stack decision

| Option | Status |
|--------|--------|
| **A. Vite + TypeScript modules** (current) | **Chosen.** UI-1 through UI-3 default. Large-tree perf = virtualization + watch discipline in the explorer, not a framework switch. |
| **B. Preact/React later** | Deferred escape hatch only if recursive pane trees become unmaintainable by hand — **not** for 10k-file trees. |
| **C. Full Terax clone (React 19 + Tailwind + shadcn)** | Rejected: high cost, product/AI divergence. |

## 9. Implementation phases

### Phase UI-0 — Design lock ✅ (this doc)

- Agree IA, tab model, non-goals; link from DESIGN.md.

### Phase UI-1 — Chrome + unified surfaces ✅

- Collapsible connection strip; status bar.
- Tokenized theme; restyle tree / tabs / panes without new features.
- Editor becomes tabs (remove permanent bottom dock).
- CodeMirror 6 + xterm WebGL.
- Tab pill animation; sidebar collapse + persisted width.
- Dirty / preview tab affordances.

### Phase UI-2 — Pane tree + keyboard ✅

- Recursive splits (max 4 leaves/tab, `src/panes.ts`); replaced the old global two-pane mode — each terminal tab now owns its own `PaneNode` tree and a `leaves: Map<ptyId, TermBundle>`.
- Shortcut registry (`src/shortcuts.ts`) wired via `installShortcuts`; command palette (`src/palette.ts`, `Mod+P`) lists all shortcuts and runs them by id.
- Tree keyboard navigation (arrow keys, Enter) via the virtualized tree (still text-only glyphs).
- Explorer: **virtualized rows** (`src/tree.ts` `VirtualTree`) — lazy one-level `fs_list` + windowed row rendering, so expanded views stay responsive with large trees.
- Richer layout blob (`version: 2`) in `layout_set` / localStorage, including each terminal tab's `paneTree` and `activeLeafId`.
- Simplifications: pane-tree restore across `session_attach` only recreates the exact multi-pane layout when the reattached ptys match a persisted tab's leaf ids 1:1; otherwise it falls back to one terminal tab per pty. Editor tabs are not restored across reattach (server-side buffers aren't rehydrated by path yet).

### Phase UI-3 — Depth ✅

- OSC 7 cwd → tab titles + inherited cwd for new tabs/splits (backend bash/zsh hooks in `pty.rs`; client `TermBundle.cwd` + `pty_open.cwd`).
- Header find bar for terminal buffer (`@xterm/addon-search`, `Mod+F`); editor uses CodeMirror search panel.
- Activity bar with Explorer (toggles sidebar) + Settings entry; explorer remains the only sidebar view until SCM exists.
- Lightweight file-type badges in the virtualized tree (`src/icons.ts`) — no heavy icon pack.
- Skipped `fs_list` pagination for now — row virtualization covers large trees; revisit only if a single directory listing becomes a protocol bottleneck.
- Light theme tokens (`data-theme`) + settings modal (theme, terminal/editor font size, WebGL preference) via `Mod+,` / activity bar.

### Out of UI phases (stay in DESIGN Phase 4+)

- Code signing, auto-update, Linux backend packages.
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
