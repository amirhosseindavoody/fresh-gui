# fresh-gui

Remote ADE daemon: WebSocket ADE API + detachable sessions + PTY + filesystem + optional Fresh editor. The desktop command is the GPUI host, installed as `fresh-gui`; this package is the headless process (`fresh-gui` in Cargo, Pixi, and the daemon archive, `fresh-gui-daemon` when the installer places it beside the host). Runs on **Linux** (documented remote) and **Windows** (standalone release binary). The daemon does not serve a browser UI.

## Run

```bash
cd /path/to/your/project
fresh-gui             # start background session, print URL, return to shell
fresh-gui             # already running → print status (URL, token, log, pid)
fresh-gui status      # same status view
fresh-gui close       # stop the background session

# Foreground (tests / debugging — does not use the per-user session lock):
fresh-gui --foreground --listen 127.0.0.1:7420 --root /path/to/project
```

`fresh-gui` keeps **one background session per user**. Starting it again while a session is live prints the access URL / token and log path instead of starting a second process. Closing the shell that launched it does **not** stop the session — use `fresh-gui close`. That one process holds every workspace (each with its own PTYs and tabs). Switching workspaces in the GPUI client does not start another daemon. The workspace list is saved to disk and reloaded on the next start; running shells are not.

Session files (private to the user):

| Path | Role |
|------|------|
| `$XDG_RUNTIME_DIR/fresh-gui/session.lock` (Linux) / `%LOCALAPPDATA%\fresh-gui\session.lock` (Windows) | Exclusive lock (one session) |
| `…/session.json` | pid, URLs, token, root, log path |
| `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` (Linux) / `%LOCALAPPDATA%\fresh-gui\fresh-gui.log` (Windows) | Daemon stdout/stderr + tracing |
| `$XDG_STATE_HOME/fresh-gui/workspaces.json` (Linux) / `%LOCALAPPDATA%\fresh-gui\workspaces.json` (Windows) | Saved workspaces (names, roots, tabs, open explorer folders). Kept across `fresh-gui close` and reboots; delete it to start with none. `FRESH_GUI_WORKSPACES_FILE` overrides the path, and enables saving under `--foreground` |

(Linux fallbacks: `/tmp/fresh-gui-$UID/` and `~/.local/state/fresh-gui/`.)

The daemon samples its own RSS about every 30 seconds and, on graceful stop, logs average and peak resident memory (MB). Child PTY processes are excluded.

On a desktop install, `fresh-gui` in the project directory opens the window against this session (it reads the token from `session.json` via `--json`). From a checkout: `pixi run gui`. A bearer token is always required (auto-generated when unset). Prefer `FRESH_GUI_TOKEN` over `--token` so the secret does not appear in `ps`. The daemon does not serve a browser UI. `--json` (hidden) prints session meta as one JSON object for that launcher.

## Install

```bash
# client + daemon into ~/.fresh-gui/bin (see the repo README)
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | sh
# daemon only:
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | FRESH_GUI_COMPONENTS=daemon sh

pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release .conda / --tag from GitHub Releases
# or from a checkout: pixi global install --path .
# or unpack a release binary archive (linux-gnu / musl / windows) and run ./bin/fresh-gui
fresh-gui
```

The package / archive ships the headless `bin/fresh-gui` binary. The native GPUI host is a separate release asset (`fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` or the Windows `.zip`).

## Endpoints

| Route | Role |
|-------|------|
| `GET /` | Not served (headless daemon) |
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
| `--no-ui` / `FRESH_GUI_NO_UI` | Accepted for compatibility; the daemon is always headless. The desktop `fresh-gui` command uses the same flag to start this process and skip the window |
| `--json` | Hidden. Print session meta as JSON on stdout (no banner). `status --json` exits 1 when nothing is running |
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

Missing file → built-in defaults (`zsh` on Unix, `powershell` on Windows, system theme, primer palette, hidden dotfiles / `.git`, line wrap on). On Unix, if the chosen shell command is missing or not executable, a new terminal falls back to `$SHELL` (when usable), then `bash`, then `sh`. The daemon logs the shell it selected. If every candidate fails, the `pty_open_failed` message names the attempts and tells you to set `terminal.shell.command`. First **Settings** / `Mod+,` open creates the documented template; later opens also insert any newly added default keys that are missing from an existing file (existing values and comments are kept). Empty shell `args` keep interactive / OSC 7 setup; non-empty args are passed through. JSONC (`//` / `/* */`) is accepted. Named `palette` values match Fresh editor theme names where applicable (colors mapped onto host CSS tokens).

See [docs/DESIGN.md](../../docs/DESIGN.md) and [docs/FRESH.md](../../docs/FRESH.md) (Fresh embedding). Security: [docs/SECURITY.md](../../docs/SECURITY.md).
