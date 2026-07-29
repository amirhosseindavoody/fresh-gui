# fresh-gui-app

Host CLI (`fresh-gui`) and the **Vite + TypeScript** UI under `ui/` (xterm.js WebGL, CodeMirror 6, unified tabs/splits, session reattach).

```bash
pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach
pixi run ui-install  # Bun deps (Bun comes from Pixi)
pixi run ui          # Vite dev server on :1420
pixi run ui-build    # emit ui/dist
pixi run ui-serve    # serve ui/dist on :1420
```

UI: leave Session empty to create; Disconnect leaves the backend session alive for reconnect. Double-click a file to edit in CodeMirror; Ctrl/Cmd+S saves via Fresh (`editor`). Tree auto-refreshes on `fs_watch`.

Tauri window: see `fresh-gui-desktop` (builds `ui/dist` via `beforeBuildCommand`).

See [docs/DESIGN.md](../../docs/DESIGN.md).
