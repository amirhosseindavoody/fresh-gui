# fresh-gui

Remote ADE daemon: WebSocket ADE API + detachable sessions + PTY + filesystem + optional Fresh editor + embedded host UI. Runs on **Linux** and **Windows**.

## Run

```bash
pixi run ui-install   # once (dev)
cd /path/to/your/project
fresh-gui             # start background session, print URL, return to shell
fresh-gui             # already running → print status (URL, token, log, pid)
fresh-gui status      # same status view
fresh-gui close       # stop the background session

# Foreground (tests / debugging — does not use the per-user session lock):
fresh-gui --foreground --listen 127.0.0.1:7420 --root /path/to/project
```

`fresh-gui` keeps **one background session per user**. Starting it again while a session is live prints the access URL / token and log path instead of starting a second process. Closing the shell that launched it does **not** stop the session — use `fresh-gui close`.

Session files (private to the user):

| Path | Role |
|------|------|
| `$XDG_RUNTIME_DIR/fresh-gui/session.lock` (Linux) / `%LOCALAPPDATA%\fresh-gui\session.lock` (Windows) | Exclusive lock (one session) |
| `…/session.json` | pid, URLs, token, root, log path |
| `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` (Linux) / `%LOCALAPPDATA%\fresh-gui\fresh-gui.log` (Windows) | Daemon stdout/stderr + tracing |

(Linux fallbacks: `/tmp/fresh-gui-$UID/` and `~/.local/state/fresh-gui/`.)

The daemon samples its own RSS about every 30 seconds and, on graceful stop, logs average and peak resident memory (MB). Child PTY processes are excluded.

Open the printed **Local access** URL (includes `?token=`) in a browser. A bearer token is always required (auto-generated when unset). Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in `ps`. After connect, the UI caches the token in tab `sessionStorage` so a reload can re-auth and reattach without keeping `?token=` in the URL.

## Install

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release .conda / --tag from GitHub Releases
# or from a checkout: pixi global install --path .
# or unpack a release binary archive (linux-gnu / musl / windows) and run ./bin/fresh-gui
fresh-gui
```

The package / archive ships `bin/fresh-gui` and UI assets under `share/fresh-gui/ui`.

## Endpoints

| Route | Role |
|-------|------|
| `GET /` | Host UI (`share/fresh-gui/ui` or `ui/dist` in dev) |
| `GET /healthz` | Liveness |
| `WS /ws` | ADE JSON frames |

Sessions own PTYs; disconnect detaches the subscriber but keeps shells running for reattach + scrollback. Fresh `Editor` (capability `editor`) handles open / edit / save with revision CAS. Sandboxed FS supports list / create / copy / move plus `fs_watch` for tree refresh; thin ADE `scene` lists open buffers.

## Flags

| Flag / env | Meaning |
|------------|---------|
| `close` / `status` | Subcommands to stop or inspect the background session |
| `--foreground` / `FRESH_GUI_FOREGROUND` | Do not detach (integration tests / debugging) |
| `--listen` / `FRESH_GUI_LISTEN` | Bind address (default `127.0.0.1:7420`; scans next ports unless `--strict-listen`) |
| `--token` / `FRESH_GUI_TOKEN` | Auth token (prefer env over flag). When unset, a random per-process token is generated |
| `--allow-no-auth` / `FRESH_GUI_ALLOW_NO_AUTH` | Disable auth (**loopback only**; for local tests — not a normal run mode) |
| `--root` / `FRESH_GUI_FS_ROOT` | FS + editor sandbox (default: cwd) |
| `--ui-dir` / `FRESH_GUI_UI_DIR` | Override UI assets directory |
| `--no-ui` / `FRESH_GUI_NO_UI` | API only |
| `--no-editor` / `FRESH_GUI_NO_EDITOR` | Omit Fresh editor |
| `--public-host` / `FRESH_GUI_PUBLIC_HOST` | Hostname in startup UI/WS URLs (else FQDN / bind address) |
| `--config` / `FRESH_GUI_CONFIG` | Path to `config.json` |

## Config

Default path: `$XDG_CONFIG_HOME/fresh-gui/config.json` or `~/.config/fresh-gui/config.json` (Windows: `%APPDATA%\fresh-gui\config.json`).

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
    "showGitDirs": false, // show .git folders (separate from showDotfiles)
    "editorMinimap": false, // VS Code–style document map; off = not loaded
    "editorLineWrap": true // soft-wrap long lines (Fresh editor.line_wrap)
  },
  // Default PTY shell when the client omits `shell` on pty_open
  "terminal": {
    "shell": { "command": "zsh", "args": [] }
  }
}
```

Missing file → built-in defaults (`zsh`, system theme, primer palette, hidden dotfiles / `.git`, line wrap on). First **Settings** / `Mod+,` open creates the documented template; later opens also insert any newly added default keys that are missing from an existing file (existing values and comments are kept). Empty shell `args` keep interactive / OSC 7 setup; non-empty args are passed through. JSONC (`//` / `/* */`) is accepted. Named `palette` values match Fresh editor theme names where applicable (colors mapped onto host CSS tokens).

See [docs/DESIGN.md](../../docs/DESIGN.md) and [docs/FRESH.md](../../docs/FRESH.md) (Fresh embedding). Security: [docs/SECURITY.md](../../docs/SECURITY.md).
