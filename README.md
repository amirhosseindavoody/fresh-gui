# fresh-gui

Terminal-first ADE **GUI on the local host**, powered by a **remote backend** (Fresh crates later).

Inspired by [Terax](https://github.com/crynta/terax-ai). Repo logistics follow [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise).

**Status:** Phase 1 + 1b (PTY + read-only file tree); Windows Tauri NSIS packaging wired. See **[docs/DESIGN.md](./docs/DESIGN.md)** and **[docs/WINDOWS.md](./docs/WINDOWS.md)**.

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

# terminal 2 — serve UI (PTY + file tree)
pixi run ui
# open http://127.0.0.1:1420/ → Connect to ws://127.0.0.1:7420/ws
```

Auth: on loopback, token is optional unless `--token` / `FRESH_GUI_TOKEN` is set. Non-loopback binds **require** a token. FS listing is sandboxed to `--root` (default: cwd).

## Windows installer (NSIS)

On a Windows machine (or via CI):

```powershell
cd crates\fresh-gui-desktop
npm ci
npm run build:windows
```

Produces `target/release/bundle/nsis/*-setup.exe`. MSI is not used (CalVer vs WiX version limits — [docs/WINDOWS.md](./docs/WINDOWS.md)).

## Development

```bash
pixi run check
pixi run test
pixi run build
```

`fresh-gui-desktop` is excluded from default check/test on Linux (no webkit).

## Protocol

JSON text frames over WebSocket `ws://host:port/ws`:

`hello` → optional `auth` → `pty_*` and/or `fs_list` / `fs_stat`

## Versioning

CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`):

```bash
pixi run update-version
```

## License

[GPL-2.0](./LICENSE) (same as Fresh). See DESIGN.md §9 / D5.
