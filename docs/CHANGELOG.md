# Changelog

## 2026-09-21

### One-line installers

- `scripts/install.sh` (`curl -fsSL …/scripts/install.sh | sh`) and `scripts/install.ps1` (`irm …/scripts/install.ps1 | iex`) download the GitHub Release for this machine, verify the sibling `.sha256` when it is published, and install `fresh-gui-app` plus `fresh-gui` into `~/.fresh-gui/bin` (Windows: `%USERPROFILE%\.fresh-gui\bin`), then update PATH.
- `latest` follows the Releases redirect (archive names include the CalVer). `FRESH_GUI_VERSION` / `-FreshGuiVersion` pins a tag. `FRESH_GUI_HOME`, `FRESH_GUI_REPOURL`, `FRESH_GUI_COMPONENTS` (`both` / `client` / `daemon`), and `FRESH_GUI_NO_PATH_UPDATE` / `-NoPathUpdate` match the pixi installer shape. Linux prefers the gnu assets; `FRESH_GUI_LIBC=musl` selects the musl daemon (the GPUI client is gnu-only).

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
