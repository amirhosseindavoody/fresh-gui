# fresh-gui-backend

Remote (Linux MVP) daemon: WebSocket ADE protocol + PTY (`portable-pty`).

```bash
pixi run backend
pixi run backend -- --listen 127.0.0.1:7420 --token secret
```

- `GET /healthz`
- `WS /ws` — JSON frames (`hello`, `auth`, `pty_*`, …)

See [docs/DESIGN.md](../../docs/DESIGN.md).
