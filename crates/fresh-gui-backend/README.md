# fresh-gui-backend

Remote (Linux MVP) daemon: WebSocket ADE protocol + PTY + read-only FS listing.

```bash
pixi run backend
pixi run backend -- --listen 127.0.0.1:7420 --token secret --root /path/to/project
```

- `GET /healthz`
- `WS /ws` — JSON frames (`hello`, `auth`, `pty_*`, `fs_list`, `fs_stat`, …)
- `--root` / `FRESH_GUI_FS_ROOT` — sandbox for `fs` (default: current directory)

See [docs/DESIGN.md](../../docs/DESIGN.md).
