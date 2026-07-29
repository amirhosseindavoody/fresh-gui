# fresh-gui

Terminal-first ADE **GUI on the local host**, powered by a **remote backend** (Fresh crates later).

Inspired by [Terax](https://github.com/crynta/terax-ai). Repo logistics follow [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise).

**Status:** Phase 3a (Fresh editor open/snapshot) on Phase 2 sessions/tabs; Windows Tauri NSIS/MSI packaging wired. See **[docs/DESIGN.md](./docs/DESIGN.md)** and **[docs/WINDOWS.md](./docs/WINDOWS.md)**.

## Split

| Piece | MVP platform | Crate / binary |
|-------|--------------|----------------|
| Host GUI | Windows (Tauri 2 + xterm.js) | `fresh-gui-desktop` |
| Host UI assets / CLI | all | `fresh-gui-app` → `fresh-gui` |
| Remote backend | Linux | `fresh-gui-backend` |
| Shared protocol | all | `fresh-gui-protocol` |
| Host client lib | all | `fresh-gui-client` |

Fresh is vendored as a **git submodule** at `vendor/fresh`, pinned by commit (DESIGN §10 D3). After clone: `git submodule update --init --recursive`.

## Quick start

```bash
pixi install

# terminal 1 — remote daemon (loopback; add --token for non-loopback)
pixi run backend
# pixi run backend -- --listen 0.0.0.0:7420 --token secret --root /path/to/project

# terminal 2 — CLI smoke
pixi run app -- smoke --backend ws://127.0.0.1:7420/ws

# terminal 2 — serve UI (PTY + file tree + tabs/splits + editor)
pixi run ui
# open http://127.0.0.1:1420/ → Connect to ws://127.0.0.1:7420/ws
# leave Session empty to create; double-click a file to open in the editor pane
```

Auth: on loopback, token is optional unless `--token` / `FRESH_GUI_TOKEN` is set. Non-loopback binds **require** a token. FS listing and editor open are sandboxed to `--root` (default: cwd). Sessions keep PTYs alive across GUI disconnect. Pass `--no-editor` to run without Fresh.

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

`hello` → optional `auth` → `session_*` / `layout_set` → `pty_*` / `fs_*` / `editor_open` (protocol `0.3.0`)

## Versioning

CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`):

```bash
pixi run update-version
```

## License

[GPL-2.0](./LICENSE) (same as Fresh). See DESIGN.md §9 / D5.
