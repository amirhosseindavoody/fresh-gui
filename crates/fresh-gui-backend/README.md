# fresh-gui-backend

Remote (Linux MVP) daemon: WebSocket ADE protocol + sessions + PTY + FS + optional Fresh editor + optional embedded web UI.

```bash
pixi run ui-install   # once
pixi run serve        # ui-build + backend (http://127.0.0.1:7420/ + ws://…/ws)

pixi run backend      # same binary; rebuild UI separately with `pixi run ui-build` if needed
pixi run backend -- --listen 127.0.0.1:7420 --token secret --root /path/to/project
pixi run backend -- --no-editor   # omit editor capability
pixi run backend -- --no-ui       # WebSocket + /healthz only
```

### Pixi package (installable binary)

Builds the same binary + ships UI under `$PREFIX/share/fresh-gui/ui` (recipe in `recipe/`):

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or from a checkout: pixi global install --path .
# or: pixi run package   # writes .conda under ./dist
fresh-gui-backend
```

- `GET /` — built host UI from `share/fresh-gui/ui` (package) or `crates/fresh-gui-app/ui/dist` (dev)
- `GET /healthz`
- `WS /ws` — JSON frames (`hello`, `auth`, `session_*`, `layout_set`, `pty_*`, `fs_*`, `editor_*`, `buffer_*`, `scene_*`, …)
- Sessions own PTYs; disconnect detaches the subscriber but keeps shells running for reattach + scrollback replay
- In-process Fresh `Editor` (capability `editor`) for open / edit / save with revision CAS
- `fs_watch` via notify; thin ADE `scene` lists open buffers (not Fresh `--web`)
- `--root` / `FRESH_GUI_FS_ROOT` — sandbox for `fs` and editor open (default: current directory)
- `--ui-dir` / `FRESH_GUI_UI_DIR` — override UI assets directory
- `--no-ui` / `FRESH_GUI_NO_UI` — disable static UI
- `--public-host` / `FRESH_GUI_PUBLIC_HOST` — hostname in startup UI/WS URLs; when unset, uses an assigned FQDN (`hostname -f` / `HOSTNAME` / `FRESH_GUI_DOMAIN`) if one looks like a real domain
- `--config` / `FRESH_GUI_CONFIG` — JSON config path (default: `$XDG_CONFIG_HOME/fresh-gui/config.json` or `~/.config/fresh-gui/config.json`)

### Config (`config.json`)

Same nesting as Fresh’s terminal shell setting. When the client omits `shell` on `pty_open`, the backend uses:

```json
{
  "terminal": {
    "shell": { "command": "zsh", "args": [] }
  }
}
```

`zsh` is the built-in default when the file is missing. Empty `args` keep the backend’s interactive / OSC 7 setup; non-empty `args` are passed through as-is. JSONC (`//` / `/* */`) is accepted.

See [docs/DESIGN.md](../../docs/DESIGN.md).
