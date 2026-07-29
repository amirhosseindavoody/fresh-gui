# fresh-gui

Terminal-first ADE **GUI on the local host**, powered by a **remote backend** (Fresh crates later).

Inspired by [Terax](https://github.com/crynta/terax-ai). Repo logistics follow [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise).

**Status:** Phase 3 (Fresh editor edit/save, fs_watch, thin scene) + **UI-1–UI-3** host chrome (pane trees, shortcuts, palette, virtualized tree, OSC 7 cwd, find, activity bar, light theme/settings); Windows Tauri NSIS/MSI packaging wired. Host UI: **[docs/UI.md](./docs/UI.md)**. See also **[docs/DESIGN.md](./docs/DESIGN.md)** and **[docs/WINDOWS.md](./docs/WINDOWS.md)**.

## Split

| Piece | MVP platform | Crate / binary |
|-------|--------------|----------------|
| Host GUI | Windows (Tauri 2 + Vite/TS UI) | `fresh-gui-desktop` |
| Host UI assets / CLI | all | `fresh-gui-app` → `fresh-gui` |
| Remote backend | Linux | `fresh-gui-backend` |
| Shared protocol | all | `fresh-gui-protocol` |
| Host client lib | all | `fresh-gui-client` |

Fresh is vendored as a **git submodule** at `vendor/fresh`, pinned by commit (DESIGN §10 D3). After clone: `git submodule update --init --recursive`.

## Quick start

```bash
pixi install
pixi run ui-install   # once (Bun via Pixi)

# one process: build UI + backend (HTTP UI + WebSocket; prefers :7420, else next free port)
pixi run serve
# terminal prints: UI: http://127.0.0.1:PORT/  and  WS: ws://127.0.0.1:PORT/ws
# open that UI URL → Connect (WS defaults to same host /ws)

# optional CLI smoke against that backend
pixi run app -- smoke --backend ws://127.0.0.1:7420/ws

# UI hot-reload during development (two processes):
#   pixi run backend
#   pixi run ui          # Vite on :1420 → Connect to ws://127.0.0.1:7420/ws
```

Auth: on loopback, token is optional unless `--token` / `FRESH_GUI_TOKEN` is set. Non-loopback binds **require** a token. FS listing and editor open are sandboxed to `--root` (default: cwd). Sessions keep PTYs alive across GUI disconnect. Pass `--no-editor` to run without Fresh. Pass `--no-ui` for API-only. Startup UI/WS URLs prefer an assigned host domain (`--public-host` / `FRESH_GUI_PUBLIC_HOST`, or auto-detected FQDN) over a bare loopback address.

### Installable Linux binary (Pixi package)

Same role as `pixi run serve`. **Install on a linux-64 machine** (the remote daemon). Windows/macOS hosts are not supported for this package — use the Windows GUI client later, or a browser against this server.

```bash
# Private repo: ensure git can clone GitHub first, e.g.:
#   gh auth login && gh auth setup-git
# or use SSH:
#   pixi global install --git git@github.com:amirhosseindavoody/fresh-gui.git

pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# exposes `fresh-gui-backend` (UI under share/fresh-gui/ui)

fresh-gui-backend
# → open the UI URL printed in the terminal (not the ws:// line)
```

Works on older enterprise glibc (2.28+) — the package build uses Rollup’s WASM backend so it does not need host `GLIBC_2.32`.

From a local checkout (optional): `pixi global install --path .` or `pixi run package` (writes `./dist`).

### Remote Linux + browser (SSH)

```bash
# on the server
pixi run serve -- --listen 127.0.0.1:7420 --token secret --root "$PWD"
# or, after packaging: fresh-gui-backend --listen 127.0.0.1:7420 --token secret --root "$PWD"

# on your laptop
ssh -L 7420:127.0.0.1:7420 user@server
# browser → http://127.0.0.1:7420/
```

## Windows installers (NSIS + MSI)

On a Windows machine (or via CI):

```powershell
cd crates\fresh-gui-desktop
npm ci
npm run build:windows
```

Produces NSIS + MSI under `target/release/bundle/`. Installer version is a WiX-safe mapping of CalVer (e.g. `2026.728.1` → `26.7.28001` — [docs/WINDOWS.md](./docs/WINDOWS.md)).

## Development

```bash
pixi run check
pixi run test
pixi run build
```

`fresh-gui-desktop` is excluded from default check/test on Linux (no webkit).

## Protocol

JSON text frames over WebSocket `ws://host:port/ws`:

`hello` → optional `auth` → `session_*` / `layout_set` → `pty_*` / `fs_*` / `editor_*` / `buffer_*` / `scene_*` (protocol `0.4.0`)

## Versioning

CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`):

```bash
pixi run update-version
```

## License

[GPL-2.0](./LICENSE) (same as Fresh). See DESIGN.md §9 / D5.
