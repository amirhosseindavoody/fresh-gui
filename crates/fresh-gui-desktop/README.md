# fresh-gui-desktop

Tauri 2 host window loading the xterm.js + file-tree UI from `crates/fresh-gui-app/ui`.

The WebView connects directly to the remote `fresh-gui-backend` WebSocket (`/ws`).

## Dev

Requires [Tauri OS prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
cd crates/fresh-gui-desktop
npm ci
npm run dev
```

## Windows installers

```powershell
npm run build:windows
```

See [docs/WINDOWS.md](../../docs/WINDOWS.md).
