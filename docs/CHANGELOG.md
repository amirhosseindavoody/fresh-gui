# Changelog

## 2026-09-22

### Shell tabs and remote sessions

- Shell tabs use an `alacritty_terminal` grid (Apache-2.0), the same library Fresh's TUI terminal wraps. The cursor is drawn, SGR colors are kept, and the alternate screen works, so vim, htop, and fish can run. Fresh `TerminalManager` is still not mounted: it belongs to Fresh's own view, not this GPUI window. libghostty was not integrated; it ships its own GPU renderer rather than a cell grid. The daemon PTY stays `portable-pty`.
- Each PTY child gets `TERM=xterm-256color` and `COLORTERM=truecolor`. Fish is started with `-i` and an init command that hooks `fish_prompt` to emit OSC 7, without changing the directory the PTY was given. The pane resizes the grid (about 8×18px cells). The wheel scrolls history, and on the alternate screen sends up/down. Shift+PageUp / Shift+PageDown scroll history.
- Drag on the grid selects text. `Ctrl+C` / `Cmd+C` copies that selection; with no selection, `Ctrl+C` is still the interrupt. The terminal's right-click and **···** menus have **Copy**. A click with no drag, or typing, clears the selection.
- OSC 52 copy (`\e]52;c;…`) writes the host clipboard, capped at 256 KiB. OSC 52 paste/read is not answered, so a program in the shell cannot read the host clipboard.
- A new shell starts in the workspace root, or the remote session root, when the client does not send a directory. The daemon no longer inherits `$HOME` just because the SSH process was started there. An explicit cwd (OSC 7 after `cd`) still wins. Switching workspaces forgets the previous shell's directory.
- Windows ConPTY spawn hides the child console (`STARTF_USESHOWWINDOW` / `SW_HIDE`) via a vendored `portable-pty` 0.9.0 (MIT) under `vendor/portable-pty`. `CREATE_NO_WINDOW` is not set on that child; it stops the conhost helper. This was not run on Windows in CI here.
- A remote window shows the Workspaces rail when the daemon advertises `workspace`, same as local. An older remote that is already running shows one row for the session root until that daemon is upgraded and restarted.

### Windows client menu, binary files, and Git

- Clicking the title bar no longer warns `a11y: set_focus called more than once in a single frame`. The dock tab frame tracks the focused panel's focus handle. Giving that frame a group role (so GPUI would stop noting a role-less id) made both nodes call `set_focus` on every redraw, including a title-bar click that does not move focus. The tab frame stays role-less and the panel is the only focus owner. GPUI still notes the role-less frame at info; the host filter (`gpui=warn`) hides that.
- Clicking a binary file no longer feeds those bytes to the editor. On Windows that showed up as `The data area passed to a system call is too small (0x8007007A)`. The daemon samples the first 8KB and refuses a file with a NUL or a run of control bytes (`binary_file`). The host opens a Binary file placeholder with **Open externally**. The daemon resolves that path inside the FS sandbox, then hands it to the OS opener.
- Closing the window with the Windows **X** can still log `window not found` and invalid window handles (`0x80040102`, `0x80070578`). Those come from GPUI calling `ShowWindow` / `DestroyWindow` after the HWND is already gone. The host publishes the layout inside the platform should-close hook, while the handle is still valid, and does not keep an HWND of its own.
- The title bar has a **File** menu (gpui-component `AppMenuBar`). **Stop Server** disconnects and, for a local daemon, runs the same stop as `fresh-gui close`. An SSH session only disconnects; the remote daemon keeps running. **Quit Client** closes the window and leaves the daemon running. Both are also in the command palette.
- **Source Control** on the activity bar (when `hello` includes `git`) replaces the explorer. It lists `git status` for the workspace root: stage, unstage, commit, pull, and push. Those run `git` on the daemon, with prompts disabled. Clicking a changed file opens a whole-file split diff; a second click pins it as a tab until closed. Diff tabs are not saved with the workspace. Right-click **Open Editor** still opens the text buffer. Protocol stays `0.4.0`; the new git and `fs_open_external` messages are additive.

### Windows client starts again

- v2026.922.10 panicked on launch with `cannot update DockArea while it is already being updated` and the window never appeared. The workspace dock applied tab-bar skin settings inside `cx.new`, before GPUI inserted the area, so the skin's notify leased a missing entity. Those settings run after the area exists. Tab bar, hidden dock-collapse button, and accessibility roles are unchanged.

### Workspaces persist across daemon restarts

- The daemon saves the workspace list to a private `workspaces.json` in its state directory (`~/.local/state/fresh-gui/` or `%LOCALAPPDATA%\fresh-gui\`) and loads it on start. Names, roots, tab titles and order, the active tab, open explorer folders, and the focused workspace survive `fresh-gui close` and a reboot. Writes are debounced and atomic; an unreadable file is moved aside to `workspaces.json.bad`. `--foreground` stays in memory unless `FRESH_GUI_WORKSPACES_FILE` is set.
- A restored terminal tab whose shell ended with the old daemon opens a new shell in the workspace root and keeps its title. A live one reattaches as before.
- The host now publishes the layout when an editor tab closes, when tabs are reordered, split, or merged, when a folder is expanded or collapsed, and before Disconnect, Reconnect, and window close (window close waits up to 500ms for the send). Before, those changes could be lost until the next tab activation.
- Restoring selects the saved active terminal even when editor tabs come before it in the strip.
- Explorer open folders are kept per workspace instead of reset on every switch. Protocol `0.4.0` gains an optional `explorer_expanded` on `workspace_layout_set` / `workspace_switched`; peers without it still parse.

### Explorer symlinks, terminal redraw, logging

- `FsEntry` gains an optional `target_kind`. A symlink whose canonical target is a folder inside the FS sandbox expands like a folder, with its children listed under the link path. Links out of the sandbox, dangling links, and file links are unchanged.
- Terminal output no longer redraws the whole window. The terminal panel still repaints itself on each chunk; the rail, explorer, dock, and status bar do not.
- The host's default log filter keeps GPUI's crates at `warn`, so routine GPUI info lines stay out of the launching shell. `RUST_LOG` still replaces the filter.
- README documents the Linux development packages the GPUI host needs to build and that the `vendor/fresh` submodule is required for any workspace build. Clippy is clean for the four project crates.

### Windows client tabs, explorer, and accessibility

- The new-terminal **+** clears the last tab. The shift is measured from the tab chrome (the title row plus the medium tab’s 12px padding and 1px border) with a 4px gap, so the button no longer sits on top of the tab.
- Explorer expand/collapse follows the folder you click. Open directories are an explicit set. A listing refresh no longer treats “we have this directory’s children” as “it is expanded”, which was reopening cached folders and closing one whose `fs_list` had not returned. A listed empty directory stays a folder.
- Explorer rows use Lucide file-type icons (code, braces, text, image, terminal, archive, and folder / folder-open, including git and source folders) tinted by kind, plus a chevron on folders the tree can expand. The host asset source adds those glyphs on top of the default gpui-kit bundle.
- Focused dock chrome and the explorer and terminal panes now have an accessibility role as well as an element id. GPUI was logging `focused element has no accessibility node (it has an id but no role)` on every focus change, and a role only on the panel made the tab-panel frame log again every frame. `window not found` / `Invalid window handle` in a short burst when the window closes come from GPUI’s own window teardown (`ShowWindow` / `DestroyWindow` after the HWND is gone), not from this host.

### Windows client path, tabs, and Ctrl+W

- Paths that Windows canonicalizes as `\\?\C:\...` or `\\?\UNC\...` show as a normal path (`C:\...`, `\\server\...`) in the explorer header, editor tab titles, workspace-rail root, status text, and copied paths. The extended prefix stays available to Win32 inside the daemon.
- Each dock tab has a close button. Right-click offers Close, Close Others, and Close to the Right, including on the last tab. The **···** menu adds Close Others and Close to the Right; the dock still supplies Close when the group can lose a tab. `Ctrl+W` still closes the active tab.
- The new-terminal **+** stays in the tab bar’s right-hand slot so it remains visible, and shifts left to sit beside the last tab while the bar has room.
- **New Workspace** prefills the project root with the open workspace’s folder (or the explorer root when that workspace has none). An empty root still means the daemon project root.
- Closing a tab with `Ctrl+W` no longer panics with `cannot update Workspace while it is already being updated`. The dock calls `on_removed` from inside the workspace update; that bookkeeping now waits until the update returns.

### Workspace rail closer to a spaces list

- The GPUI workspace column is 232px, to the left of the activity bar. Each row shows the display name and a shortened project root (`~` for a home prefix, **Default root** when the root is empty). The active row uses an accent fill and a left accent bar. Rename and Close show on hover and on right-click, instead of sitting on the active row. The last workspace still cannot be closed. One open workspace shows a short hint so the rail is not an empty strip.
- **+** opens a panel for the display name and an absolute project root. An empty name still becomes the folder name on the daemon. The command palette can create, rename, and close the focused workspace.
- Protocol `0.4.0` and capability `workspace` are unchanged.

### Windows terminal shows a PowerShell prompt

- A new terminal on Windows stayed a blank pane. ConPTY asks for the cursor (`CSI 6 n`) before it draws anything and waits for a cursor-position report; the host never answered, so the shell produced no cells. The terminal view now replies (`CSI row ; col R`) and answers a device-attributes query the same way.
- Empty `terminal.shell.args` also appended a Unix `-l`. Windows PowerShell runs that as the command `-l` and exits, so there is still no prompt after the cursor report. `powershell`, `pwsh`, and `cmd` now start as the interactive console process. bash and zsh still get the OSC 7 setup.
- The first Windows launch could also sit with no window. `fresh-gui --json` detaches the daemon with inherited standard handles, so the daemon kept the launcher's stdout pipe and the desktop app waited forever for that process to exit. A later launch attached to the daemon and showed the blank pane. The daemon spawn now clears the inherit flag on the launcher's standard handles before `CreateProcess`.

### GPUI host stays up after the first terminal activates

- Opening the window no longer panics with `cannot read TerminalPanel while it is already being updated` once the dock marks that terminal active. The dock delivers `set_active` inside the panel's own update, and publishing the workspace tab list was reading the panel (its title) before that update returned. The publish now runs after the update releases the entity. Numbered tabs, splits, and workspace layout saves are unchanged.

### One command: `fresh-gui`

- The GPUI host is the user-facing command. With no arguments it reuses the per-user daemon session or starts `fresh-gui-daemon` (Cargo target dir: the sibling `fresh-gui` binary), then opens the window. The loopback token is read from session meta via the daemon's hidden `--json` output, not pasted onto the GUI argv.
- `fresh-gui /path/to/project` uses that directory as the daemon root when it starts the session. If a session is already running, the host switches to a workspace with that root or creates one.
- `fresh-gui user@host` and `fresh-gui <saved-name>` run the existing SSH bootstrap (install or start the remote daemon, tunnel `/ws`, open the window). `fresh-gui remote add|connect|list|remove|daemon` stay. `fresh-gui status` and `fresh-gui close` forward to the daemon binary.
- `fresh-gui --no-ui` starts or reuses the local daemon and does not open a window, so a remote `fresh-gui --no-ui` still works when PATH points at the desktop command. SCP still installs the headless archive member as `~/.local/bin/fresh-gui`.
- Installers (`scripts/install.sh`, `scripts/install.ps1`) place the host on `PATH` as `fresh-gui` (and `fresh-gui-app` as another name for the same file). When both archives are installed the daemon is `fresh-gui-daemon`. A daemon-only install keeps the headless binary named `fresh-gui`. `scripts/package-client.sh` now stores the host in the client archive as `fresh-gui` / `fresh-gui.exe`. The ADE protocol is unchanged.
- The Linux GPUI client release job checks the packaged tarball for a `fresh-gui` member (the name `scripts/package-client.sh` writes). The Windows job has no member-name check. Release notes from that workflow use `./fresh-gui` and `fresh-gui.exe`.

### Dock splits, numbered terminals, explorer multi-select

- Terminal and editor tabs are gpui-component dock panels. Drag a tab to a pane edge for a horizontal or vertical split, drop it on another tab to merge, and drag within the strip to reorder. Empty groups collapse. The last remaining tab cannot be dragged (the dock refuses a drag from a lone group).
- The dock tab strip is the skin default 32px bar. The new-terminal **+** is in each panel’s `title_suffix`, at the far right of the active group beside the **···** menu. Title, rail, explorer header, tree rows, and status bar stay at the denser sizes from 2026-09-21.
- New terminal tabs are titled `1`, `2`, `3`, … inside the focused workspace (numbers are not reused there). Right-click the tab title or **··· → Rename** sets a custom title. `SessionTabTitle.workspace_id` is the workspace that owns the tab. OSC 7 still records cwd for the next PTY and no longer replaces the tab title.

### Multi-workspace manager

- The daemon keeps several named workspaces in the one per-user process. Each workspace has an id, display name, project root, and its own ADE session (PTYs, scrollback, tab list). Switching attaches that session and leaves the others running. Closing the GPUI window does not drop them.
- Protocol `0.4.0` gains capability `workspace` and messages `workspace_list` / `workspace_create` / `workspace_rename` / `workspace_close` / `workspace_switch` / `workspace_layout_set`. Older `session_*` clients still connect.
- The GPUI host shows a workspace rail to the left of the activity bar: create (name and optional absolute root), switch, rename, and close. The focused workspace has its own dock (splits, numbered titles, explorer multi-select). A terminal or file opened in one workspace is not added to another workspace’s tab strip. Switching saves a flat tab list, so split geometry is rebuilt as one group.
- Design: [docs/WORKSPACES.md](./WORKSPACES.md).
- The explorer multi-selects with Ctrl/Cmd-click and Shift-click. **Copy Path** writes absolute paths (ADE `FsEntry.path`), one per line. Dragging a file onto a folder sends `fs_move`. With the explorer focused, Ctrl/Cmd+C arms an in-app file clipboard (and writes the same paths plus GPUI `ExternalPaths`); Ctrl/Cmd+V pastes with `fs_copy` into the selected folder or the parent of a selected file. This GPUI snapshot does not offer `text/uri-list` on Linux clipboard writes, so pasting into a file manager is not reliable.

## 2026-09-21

### PTY shell fallback when the default is missing

- On Unix, opening a terminal no longer stops at `pty_open_failed: spawn shell zsh` when `zsh` is not installed. Spawn tries the configured command (default `zsh`, or the client `shell` override), then `$SHELL` when that binary is usable, then `bash`, then `sh`. The choice is logged, including which candidates were skipped. If every candidate is missing or not executable, the error names the attempts and tells you to set `terminal.shell.command` in `config.json`. A spawn that still fails uses the same hint. Windows keeps `powershell` and does not walk the Unix chain.

### One-line installers

- `scripts/install.sh` (`curl -fsSL …/scripts/install.sh | sh`) and `scripts/install.ps1` (`irm …/scripts/install.ps1 | iex`) download the GitHub Release for this machine, verify the sibling `.sha256` when it is published, and install `fresh-gui-app` plus `fresh-gui` into `~/.fresh-gui/bin` (Windows: `%USERPROFILE%\.fresh-gui\bin`), then update PATH.
- `latest` follows the Releases redirect (archive names include the CalVer). `FRESH_GUI_VERSION` / `-FreshGuiVersion` pins a tag. `FRESH_GUI_HOME`, `FRESH_GUI_REPOURL`, `FRESH_GUI_COMPONENTS` (`both` / `client` / `daemon`), and `FRESH_GUI_NO_PATH_UPDATE` / `-NoPathUpdate` match the pixi installer shape. Linux prefers the gnu assets; `FRESH_GUI_LIBC=musl` selects the musl daemon (the GPUI client is gnu-only).
- Windows release assets named `.zip` are GNU tar archives today (`tar -a` when Info-ZIP is absent). The installers extract those with `tar`, and use unzip / `Expand-Archive` when the file is a real PK zip.

### Denser GPUI chrome

- Title bar (30px), activity rail (36px, small icon buttons), explorer header (26px), tree rows (22px), tab strip (`TabBar::small()`, 24px), and status bar (22px) drop the medium gpui-component padding and min heights that made the shell look sparse next to VS Code / Zed.
- The new-terminal **+** is an xsmall button in the tab bar’s `last_empty_space`, immediately after the last tab. A zero-width suffix only exists so gpui-component mounts that slot; the button is no longer a right-aligned control.

### Linux GPUI client on GitHub Releases

- The release workflow builds `fresh-gui-app` for `x86_64-unknown-linux-gnu` on `ubuntu-latest` and uploads `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` plus `.sha256`, using the same `scripts/package-client.sh` layout as the Windows client. Daemon archives stay a separate matrix and are headless `bin/fresh-gui`.
- Pull requests that touch the client build compile and package that archive without bumping CalVer or publishing a Release (the bump job stays push/dispatch-only).
- Linux runtime: glibc ≥ 2.39, an X11 or Wayland session, fontconfig, and a Vulkan loader (`libvulkan.so.1` is loaded on demand). Directly linked libraries are libxcb and libxkbcommon. This is not the daemon's glibc 2.31 zigbuild.
- Daemon packages (`.conda` and standalone archives) no longer include `share/fresh-gui/ui`. Release CI does not build a browser shell. The daemon is headless ADE (`/ws` + `/healthz`). `--no-ui` is still accepted so existing SSH connect commands keep working.
- Removed the Vite/React host (`crates/fresh-gui-app/ui`), `serve-ui`, and `--ui-dir`. A later web UI would be the GPUI client via WebAssembly, which is not in this change.

### SSH remote bootstrap for the GPUI host

- `fresh-gui-app remote add|list|remove|connect` saves OpenSSH targets (`user@host` or a `Host` alias) in `remotes.json`. Auth stays with the system `ssh` client (no password prompt).
- Connect probes the remote for a `fresh-gui` binary and a live session. If the binary is missing it copies a Linux daemon (local path, release URL, or the latest GitHub linux-gnu asset) to `~/.local/bin/fresh-gui`, starts `fresh-gui --no-ui`, reads the token from `session.json`, and opens a loopback tunnel into the GPUI window.
- Release workflow also publishes `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` (GPUI host). Daemon archives are the headless binary, separate from the client.

### Fresh pin and standalone release binaries

- Vendored Fresh moved to fork `master` `14f7d28b7ab18b6cdefc75ab94c5df34044ae3d0` ([fresh#4](https://github.com/amirhosseindavoody/fresh/pull/4)). The change from `ddfc322` is CI/plugin-test only. Embedding still uses `fresh-editor` feature `runtime` only.
- GitHub Releases publish standalone daemon archives beside the linux-64 `.conda` package: linux-gnu (glibc ≥ 2.31), linux-musl, and windows-msvc (`scripts/package-binary.sh`). Those archives were first published with `share/fresh-gui/ui`; later the same day they became headless `bin/fresh-gui` only (see above). The native GPUI host (`fresh-gui-app`) is a separate client asset.
- Daemon session lock/spawn is split into unix/windows modules (Fresh daemon pattern: `setsid` vs `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`) so the Windows binary builds. Windows session files live under `%LOCALAPPDATA%\fresh-gui\`; config defaults to `%APPDATA%\fresh-gui\config.json`.

### Relicense to GPL-3.0-or-later

- Replaced the root `LICENSE` with GNU GPL Version 3 (same text as Fresh) and set workspace / packaging SPDX to `GPL-3.0-or-later` so fresh-gui matches [Fresh](https://github.com/sinelaw/fresh) (`GPL-3.0-or-later`).

### Native GPUI host + Fresh pin toward upstream

- Primary host is now a native **gpui-kit 0.6.6** / gpui-component desktop shell (`fresh-gui-app`, default subcommand). It speaks existing ADE `/ws` JSON via `fresh-gui-client` (protocol unchanged). The Vite/React shell that still existed at this point was removed later the same day.
- Workspace: activity bar, collapsible explorer, unified terminal/editor tabs, status bar, command palette, Go to File. Terminal is a VTE view of remote PTY bytes; editor tabs are gpui `Editor` views of Fresh snapshots (save = `buffer_edit` + `buffer_save`).
- `gpui` / `gpui_platform` pinned to **gpui-pre 0.3.6** (the snapshot gpui-component requires) to avoid duplicate-crate type errors.
- Vendored Fresh pinned to fork `master` `ddfc322bc977fc21c4c837ae79906c04a5717c58` after [fresh#3](https://github.com/amirhosseindavoody/fresh/pull/3) merged (same tree as trial `31c311bfa44fbbdb4c8b258357af3c1d7d0e81c6`). `cargo check -p fresh-gui` succeeded with no embedding API changes. Project license is **GPL-3.0-or-later**, matching upstream Fresh.
- Linux GUI needs X11 or Wayland, fontconfig, and wgpu/Vulkan. Native v1 gaps: splits, markdown WYSIWYG, minimap, context menus, layout v4 restore, mouse selection in the terminal, find, palettes, tab pin/reorder.

## 2026-07-30

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
