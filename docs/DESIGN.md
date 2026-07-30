# fresh-gui Design

Terminal-first IDE shell on a **local host**, connected to a **Linux remote daemon** that embeds [Fresh](https://github.com/sinelaw/fresh) for editor state. Product inspiration: [Terax](https://github.com/crynta/terax-ai). Install and usage: [README.md](../README.md). Host chrome: [UI.md](./UI.md). Fresh embedding: [FRESH.md](./FRESH.md). Access model: [SECURITY.md](./SECURITY.md).

## 1. Problem

Developers often keep a Windows or macOS laptop as the interactive machine and a Linux box (WSL, lab node, cloud VM) as the real workspace. Alternatives either stream a full remote IDE, put a thin browser UI on the same machine as the editor (`fresh --web`), or ship a native GUI in-process with the editor.

**fresh-gui** uses a different split: a **local, terminal-first ADE shell** that speaks a versioned WebSocket protocol to a **Fresh-backed daemon on the remote machine**. The remote owns files, PTYs, and editor buffers; the host owns windowing, layout, and input devices.

## 2. Goals and non-goals

### Goals

- **Split deployment:** browser host UI ↔ Linux remote backend.
- **Terminal-first UX:** multi-tab / split PTY as the primary surface, with editor and explorer as peers.
- **Fresh as backend of truth:** reuse Fresh crates for buffer/editor semantics; do not re-implement editor core in the host UI.
- **Pixi + Cargo workspace** for reproducible Rust development.
- **Documented, versioned protocol** between host and remote with capability negotiation.
- **Secure-by-default remote access:** always-on bearer token; loopback bind + SSH tunnel as the supported remote path.

### Non-goals

- Feature parity with Terax AI agents, theme marketplaces, or web preview panes.
- Replacing Fresh’s TUI or shipping as a fork of Fresh.
- Multi-user collaborative editing.
- Publishing to winget / conda-forge as a primary channel (Pixi global install and GitHub Releases cover distribution today).
- Public TLS / `wss://` exposure by default (SSH tunnel is the remote-access answer).

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
│  Browser UI                 │  tunnel │  fresh-gui                       │
│    layout, tabs, chrome     │◄───────►│    sessions, PTY, FS, config     │
│    xterm + CodeMirror       │  /ws    │    embeds Fresh Editor (optional)│
│                             │  JSON   │                                  │
│  fresh-gui-client (CLI)     │         │  vendor/fresh (submodule)        │
└─────────────────────────────┘         └──────────────────────────────────┘
                 ▲                                        ▲
                 └──────── fresh-gui-protocol ────────────┘
```

### Crates

| Crate | Binary? | Role |
|-------|---------|------|
| `fresh-gui-protocol` | no | Versioned messages, capability constants, errors |
| `fresh-gui` | yes (`fresh-gui`) | Linux daemon: HTTP UI + WebSocket ADE, sessions, PTY, FS, Fresh editor |
| `fresh-gui-client` | no | Dial, auth, typed request helpers |
| `fresh-gui-app` | yes (`fresh-gui-app`) | CLI (`ping` / `smoke` / `attach` / `serve-ui`) + Vite/TS UI (`ui/`) |

### Process model

1. Operator starts **`fresh-gui`** on the Linux machine (background session by default, or `--foreground` for tests). One daemon process holds the session lock; Fresh Editor runs in-process on a dedicated thread; PTY shells are child processes.
2. Operator opens the printed Local access URL in a browser; the UI authenticates with the embedded `?token=` (then caches it in tab `sessionStorage`).
3. After `hello` + `auth`, the client creates or attaches a **session**. Layout is persisted in `layout_set` and `localStorage`.
4. Terminal panes map to remote PTYs in that session. Explorer and editor talk to sandboxed FS / Fresh buffer APIs over the same socket.
5. Disconnect detaches the WebSocket subscriber; the session and PTYs keep running for reattach + scrollback.

## 5. Protocol

Wire format: **JSON text frames** over WebSocket at `/ws`. Protocol version is negotiated in `hello` and must match exactly (`PROTOCOL_VERSION`, currently `0.4.0`). PTY payloads use standard base64 in `pty_data`. Message shapes live in `fresh-gui-protocol` and are mirrored in the host UI (`ui/src/protocol.ts`).

This is a **new ADE protocol**, not Fresh `--web` scene. Fresh Editor is an optional capability on top of PTY / session / FS.

### Capabilities

Default backend capabilities (omit `editor` / `scene` with `--no-editor`):

| Capability | Role |
|------------|------|
| `ping` | Liveness |
| `pty` | Create / data / resize / close |
| `session` | Create / attach / list; `layout_set`; PTYs belong to a session |
| `fs` | List, authorize, stat, watch; create / copy / move under the sandbox |
| `editor` | Open / edit / save / close via embedded Fresh |
| `scene` | Thin ADE open-buffer list (`scene_get` / `scene_snapshot`) |

### Message families

- **Control** — `hello`, `auth` / `auth_ok` / `auth_error`, `ping` / `pong`, `error`.
- **Session** — `session_create` / `session_attach` / `session_list`, `layout_set`.
- **PTY** — `pty_open` (optional `cwd` / `shell`), `pty_data`, `pty_resize`, `pty_close` / `pty_closed`.
- **FS** — `fs_list` / `fs_stat` / `fs_authorize`; `fs_watch` / `fs_unwatch` / `fs_changed`; `fs_create` / `fs_copy` / `fs_move` / `fs_delete` (and matching result messages). Paths are sandboxed under `--root` / `FRESH_GUI_FS_ROOT`, plus directories authorized via `fs_authorize` (terminal cwd sync outside the primary root). Delete refuses the primary root and authorized cwd roots.
- **Editor** — `editor_open` / `editor_open_link` / `editor_opened`, `buffer_snapshot`, `buffer_edit` / `buffer_changed`, `buffer_save` / `buffer_saved`, `editor_close` (revision CAS on edit/save).
- **Scene** — `scene_get` / `scene_snapshot` (open buffers for host chrome; not Fresh cell scene).

## 6. Backend behavior

### Daemon session

Default `fresh-gui` detaches a **per-user background session** (exclusive flock under `$XDG_RUNTIME_DIR/fresh-gui/`), prints status / Local access URL, and returns the shell. Re-running reprints status; `fresh-gui close` stops the daemon. Logs go to `$XDG_STATE_HOME/fresh-gui/fresh-gui.log`. See [SECURITY.md](./SECURITY.md) for token handling in `session.json`.

While serving, the daemon samples its own resident set from `/proc/self/status` (`VmRSS` / `VmHWM`) about every 30 seconds. On SIGTERM / Ctrl-C (before graceful drain completes) it logs structured **average** and **peak** RSS in MB. Measurement covers the backend process only (Axum server, session state, embedded Fresh editor) — not PTY child shells. Fresh has no production memory monitor API; this is host-lifecycle telemetry.

### Sessions and PTYs

`SessionStore` holds multi-PTY sessions. Closing the WebSocket detaches the subscriber; PTYs keep running. Reattach replays ~64KB of scrollback per PTY and restores layout when the persisted leaf ids match live PTYs 1:1 (otherwise one terminal tab per PTY). Editor tabs are not restored across reattach (buffers are not rehydrated by path yet).

PTY shell defaults come from `config.json` (`terminal.shell`). Bash/zsh hooks emit OSC 7 so the host can track cwd for new tabs/splits and explorer re-rooting.

### Filesystem

`FsRoot` lists and mutates only under the sandbox root and authorized directories:

- **Read:** `list`, `stat`, recursive `fs_watch` (skips noisy trees such as `.git`, `target`, `node_modules`, …).
- **Write:** `create` (empty file or directory), `copy_into`, `move_into` (conflict names get a ` copy` / ` copy N` suffix). Names must be a single path segment.

### Editor

When enabled, Fresh `Editor` runs in-process on a dedicated `!Send` thread (`EditorWorker`); see [FRESH.md](./FRESH.md). Path open supports Fresh-style `:line` / `:line:col` suffixes and Ctrl/Cmd+click link detection (`path_link`) with optional terminal cwd.

### Config

Backend `config.json` (JSONC) holds UI prefs (theme, palette, fonts, explorer visibility, minimap, editor line wrap) and default shell. Snapshot is sent in `Hello.ui`. Settings / `Mod+,` opens the file in an editor tab; saving reloads live prefs. Missing keys are filled without overriding existing values.

### Packaging

Linux: Pixi `[package]` + `recipe/` installs `bin/fresh-gui` and UI under `share/fresh-gui/ui`. Recipe fetches the pinned Fresh tree via `vendor/fresh.rev` when submodules are missing. CI on `main` bumps CalVer and publishes GitHub Releases.

## 7. Host surfaces

| Surface | How it connects |
|---------|-----------------|
| **Embedded UI** | Served from the same port as `/ws` (`GET /` → `ui/dist` or packaged `share/fresh-gui/ui`) |
| **Vite dev** | `pixi run ui` on `:1420`, points at the backend WS |
| **CLI** | `fresh-gui-app ping|smoke|attach` via `fresh-gui-client` |

Host chrome is React 19 + Tailwind + shadcn (Button, Tabs, DropdownMenu, ContextMenu — same family Terax uses); ADE protocol, xterm, CodeMirror, and the virtualized tree stay imperative TypeScript modules attached once from the React shell. Full IA and shortcuts: [UI.md](./UI.md).

## 8. Fresh coupling

Fresh is a **git submodule** at `vendor/fresh`, pinned by commit SHA (also recorded in `vendor/fresh.rev` for package builds). Workspace `exclude = ["vendor/fresh"]` keeps Fresh’s own Cargo workspace separate. The daemon path-depends on `fresh-editor` with `runtime` (plugins / web / Fresh GUI off) and runs `Editor` on a dedicated `!Send` thread.

Full integration detail — vendoring, `EditorHandle`, protocol mapping, path_link, what is *not* from Fresh: **[FRESH.md](./FRESH.md)**.

**Current pin:** `f5f2c4639f7d5ed3d6b3ef3d2343365ced426401` (integration fork). Prefer `https://github.com/amirhosseindavoody/fresh.git`; upstream tracking: `https://github.com/sinelaw/fresh.git`.

```bash
git clone --recurse-submodules https://github.com/amirhosseindavoody/fresh-gui.git
# or after clone:
git submodule update --init --recursive
```

Bump the pin with `git -C vendor/fresh fetch && git -C vendor/fresh checkout --detach <rev>`, then stage `vendor/fresh` **and** update `vendor/fresh.rev` to the same SHA.

## 9. Development environment

- **Pixi** (conda-forge): tasks `check`, `test`, `build`, `clippy`, `fmt`, `ui` / `ui-install` / `ui-build`, `serve`, `package`, `update-version`.
- **Rust** via Pixi / rust-version `1.97` (edition 2024).
- **Bun** for the Vite/TS UI (`crates/fresh-gui-app/ui/bun.lock`).
- **Versioning:** CalVer `YYYY.MMDD.N` (e.g. `2026.730.2`). `scripts/update-version.sh` bumps workspace manifests; CI also bumps and publishes backend Releases.

## 10. Security

Always-on bearer token (including loopback), default bind `127.0.0.1:7420`, SSH tunnel for remote access, `--allow-no-auth` loopback-only for tests. Full design: [SECURITY.md](./SECURITY.md).

## 11. License

The entire `fresh-gui` project (all crates) is **GPL-2.0**, matching Fresh. The `vendor/fresh` submodule remains under Fresh’s own GPL-2.0 terms.

## 12. Architecture decisions

These are settled product choices, kept here as rationale—not a backlog.

| ID | Choice | Why |
|----|--------|-----|
| **D1** | New ADE protocol (PTY-first); Fresh `--web` scene is not the wire | Terminal-first UX without fighting an editor-centric grid scene |
| **D2** | Browser UI (React + xterm.js WebGL + CodeMirror) | Fast UI iteration and a mature terminal emulator; Rust owns the daemon and client library |
| **D3** | Fresh as submodule + git rev pin | Portable, editable, explicit pin; also mirrored in `vendor/fresh.rev` for packaging |
| **D4** | PTY + FS + editor as layered capabilities | Useful remote shell first; explorer and Fresh editor negotiate as capabilities |
| **D5** | GPL-2.0 everywhere | Same license as Fresh; no split licensing |

## 13. Repository layout

```
fresh-gui/
  Cargo.toml / Cargo.lock
  pixi.toml / pixi.lock
  README.md
  LICENSE
  docs/
    DESIGN.md          # this file
    FRESH.md           # Fresh editor embedding
    UI.md
    SECURITY.md
  vendor/
    fresh/             # git submodule
    fresh.rev          # pin for package builds
  crates/
    fresh-gui-protocol/
    fresh-gui/
    fresh-gui-client/
    fresh-gui-app/     # CLI + ui/
  recipe/              # Pixi / rattler-build package
  scripts/
    update-version.sh
```
