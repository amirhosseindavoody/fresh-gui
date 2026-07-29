# fresh-gui-app

Host CLI (`fresh-gui`) and the Vite + TypeScript UI under `ui/`.

```bash
pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach
pixi run ui-install  # once
pixi run ui          # Vite on :1420
pixi run ui-build
pixi run ui-serve    # serve ui/dist on :1420
```

**UI basics:** leave Session empty to create; Disconnect keeps the backend session. Open files from the tree (editor tabs). `Mod+S` saves. Settings / `Mod+,` opens `config.json` on the backend. Right-click tabs or tree rows to copy paths. In the terminal, mouse-drag to select, then `Mod+C` to copy (`Mod+V` pastes). Theme follows the OS by default.

Tauri window: `fresh-gui-desktop` (builds `ui/dist` via `beforeBuildCommand`).

Product overview: [README.md](../../README.md). Design: [docs/DESIGN.md](../../docs/DESIGN.md), [docs/UI.md](../../docs/UI.md).
