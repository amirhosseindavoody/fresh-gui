# fresh-gui

Linux remote daemon: WebSocket ADE API + detachable sessions + PTY + filesystem + optional Fresh editor + embedded host UI.

## Run

```bash
pixi run ui-install   # once (dev)
pixi run serve        # build UI + start (http://127.0.0.1:7420/ + ws://…/ws)
pixi run serve-release  # same with Cargo --release (incremental local release testing)

pixi run backend -- --listen 127.0.0.1:7420 --root /path/to/project
pixi run backend-release -- --listen 127.0.0.1:7420   # release binary, no UI rebuild
pixi run backend -- --no-editor   # omit editor capability
pixi run backend -- --no-ui       # WebSocket + /healthz only
```

Open the printed **Local access** URL (includes `?token=`) in a browser — not the bare `ws://` line. A bearer token is always required (auto-generated when unset). Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in `ps`.

## Install

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release .conda / --tag from GitHub Releases
# or from a checkout: pixi global install --path .
fresh-gui
```

The package ships `bin/fresh-gui` and UI assets under `share/fresh-gui/ui`.

## Endpoints

| Route | Role |
|-------|------|
| `GET /` | Host UI (`share/fresh-gui/ui` or `ui/dist` in dev) |
| `GET /healthz` | Liveness |
| `WS /ws` | ADE JSON frames |

Sessions own PTYs; disconnect detaches the subscriber but keeps shells running for reattach + scrollback. Fresh `Editor` (capability `editor`) handles open / edit / save with revision CAS. `fs_watch` refreshes the tree; thin ADE `scene` lists open buffers.

## Flags

| Flag / env | Meaning |
|------------|---------|
| `--listen` / `FRESH_GUI_LISTEN` | Bind address (default `127.0.0.1:7420`; scans next ports unless `--strict-listen`) |
| `--token` / `FRESH_GUI_TOKEN` | Auth token (prefer env over flag). When unset, a random per-process token is generated and printed once |
| `--allow-no-auth` / `FRESH_GUI_ALLOW_NO_AUTH` | Disable auth (**loopback only**; for local tests — not a normal run mode) |
| `--root` / `FRESH_GUI_FS_ROOT` | FS + editor sandbox (default: cwd) |
| `--ui-dir` / `FRESH_GUI_UI_DIR` | Override UI assets directory |
| `--no-ui` / `FRESH_GUI_NO_UI` | API only |
| `--no-editor` / `FRESH_GUI_NO_EDITOR` | Omit Fresh editor |
| `--public-host` / `FRESH_GUI_PUBLIC_HOST` | Hostname in startup UI/WS URLs (else FQDN / bind address) |
| `--config` / `FRESH_GUI_CONFIG` | Path to `config.json` |

## Config

Default path: `$XDG_CONFIG_HOME/fresh-gui/config.json` or `~/.config/fresh-gui/config.json`.

```jsonc
{
  // Host UI — applied on connect and when this file is saved
  "ui": {
    "theme": "system", // system | light | dark (used when palette is primer)
    "palette": "primer", // primer | nord | dracula | … — also via activity bar / Mod+Shift+P “Color Palette”
    "terminalFontSize": 14,
    "editorFontSize": 14,
    "fontWeight": 400, // UI chrome 100–900
    "monoFontWeight": 400, // terminal + editor
    "fontFamily": "", // empty → IBM Plex Sans
    "monoFontFamily": "", // empty → IBM Plex Mono
    "webgl": true,
    "showDotfiles": false, // show .* names in the explorer
    "showGitDirs": false // show .git folders (separate from showDotfiles)
  },
  // Default PTY shell when the client omits `shell` on pty_open
  "terminal": {
    "shell": { "command": "zsh", "args": [] }
  }
}
```

Missing file → built-in defaults (`zsh`, system theme, primer palette, hidden dotfiles / `.git`). First **Settings** / `Mod+,` open creates the documented template; later opens also insert any newly added default keys that are missing from an existing file (existing values and comments are kept). Empty shell `args` keep interactive / OSC 7 setup; non-empty args are passed through. JSONC (`//` / `/* */`) is accepted. Named `palette` values match Fresh editor theme names where applicable (colors mapped onto host CSS tokens).

See [docs/DESIGN.md](../../docs/DESIGN.md) and [docs/SECURITY.md](../../docs/SECURITY.md).
