# fresh-gui Design

Terminal-first IDE shell on a **local host**, connected to a **Linux remote daemon** that embeds [Fresh](https://github.com/sinelaw/fresh) for editor state. Product inspiration: [Terax](https://github.com/crynta/terax-ai). Install and usage: [README.md](../README.md). Host chrome: [UI.md](./UI.md). Fresh embedding: [FRESH.md](./FRESH.md). Access model: [SECURITY.md](./SECURITY.md).

## 1. Problem

Developers often keep a Windows or macOS laptop as the interactive machine and a Linux box (WSL, lab node, cloud VM) as the real workspace. Alternatives either stream a full remote IDE, put a thin browser UI on the same machine as the editor (`fresh --web`), or ship a native GUI in-process with the editor.

**fresh-gui** uses a different split: a **local, terminal-first ADE shell** that speaks a versioned WebSocket protocol to a **Fresh-backed daemon on the remote machine**. The remote owns files, PTYs, and editor buffers; the host owns windowing, layout, and input devices.

## 2. Goals and non-goals

### Goals

- **Split deployment:** native GPUI host ↔ Linux remote backend.
- **Terminal-first UX:** multi-tab / split PTY as the primary surface, with editor and explorer as peers.
- **Fresh as backend of truth:** reuse Fresh crates for buffer/editor semantics; do not re-implement editor core in the host UI.
- **Pixi + Cargo workspace** for reproducible Rust development.
- **Documented, versioned protocol** between host and remote with capability negotiation.
- **Secure-by-default remote access:** always-on bearer token; loopback bind + SSH tunnel as the supported remote path.

### Non-goals

- Feature parity with Terax AI chrome (composer rail, agent diffs marketplace) or theme marketplaces / web preview panes.
- Embedding the VS Code Copilot extension, or treating Copilot CLI as an inline-autocomplete engine (see [COPILOT.md](./COPILOT.md)).
- Replacing Fresh’s TUI or shipping as a fork of Fresh.
- Multi-user collaborative editing.
- Publishing to winget / conda-forge as a primary channel (the install scripts, Pixi global install, and GitHub Releases cover distribution today).
- Public TLS / `wss://` exposure by default (SSH tunnel is the remote-access answer).

**Agent direction (design only):** terminal-first Copilot CLI / ACP integration is under design in [COPILOT.md](./COPILOT.md) — not shipped.

## 3. Prior art

### Terax

[Terax](https://github.com/crynta/terax-ai): Tauri 2 + Rust + React, terminal-first ADE with WebGL xterm and CodeMirror. **Local monolith**—PTY and UI share one process. fresh-gui borrows layout and “terminal is the hero” product sense, not the in-process architecture.

### Fresh

Fresh is a high-performance terminal editor (Rust) with a TUI, an in-process GUI surface, and `fresh --web` over a scene WebSocket. Orchestrator already models SSH / remote workspaces. This project adds a **host-native ADE GUI** that treats Fresh as an explicit remote service via a PTY-first ADE protocol (not Fresh `--web` scene envelopes).

### pixi-mise

Logistics template: `pixi.toml` + Cargo workspace under `crates/`, CalVer `YYYY.MMDD.N`, `scripts/update-version.sh`, optional `recipe/` for packaging.

## 4. Architecture overview

```
┌─────────────────────────────┐         ┌──────────────────────────────────┐
│  Host                       │         │  Remote (Linux)                  │
│                             │  SSH    │                                  │
│  Native GPUI shell          │  tunnel │  fresh-gui                       │
│    activity, tabs, chrome   │◄───────►│    sessions, PTY, FS, config     │
│    VTE view + gpui Editor   │  /ws    │    embeds Fresh Editor (optional)│
│                             │  JSON   │                                  │
│  CLI                        │         │  vendor/fresh (submodule)        │
└─────────────────────────────┘         └──────────────────────────────────┘
                 ▲                                        ▲
                 └──────── fresh-gui-protocol ────────────┘
```

### Crates

| Crate | Binary? | Role |
|-------|---------|------|
| `fresh-gui-protocol` | no | Versioned messages, capability constants, errors |
| `fresh-gui` | yes (`fresh-gui`) | Headless daemon: WebSocket ADE, sessions, PTY, FS, Fresh editor (Linux primary; Windows binary also released). Cargo and the daemon archive keep this file name. |
| `fresh-gui-client` | no | Dial, auth, typed request helpers |
| `fresh-gui-app` | yes (`fresh-gui-app`) | Native GPUI host. Installers put it on `PATH` as `fresh-gui` (and keep `fresh-gui-app` as a second name). CLI: open, `status` / `close`, `ping` / `smoke` / `attach` / `remote` |

### Process model

1. One daemon process per user holds the session lock (background session by default, or `--foreground` for tests). Fresh Editor runs in-process on a dedicated thread; PTY shells are child processes. Linux is the documented remote; the same daemon binary is also released for Windows. The desktop command starts this process when `fresh-gui status` would report nothing. A daemon-only install (Pixi, musl, the SCP'd `~/.local/bin/fresh-gui`) still starts it by running `fresh-gui`.
2. The native host authenticates and creates/attaches a session. With the installer, `fresh-gui` (no URL) reads the token from the daemon's private session meta via `fresh-gui-daemon --json` on a local pipe. `--backend` with a Local access URL (`?token=`) or `ws://…/ws` plus `FRESH_GUI_TOKEN` still connects explicitly. The daemon serves `/ws` and `/healthz` only.
3. After `hello` + `auth`, the client lists **workspaces** (or creates the default one) and switches to the focused workspace. Each workspace owns one ADE session. The daemon still accepts `layout_set` v4; the GPUI host restores workspace tab lists (`workspace_layout_set`, layout JSON v5) instead of that older blob.
4. Terminal panes map to remote PTYs in the focused workspace’s session. Explorer and editor talk to sandboxed FS / Fresh buffer APIs over the same socket. Other workspaces stay attached on the daemon with their own PTYs.
5. Disconnect detaches the WebSocket subscriber; every workspace, its session, and its PTYs keep running for reattach. See [WORKSPACES.md](./WORKSPACES.md).

## 5. Protocol

Wire format: **JSON text frames** over WebSocket at `/ws`. Protocol version is negotiated in `hello` and must match exactly (`PROTOCOL_VERSION`, currently `0.4.0`). PTY payloads use standard base64 in `pty_data`. Message shapes live in `fresh-gui-protocol`.

This is a **new ADE protocol**, not Fresh `--web` scene. Fresh Editor is an optional capability on top of PTY / session / FS.

### Capabilities

Default backend capabilities (omit `editor` / `scene` with `--no-editor`):

| Capability | Role |
|------------|------|
| `ping` | Liveness |
| `pty` | Create / data / resize / close |
| `session` | Create / attach / list; `layout_set`; PTYs belong to a session |
| `workspace` | Several named workspaces in the one daemon; each owns a session and a tab list. Switching moves the subscriber |
| `fs` | List, authorize, stat, watch; create / copy / move under the sandbox |
| `editor` | Open / edit / save / close via embedded Fresh |
| `scene` | Thin ADE open-buffer list (`scene_get` / `scene_snapshot`) |

### Message families

- **Control** — `hello`, `auth` / `auth_ok` / `auth_error`, `ping` / `pong`, `error`.
- **Session** — `session_create` / `session_attach` / `session_list`, `layout_set`.
- **Workspace** — `workspace_list` / `workspace_create` / `workspace_rename` / `workspace_close` / `workspace_switch` / `workspace_layout_set`. Additive on protocol `0.4.0` (new capability + messages; old clients keep using sessions). Model: [WORKSPACES.md](./WORKSPACES.md).
- **PTY** — `pty_open` (optional `cwd` / `shell`), `pty_data`, `pty_resize`, `pty_close` / `pty_closed`.
- **FS** — `fs_list` / `fs_stat` / `fs_authorize`; `fs_watch` / `fs_unwatch` / `fs_changed`; `fs_create` / `fs_copy` / `fs_move` / `fs_delete` (and matching result messages). Paths are sandboxed under `--root` / `FRESH_GUI_FS_ROOT`, plus directories authorized via `fs_authorize` (terminal cwd sync outside the primary root). Delete refuses the primary root and authorized cwd roots.
- **Editor** — `editor_open` / `editor_open_link` / `editor_opened`, `buffer_snapshot`, `buffer_edit` / `buffer_changed`, `buffer_save` / `buffer_saved`, `editor_close` (revision CAS on edit/save).
- **Scene** — `scene_get` / `scene_snapshot` (open buffers for host chrome; not Fresh cell scene).

## 6. Backend behavior

### Daemon session

Default `fresh-gui` detaches a **per-user background session** (exclusive lock under `$XDG_RUNTIME_DIR/fresh-gui/` on Unix, or `%LOCALAPPDATA%\fresh-gui\` on Windows), prints status / Local access URL, and returns the shell. Re-running reprints status; `fresh-gui close` stops the daemon. Logs go to `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` on Unix (fallback `~/.local/state/fresh-gui/`) or `%LOCALAPPDATA%\fresh-gui\fresh-gui.log` on Windows. Detach follows Fresh’s daemon pattern (`setsid` on Unix; `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP` on Windows). See [SECURITY.md](./SECURITY.md) for token handling in `session.json`.

While serving, the daemon samples its own resident set from `/proc/self/status` (`VmRSS` / `VmHWM`) about every 30 seconds. On SIGTERM / Ctrl-C (before graceful drain completes) it logs structured **average** and **peak** RSS in MB. Measurement covers the backend process only (Axum server, session state, embedded Fresh editor) — not PTY child shells. Fresh has no production memory monitor API; this is host-lifecycle telemetry.

### Sessions and PTYs

`SessionStore` holds multi-PTY sessions. `WorkspaceStore` groups them: each workspace has an id, display name, root directory, one session, a flat tab list, the active tab, and the explorer's open folders. The GPUI host switches workspaces without restarting the daemon; idle sessions keep their PTYs and tab lists. Closing the WebSocket detaches the subscriber; PTYs keep running. Reattach replays ~64KB of scrollback per PTY. The GPUI host restores from the structured list (`workspace_layout_set` / `workspace_switched.tabs` and `explorer_expanded`); the daemon still mirrors a v5 JSON blob onto the session for `session_attach` clients, and still accepts the older `layout_set` blob from them.

The background daemon saves the workspace list to a private `workspaces.json` in its state directory after each change (debounced, atomic rename) and reloads it on start, so workspaces, tabs, and open folders survive `fresh-gui close` and a reboot. PTYs do not: a restored terminal tab with a dead `pty_id` is respawned by the host with its saved title. Split geometry is not saved. Details: [WORKSPACES.md](./WORKSPACES.md#persistence).

PTY shell defaults come from `config.json` (`terminal.shell`; Unix default command `zsh`, Windows `powershell`). On Unix, spawn checks that the chosen command exists and is executable before starting it. When it does not, the order is the configured command (or a client `pty_open` shell), then `$SHELL` if that binary is usable, then `bash`, then `sh`. The selected shell and any skips are logged. If every candidate fails, `pty_open_failed` names the attempts and points at `terminal.shell.command` in `config.json`. Windows does not walk that chain. Bash/zsh hooks emit OSC 7 so the host can track cwd for new tabs/splits and explorer re-rooting.

### Filesystem

`FsRoot` lists and mutates only under the sandbox root and authorized directories:

- **Read:** `list`, `stat`, recursive `fs_watch` (skips noisy trees such as `.git`, `target`, `node_modules`, …).
- **Write:** `create` (empty file or directory), `copy_into`, `move_into` (conflict names get a ` copy` / ` copy N` suffix). Names must be a single path segment.

### Editor

When enabled, Fresh `Editor` runs in-process on a dedicated `!Send` thread (`EditorWorker`); see [FRESH.md](./FRESH.md). Path open supports Fresh-style `:line` / `:line:col` suffixes and Ctrl/Cmd+click link detection (`path_link`) with optional terminal cwd.

### Config

Backend `config.json` (JSONC) holds UI prefs (theme, palette, fonts, explorer visibility, minimap, editor line wrap) and default shell. Snapshot is sent in `Hello.ui`. Settings / `Mod+,` opens the file in an editor tab; saving reloads live prefs. Missing keys are filled without overriding existing values.

### Packaging

Pixi `[package]` + `recipe/` installs the headless `bin/fresh-gui` (linux-64 `.conda`, glibc 2.28+). Recipe fetches the pinned Fresh tree via `vendor/fresh.rev` when submodules are missing. It does not build or install a browser UI.

CI on `main` (and `workflow_dispatch`) bumps CalVer and publishes a GitHub Release with:

| Asset | What it is |
|-------|------------|
| `fresh-gui-*-*.conda` | linux-64 headless daemon |
| `fresh-gui-*-x86_64-unknown-linux-gnu.tar.gz` | headless daemon, glibc ≥ 2.31 (`scripts/package-binary.sh`) |
| `fresh-gui-*-x86_64-unknown-linux-musl.tar.gz` | headless daemon, musl |
| `fresh-gui-*-x86_64-pc-windows-msvc.zip` | headless daemon, Windows |
| `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` | Linux GPUI host (`fresh-gui` in new archives; `fresh-gui-app` in older ones), built on `ubuntu-latest` |
| `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` | Windows GPUI host (`fresh-gui.exe`; older archives: `fresh-gui-app.exe`) |

`pixi.toml` stays `linux-64`. Client archives are built by `scripts/package-client.sh` (no embedded Vite UI). `pixi run gui` remains the from-source host. The Linux client is built on `ubuntu-latest` and needs glibc ≥ 2.39 (not the daemon's glibc 2.31 zigbuild), a display, fontconfig, and a Vulkan loader.

`scripts/install.sh` and `scripts/install.ps1` are the one-line installers (`curl | sh`, `irm | iex`). They resolve `latest` from the GitHub Releases redirect (asset names include the CalVer), download the **client and daemon** for the host OS, verify the sibling `.sha256` when that asset exists, and install into `~/.fresh-gui/bin` (Windows: `%USERPROFILE%\.fresh-gui\bin`), then update PATH. When both archives are present the GPUI host is `fresh-gui` (a `fresh-gui-app` hardlink points at the same file) and the headless binary is `fresh-gui-daemon`. A daemon-only install keeps the headless binary named `fresh-gui`. The installers accept either archive member name (`fresh-gui` / `fresh-gui.exe` from `scripts/package-client.sh`, and `fresh-gui-app` / `fresh-gui-app.exe` from archives published before that rename). `FRESH_GUI_COMPONENTS=client|daemon` installs one of them. Linux defaults to the gnu assets; `FRESH_GUI_LIBC=musl` (also auto-detected on Alpine) selects the musl daemon. The GPUI client is published for gnu only, so a musl install skips it. If the default install asks for both and one archive is missing from that tag, the other is still installed. Windows assets use a `.zip` name; current releases are GNU tar files inside that name (Info-ZIP was not on the runner), so the installers sniff the magic and extract with `tar` or with unzip / `Expand-Archive`. See [README.md](../README.md#install).

## 7. Host surfaces

| Surface | How it connects |
|---------|-----------------|
| **Native GPUI host (only UI)** | `fresh-gui` / `pixi run gui` — gpui-kit 0.6.6 (`gpui-pre` = 0.3.6, same snapshot gpui-component requires). Speaks ADE over `fresh-gui-client`. Cargo binary name remains `fresh-gui-app`. |
| **CLI** | `fresh-gui ping\|smoke\|attach\|status\|close\|remote` via `fresh-gui-client` and the daemon binary |
| **SSH bootstrap** | `fresh-gui user@host`, `fresh-gui remote add` / `remote connect` — OpenSSH only |

### SSH remote bootstrap

The GPUI host can reach a Linux box without a hand-made tunnel. Saved targets (`user@host` or an OpenSSH `Host` alias) live in `~/.config/fresh-gui/remotes.json` (Windows: `%APPDATA%\fresh-gui\remotes.json`). That file stores destinations, not ADE tokens.

`remote connect` shells out to the system `ssh` / `scp` (keys, agent, `ssh_config`; `BatchMode=yes`, so there is no password prompt):

1. Run a probe over SSH. It checks `~/.local/bin/fresh-gui` (then `PATH`) and the daemon's private `session.json` (`$XDG_RUNTIME_DIR/fresh-gui/` or `/tmp/fresh-gui-$UID`).
2. If the binary is missing, download or unpack a Linux daemon (saved path, saved release URL, `FRESH_GUI_DAEMON_PATH` / `FRESH_GUI_DAEMON_URL`, or the latest GitHub `x86_64-unknown-linux-gnu` `.tar.gz`) and `scp` it to `~/.local/bin/fresh-gui`.
3. If no session is running, start `fresh-gui --no-ui` (optional `--root`) and read the token from `session.json`. On a server that only has the daemon archive, that binary is headless. On a machine where `fresh-gui` is the GPUI host, `--no-ui` starts `fresh-gui-daemon` and returns. The probe still prefers `~/.local/bin/fresh-gui`, which SCP installs as the headless binary.
4. Open `ssh -N -L 127.0.0.1:<local>:127.0.0.1:<remote>` and hand `ws://127.0.0.1:<local>/ws` plus the token to the GPUI window. Closing the window kills the tunnel; the remote daemon keeps running.

Fresh Orchestrator already models SSH workspaces, but that code is TUI/plugin-only and is not linked into the ADE daemon (`fresh-editor` feature `runtime`). The host does not reimplement an SSH client. Saved SSH targets stay a flat list. Workspaces on the connected daemon are a separate registry (several projects inside the one remote process); see [WORKSPACES.md](./WORKSPACES.md).

The ADE protocol did **not** need to change for the native host: only the renderer switched from browser (React/CodeMirror/xterm) to GPUI. Fresh remains on the daemon. Combined license is GPL-3.0-or-later (host, matching Fresh) plus Apache-2.0 (gpui-kit). Apache-2.0 can be combined with GPL-3.0, so the binary is GPL-3.0-or-later.

Native chrome: workspace rail, activity bar, collapsible explorer, docked terminal/editor tabs, status bar, command palette, Go to File. Ribbons stay tighter than gpui-component medium defaults (30px title, 36px activity rail, 26px explorer header, 22px status and tree rows). The dock tab strip is the skin’s default 32px bar; the new-terminal **+** lives in that group’s far-right suffix. Dragging a tab to a pane edge splits horizontally or vertically; dropping it on a tab merges; dragging in the strip reorders. gpui-component will not drag the last remaining tab. Terminal tabs are numbered `1`, `2`, `3`, … inside the focused workspace (`SessionTabTitle.workspace_id` is that workspace) and can be renamed from the tab or the **···** menu. OSC 7 still records cwd for the next PTY and does not retitle the tab. The explorer multi-selects (Ctrl/Cmd-click, Shift-click), copies absolute paths, moves a drag onto a folder (`fs_move`), and copies files for in-app paste (`fs_copy`). The left rail is a 232px spaces list (display name and shortened project root) to the left of the activity bar; switching swaps the dock for that workspace’s session. Terminal is a VTE grid of remote PTY bytes (not Fresh `TerminalManager`). Editor tabs use gpui-component `Editor` as a **view** of Fresh snapshots (save is local dirty + `buffer_edit` then `buffer_save`). Host chrome notes: [UI.md](./UI.md).

Linux GUI needs X11 or Wayland, fontconfig, FreeType, and a working wgpu/Vulkan backend. Release clients cover Linux x86_64 (`ubuntu-latest`) and Windows x86_64; `pixi.toml` stays `linux-64` for the daemon package. Linking `fresh-gui-app` (`cargo run` / `cargo test`) also needs a C++ toolchain (`g++` / `libstdc++`) because gpui-kit pulls native GPU/text stacks. The Linux release job installs those libraries (Wayland, X11/XKB, Vulkan, fontconfig, FreeType) before `cargo build`.

## 8. Fresh coupling

Fresh is a **git submodule** at `vendor/fresh`, pinned by commit SHA (also recorded in `vendor/fresh.rev` for package builds). Workspace `exclude = ["vendor/fresh"]` keeps Fresh’s own Cargo workspace separate. The daemon path-depends on `fresh-editor` with `runtime` (plugins / web / Fresh GUI off) and runs `Editor` on a dedicated `!Send` thread.

Full integration detail — vendoring, `EditorHandle`, protocol mapping, path_link, what is *not* from Fresh: **[FRESH.md](./FRESH.md)**.

**Current pin:** `14f7d28b7ab18b6cdefc75ab94c5df34044ae3d0` on the integration fork `master` ([fresh#4](https://github.com/amirhosseindavoody/fresh/pull/4) merged on top of [fresh#3](https://github.com/amirhosseindavoody/fresh/pull/3)). The delta from `ddfc322` is CI/plugin-test only (`markdown_compose.ts`, `test_focus_log.ts`); embedding still links `fresh-editor` with feature `runtime` only.

```bash
git clone --recurse-submodules https://github.com/amirhosseindavoody/fresh-gui.git
# or after clone:
git submodule update --init --recursive
```

Bump the pin with `git -C vendor/fresh fetch && git -C vendor/fresh checkout --detach <rev>`, then stage `vendor/fresh` **and** update `vendor/fresh.rev` to the same SHA.

## 9. Development environment

- **Pixi** (conda-forge): tasks `check`, `test`, `build`, `clippy`, `fmt`, `gui`, `serve`, `package`, `package-binary`, `package-client`, `update-version`.
- **Rust** via Pixi / rust-version `1.97` (edition 2024).
- **Versioning:** CalVer `YYYY.MMDD.N` (e.g. `2026.921.4`). `scripts/update-version.sh` bumps workspace manifests; CI also bumps and publishes backend Releases (linux-64 `.conda`, standalone linux-gnu / musl / windows daemon archives, and Linux + Windows GPUI client archives).

## 10. Security

Always-on bearer token (including loopback), default bind `127.0.0.1:7420`, SSH tunnel for remote access, `--allow-no-auth` loopback-only for tests. Full design: [SECURITY.md](./SECURITY.md).

## 11. License

The entire `fresh-gui` project (all crates) is **GPL-3.0-or-later**, matching Fresh. The `vendor/fresh` submodule remains under Fresh’s own GPL-3.0-or-later terms.

## 12. Architecture decisions

These are settled product choices, kept here as rationale—not a backlog.

| ID | Choice | Why |
|----|--------|-----|
| **D1** | New ADE protocol (PTY-first); Fresh `--web` scene is not the wire | Terminal-first UX without fighting an editor-centric grid scene |
| **D2** | Native GPUI host is the only UI | Desktop ADE chrome (Zed / VS Code feel) without a second editor core. A later web UI would be this GPUI client via WebAssembly |
| **D3** | Fresh as submodule + git rev pin | Portable, editable, explicit pin; also mirrored in `vendor/fresh.rev` for packaging |
| **D4** | PTY + FS + editor as layered capabilities | Useful remote shell first; explorer and Fresh editor negotiate as capabilities |
| **D5** | GPL-3.0-or-later everywhere | Same license as Fresh; no split licensing |

## 13. Repository layout

```
fresh-gui/
  Cargo.toml / Cargo.lock
  pixi.toml / pixi.lock
  README.md
  LICENSE
  docs/
    DESIGN.md          # this file
    WORKSPACES.md      # multi-workspace client ↔ daemon model
    FRESH.md           # Fresh editor embedding
    UI.md
    SECURITY.md
    COPILOT.md         # Copilot CLI / ACP agent design (issue #49)
  vendor/
    fresh/             # git submodule
    fresh.rev          # pin for package builds
  crates/
    fresh-gui-protocol/
    fresh-gui/
    fresh-gui-client/
    fresh-gui-app/     # native GPUI host + CLI + optional ui/
  recipe/              # Pixi / rattler-build package
  scripts/
    update-version.sh
    package-binary.sh  # standalone daemon archives (gnu / musl / windows)
    package-client.sh  # GPUI host archives (Linux tar.gz, Windows zip)
    install.sh         # curl | sh installer (client + daemon)
    install.ps1        # Windows irm | iex installer
```