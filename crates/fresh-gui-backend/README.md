# fresh-gui-backend

Remote (Linux MVP) daemon: WebSocket ADE protocol + sessions + PTY + read-only FS listing.

```bash
pixi run backend
pixi run backend -- --listen 127.0.0.1:7420 --token secret --root /path/to/project
```

- `GET /healthz`
- `WS /ws` — JSON frames (`hello`, `auth`, `session_*`, `layout_set`, `pty_*`, `fs_list`, `fs_stat`, …)
- Sessions own PTYs; disconnect detaches the subscriber but keeps shells running for reattach + scrollback replay
- `--root` / `FRESH_GUI_FS_ROOT` — sandbox for `fs` (default: current directory)

See [docs/DESIGN.md](../../docs/DESIGN.md).
