# fresh-gui

Terminal-first ADE **GUI on the local host**, powered by a **remote backend** (Fresh crates later; Phase 1 is a PTY daemon).

Inspired by [Terax](https://github.com/crynta/terax-ai). Repo logistics follow [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise).

**Status:** Phase 1 (remote PTY loop). See **[docs/DESIGN.md](./docs/DESIGN.md)**.

## Split

| Piece | MVP platform | Crate / binary |
|-------|--------------|----------------|
| Host GUI | Windows (Tauri 2 + xterm.js) | `fresh-gui-desktop` |
| Host UI assets / CLI | all | `fresh-gui-app` → `fresh-gui` |
| Remote backend | Linux | `fresh-gui-backend` |
| Shared protocol | all | `fresh-gui-protocol` |
| Host client lib | all | `fresh-gui-client` |

Fresh is vendored as a **git submodule** at `vendor/fresh`, pinned by commit (DESIGN §10 D3). After clone: `git submodule update --init --recursive`.

## Quick start (Phase 1)

```bash
pixi install

# terminal 1 — remote daemon (loopback; add --token for non-loopback)
pixi run backend
# pixi run backend -- --listen 0.0.0.0:7420 --token secret

# terminal 2 — CLI smoke
pixi run app -- smoke --backend ws://127.0.0.1:7420/ws

# terminal 2 — serve xterm.js UI (browser / until Tauri deps exist)
pixi run ui
# open http://127.0.0.1:1420/ → Connect to ws://127.0.0.1:7420/ws

# Tauri window (needs OS WebView / webkit prerequisites)
pixi run desktop
```

Auth: on loopback, token is optional unless `--token` / `FRESH_GUI_TOKEN` is set. Non-loopback binds **require** a token.

## Development

```bash
pixi run check
pixi run test
pixi run build
```

`fresh-gui-desktop` is excluded from default check/test (Linux CI without webkit). Build it explicitly with `pixi run desktop` / `cargo check -p fresh-gui-desktop`.

## Protocol (Phase 1)

JSON text frames over WebSocket `ws://host:port/ws`:

`hello` → optional `auth` → `pty_open` / `pty_data` (base64) / `pty_resize` / `pty_close`

## Versioning

CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`):

```bash
pixi run update-version
```

## License

[GPL-2.0](./LICENSE) (same as Fresh). See DESIGN.md §9 / D5.
