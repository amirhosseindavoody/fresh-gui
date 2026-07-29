# fresh-gui-desktop

Tauri 2 host window loading the xterm.js + file-tree UI from `crates/fresh-gui-app/ui`.

The WebView connects directly to the remote `fresh-gui` WebSocket (`/ws`).

## Dev

Requires [Tauri OS prerequisites](https://v2.tauri.app/start/prerequisites/).

```bash
cd crates/fresh-gui-desktop
npm ci
npm run dev
```

## Windows installers (NSIS + MSI)

```powershell
npm run build:windows
```

Bundle version is mapped from workspace CalVer for WiX (see [docs/WINDOWS.md](../../docs/WINDOWS.md)).
