# fresh-gui-desktop

Tauri 2 host window loading the xterm.js UI from `crates/fresh-gui-app/ui`.

The WebView connects directly to the remote `fresh-gui-backend` WebSocket (`/ws`).

## Prerequisites

Install [Tauri 2 OS prerequisites](https://v2.tauri.app/start/prerequisites/) (Windows MVP; on Linux: webkit2gtk, etc.).

```bash
pixi run desktop
# or:
cargo run -p fresh-gui-desktop
```

`pixi run check` does not build this crate by default (see workspace `default-members`) so Linux CI without webkit still works.
