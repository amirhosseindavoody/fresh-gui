# fresh-gui-backend

Remote (Linux MVP) daemon: WebSocket ADE protocol + sessions + PTY + FS + optional Fresh editor.

```bash
pixi run backend
pixi run backend -- --listen 127.0.0.1:7420 --token secret --root /path/to/project
# pixi run backend -- --no-editor   # omit editor capability
```

- `GET /healthz`
- `WS /ws` — JSON frames (`hello`, `auth`, `session_*`, `layout_set`, `pty_*`, `fs_*`, `editor_*`, `buffer_*`, `scene_*`, …)
- Sessions own PTYs; disconnect detaches the subscriber but keeps shells running for reattach + scrollback replay
- In-process Fresh `Editor` (capability `editor`) for open / edit / save with revision CAS
- `fs_watch` via notify; thin ADE `scene` lists open buffers (not Fresh `--web`)
- `--root` / `FRESH_GUI_FS_ROOT` — sandbox for `fs` and editor open (default: current directory)

See [docs/DESIGN.md](../../docs/DESIGN.md).
