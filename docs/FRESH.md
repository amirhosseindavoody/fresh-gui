# Fresh integration

How **fresh-gui** embeds and talks to [Fresh](https://github.com/sinelaw/fresh). Product architecture: [DESIGN.md](./DESIGN.md). Host UI: [UI.md](./UI.md).

## 1. Role of Fresh

Fresh is the **remote buffer authority** inside the Linux daemon (`fresh-gui`). The host UI (browser) never links Fresh crates. It speaks the ADE WebSocket protocol; the daemon translates editor messages into in-process Fresh `Editor` calls.

```
┌──────────────────────┐         ADE /ws (JSON)         ┌────────────────────────────┐
│  Host UI             │◄──────────────────────────────►│  fresh-gui (Linux)          │
│  CodeMirror, xterm,  │   editor_open / buffer_edit /  │  EditorHandle ──► Fresh     │
│  React chrome        │   buffer_save / …              │  Editor (!Send thread)      │
└──────────────────────┘                                 │  vendor/fresh (submodule)   │
                                                         └────────────────────────────┘
```

**Fresh owns:** open file → buffer text, language mode, disk save via Fresh’s filesystem, path-link / `:line:col` parsing helpers.

**fresh-gui owns:** ADE protocol, sessions, PTY, FS sandbox for the explorer, host rendering (CodeMirror / xterm), revision CAS on the wire, config chrome (`ui.*`).

This is intentional: the wire is a **PTY-first ADE protocol**, not Fresh `--web` scene envelopes (see DESIGN D1).

## 2. Vendoring and build

| Piece | Behavior |
|-------|----------|
| Submodule | `vendor/fresh` → integration fork `https://github.com/amirhosseindavoody/fresh.git` (upstream: sinelaw/fresh) |
| Pin | Commit SHA in the submodule **and** `vendor/fresh.rev` (for package builds without submodule checkout) |
| Workspace | Root `Cargo.toml` `exclude = ["vendor/fresh"]` so Fresh keeps its own Cargo workspace |
| Link | Only `crates/fresh-gui` depends on Fresh |

Cargo dependency (`crates/fresh-gui/Cargo.toml`):

```toml
fresh = {
  path = "../../vendor/fresh/crates/fresh-editor",
  package = "fresh-editor",
  default-features = false,
  features = ["runtime"]
}
```

The Rust crate name imported in code is `fresh` (lib name of `fresh-editor`).

### Features

| Feature | Status | Why |
|---------|--------|-----|
| `runtime` | **on** | Editor core, languages, syntect, etc. |
| `default` | **off** | Avoid pulling unused Fresh defaults |
| `plugins` / `embed-plugins` | **off** | Embedding is buffer open/edit/save only |
| `tree-sitter` | **off** | Not used by the ADE host |
| `web` | **off** | No Fresh `--web` / web-ui |
| `gui` | **off** | Fresh’s own wgpu GUI crate is not linked (name collision only with this repo’s `fresh-gui`) |

Package builds (`recipe/build.sh`) call `ensure_vendor_fresh()`: init the submodule if possible, otherwise shallow-fetch `vendor/fresh.rev`.

### Bumping Fresh

```bash
git -C vendor/fresh fetch
git -C vendor/fresh checkout --detach <rev>
# stage submodule + write the same SHA to vendor/fresh.rev
```

## 3. In-process editor (`EditorWorker`)

Fresh’s `Editor` is **`!Send`**. The daemon therefore runs it on a dedicated OS thread (`"fresh-editor"`) with a current-thread Tokio runtime. All ADE editor ops go through a cloneable `EditorHandle` that serializes commands onto that thread.

Implementation: `crates/fresh-gui/src/editor_worker.rs`.

### Lifecycle

1. After bind, `main` calls `EditorHandle::spawn(working_dir)` unless `--no-editor` / `FRESH_GUI_NO_EDITOR`.
2. Worker builds Fresh config via `Config::load_with_layers` under a private temp state dir (`/tmp/fresh-gui-editor-{pid}`), forces `animations = false`, and constructs `Editor::with_working_dir` with `StdFileSystem`.
3. On failure to spawn, `AppState.editor` stays `None` and Hello omits `editor` / `scene` capabilities.
4. `Editor` is dropped **after** the worker’s `block_on` returns (avoids Drop during Tokio async teardown).

### Handle API ↔ Fresh

| `EditorHandle` | Fresh / ADE behavior |
|----------------|----------------------|
| `open(path, preview)` | `open_file` / `open_file_preview`; snapshot text + language; track path/rev |
| `edit(buffer_id, base_rev, text)` | Activate buffer, CAS on ADE `rev`, `replace_content` with full text |
| `save(buffer_id, base_rev)` | CAS then Fresh `Editor::save` |
| `close(buffer_id)` | Drop ADE tracking only — does **not** call a Fresh close API |
| `scene()` | List tracked buffers + Fresh `active_buffer()` for thin ADE scene |

Limits: snapshots larger than **2 MiB** are rejected (`MAX_SNAPSHOT_BYTES`). Large-file unloaded regions in Fresh can make `buffer.to_string()` fail.

### Revisions

`buffer_id` is Fresh’s `BufferId` as a decimal string. **`rev` is ADE-side** (starts at `0`, increments on successful edit/save). It is not Fresh’s internal undo revision. The host CodeMirror document is the interactive view; the daemon’s tracked rev is the conflict token on the wire.

### `--no-editor`

- Skips `EditorHandle::spawn`.
- Strips `editor` and `scene` from Hello capabilities.
- Editor/scene messages return `editor_unavailable` / `scene_unavailable`.
- PTY, session, and FS still work.

## 4. Protocol surface

Capability `editor` (and optional `scene`) on the same `/ws` connection as PTY/FS.

| Direction | Messages |
|-----------|----------|
| Open | `editor_open` → `editor_opened` + `buffer_snapshot` |
| Ctrl/Cmd+click | `editor_open_link` → same opened + snapshot pair |
| Edit | `buffer_edit` (`base_rev`, full `text`) → `buffer_changed` (new `rev`) or `error` |
| Save | `buffer_save` → `buffer_saved` (path + `rev`) |
| Close | `editor_close` (ADE tracking) |
| Scene | `scene_get` → `scene_snapshot` (open-buffer list for chrome — **not** Fresh `--web` cell scene) |

Open replies are produced in `server.rs` (`reply_editor_opened`). Settings `config.json` is a special open path: the file is created/hydrated if missing, then opened like any other buffer; a successful save of that path reloads live UI prefs.

## 5. Path resolution and link open

`crates/fresh-gui/src/path_open.rs` reuses Fresh detectors so terminal and quick-open behavior match Fresh:

| Fresh API | Use |
|-----------|-----|
| `parse_path_line_col` | `path:line` / `path:line:col` suffixes on `editor_open` |
| `expand_tilde` | `~` expansion |
| `detect_link_at` | `editor_open_link` from a line of text + column |

Candidate order matches Fresh `terminal_link`: absolute (after `~`), then terminal OSC 7 `cwd`, then the FS sandbox root. Relative paths under a cwd outside `--root` may call `FsRoot::authorize` so explorer and editor stay consistent with Terax-style cwd sync.

Line/column from path or link are returned on `editor_opened` for the **host** to reveal in CodeMirror. The Fresh in-process cursor is not moved for that jump.

## 6. What uses Fresh vs what does not

### From Fresh

- In-process `Editor` for buffer open / content replace / save.
- `path_link` + quick-open path parsing.
- Theme **color values** copied into host palettes (`ui/src/palettes.ts` maps RGB from `vendor/fresh/crates/fresh-editor/themes/*.json` onto CSS variables for chrome, xterm, and CodeMirror). Primer remains a host-native palette in `tokens.css`.
- `terminal.shell.{command,args}` shape aligned with Fresh’s shell config (stored under fresh-gui’s `config.json`, not Fresh’s config directory).

### Not from Fresh (by design)

| Area | fresh-gui implementation |
|------|--------------------------|
| Wire protocol | ADE JSON over `/ws` — not Fresh `--web` scene |
| PTY | Host `portable-pty` + OSC 7 hooks (`pty.rs`); Fresh’s `TerminalManager` unused |
| Explorer FS | Sandboxed `fs.rs` / `fs_watch.rs` (list, create, copy, move, watch). Fresh `StdFileSystem` is only used inside the editor for buffer I/O |
| Host editing UX | CodeMirror 6 (syntax highlight, minimap, search); markdown tabs also get a host WYSIWYG preview (`markdown-wysiwyg.ts`) because Fresh Compose/Page View is a plugin and plugins are not enabled on the ADE path |
| Host terminal UX | xterm.js WebGL |
| Host chrome | React + Tailwind + shadcn |
| Plugins / LSP / tree-sitter in the ADE path | Features off; not exposed over the protocol |
| Orchestrator / coding agents | Fresh plugin not loaded; agent direction for ADE is design-only ([COPILOT.md](./COPILOT.md)) — steal registry/resume patterns, do not embed Orchestrator yet |
| Session / explorer restore | Host layout blob in Rust `SessionStore` (`layout_set`); explorer expanded/scroll snapshots mirror Fresh `FileExplorerState` fields without using Fresh workspace files under `$XDG_DATA_HOME/fresh/workspaces` (ADE uses host `VirtualTree` + sandboxed FS) |

## 7. Host UI wiring

The React shell does not import Fresh. Imperative ADE code in `crates/fresh-gui-app/ui`:

| Module | Role |
|--------|------|
| `ade/bootstrap.ts` | `editor_open` / `editor_open_link`, buffer edit/save CAS, tab presentation, session layout restore |
| `layout-persist.ts` | Layout blob v4 schema + restore planner (multi-tab PTYs, editors, explorer snapshots) |
| `editor.ts` | CodeMirror view; applies snapshot text; reveals line/col from open |
| `markdown-preview.ts` / `markdown-wysiwyg.ts` | Host markdown render + editable preview; DOM is live while open, flushes markdown into CodeMirror for ADE save (Fresh Compose plugin is not on the ADE path — see §6) |
| `path-link.ts` | Host-side hover detector mirrored for UX; open still goes through backend Fresh `detect_link_at` |
| `palettes.ts` | Fresh theme colors → CSS tokens |
| `terminal.ts` / `osc7.ts` | PTY I/O and cwd (feeds open/link `cwd`) |

Workspace rule: prefer extending Fresh-backed backend surfaces over inventing a second editor engine in the TypeScript UI (see `.cursor/rules/leverage-fresh-editor.mdc`).

## 8. Config touchpoints

| Location | Relation to Fresh |
|----------|-------------------|
| `~/.config/fresh-gui/config.json` | fresh-gui daemon config (JSONC) |
| `terminal.shell` | Same field shape as Fresh shell config; empty `args` keep interactive + OSC 7 setup |
| `ui.editorLineWrap` | Host soft wrap; mirrors Fresh `editor.line_wrap` (default on). Toggle via `Alt+Z` / command palette |
| `ui.*` | Host-only prefs → `Hello.ui` |
| Fresh editor state dir | Ephemeral under `/tmp/fresh-gui-editor-{pid}` for the embedded `Editor` |
| Fresh user config | Not loaded wholesale into the ADE daemon |

## 9. Key source files

| Path | Role |
|------|------|
| `vendor/fresh/` | Fresh submodule tree |
| `vendor/fresh.rev` | Packaging pin SHA |
| `crates/fresh-gui/Cargo.toml` | Sole Fresh path dependency |
| `crates/fresh-gui/src/editor_worker.rs` | `!Send` Fresh thread + `EditorHandle` |
| `crates/fresh-gui/src/path_open.rs` | Fresh path/link resolve + sandbox |
| `crates/fresh-gui/src/server.rs` | ADE handlers, capabilities, settings open/reload |
| `crates/fresh-gui/src/main.rs` | `--no-editor`, spawn after bind |
| `crates/fresh-gui/src/pty.rs` | Host PTY (not Fresh terminal) |
| `crates/fresh-gui/src/fs.rs` | Explorer FS sandbox |
| `crates/fresh-gui-protocol/src/lib.rs` | `editor_*` / `buffer_*` / `scene_*` messages |
| `crates/fresh-gui-app/ui/src/ade/bootstrap.ts` | Host editor protocol controller |
| `crates/fresh-gui-app/ui/src/palettes.ts` | Fresh theme colors → host tokens |
| `recipe/build.sh` | Ensure Fresh pin for package builds |

## 10. Operational summary

1. Clone with submodules (or let `recipe/build.sh` fetch `fresh.rev`).
2. `fresh-gui` starts → optional Fresh `Editor` thread → Hello advertises `editor` + `scene`.
3. Host opens a path → ADE `editor_open` → Fresh open → snapshot to CodeMirror.
4. Edits replace full buffer text under ADE revision CAS; save writes through Fresh.
5. Disable embedding with `--no-editor` for a PTY/FS-only daemon.
