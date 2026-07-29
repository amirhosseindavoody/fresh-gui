# fresh-gui-app

Host CLI (`fresh-gui`) and the **xterm.js** UI assets under `ui/` (tabs, splits, session reattach, editor pane).

```bash
pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach
pixi run ui          # serve ui/ on :1420
```

UI: leave Session empty to create; Disconnect leaves the backend session alive for reconnect. Double-click a file to edit in CodeMirror; Ctrl/Cmd+S saves via Fresh (`editor`). Tree auto-refreshes on `fs_watch`.

Tauri window: see `fresh-gui-desktop`.

See [docs/DESIGN.md](../../docs/DESIGN.md).
