# fresh-gui

A **terminal-first IDE shell** for a remote Linux machine. Run the backend on your server; open shells, edit files, and browse the tree from a **native GPUI host** (Zed / VS Code feel). A browser ADE UI remains available for smoke tests.

Inspired by [Terax](https://github.com/crynta/terax-ai) (layout and terminal-first product sense — not Tauri). The remote editor core is [Fresh](https://github.com/sinelaw/fresh). The host is a renderer only: it speaks `fresh-gui-protocol` over WebSocket; Fresh stays buffer authority on the daemon.

## Install the backend (Linux)

On the machine that holds your project:

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release tag / .conda from https://github.com/amirhosseindavoody/fresh-gui/releases

cd /path/to/your/project
fresh-gui          # starts a background session, prints the URL, returns the shell
fresh-gui          # already running → reprint URL / token / log path
fresh-gui close    # stop the session
```

The process prints something like:

```text
  fresh-gui session
  pid:  12345
  UI:   http://127.0.0.1:7420/
  WS:   ws://127.0.0.1:7420/ws
  root: /path/to/your/project
  log:  ~/.local/state/fresh-gui/fresh-gui.log

  Local access (this machine):
    http://127.0.0.1:7420/?token=<token>

  From another machine (e.g. your laptop) — SSH tunnel, nothing exposed to the network:
    ssh -L 7420:127.0.0.1:7420 user@your-server
    then open: http://127.0.0.1:7420/?token=<token>

  Stop with: fresh-gui close
```

Open the **Local access** URL in the native host (token is embedded; the app auto-connects), or in a browser for the legacy Vite UI. A bearer token is **always required**, including on loopback — on shared hosts every local account can reach `127.0.0.1`. When you do not pass `--token` / `FRESH_GUI_TOKEN`, the process generates a random token and stores it in the private session meta (mode `0600`) so `fresh-gui` can reprint it later. Prefer `FRESH_GUI_TOKEN=…` over `--token` so the secret does not show up in `ps`.

Only **one background session per user** is allowed (exclusive lock under `$XDG_RUNTIME_DIR/fresh-gui/`). Closing the launching terminal does not stop the session.

While the daemon runs it samples its own resident memory (`VmRSS` / `VmHWM` via `/proc/self/status`) about every 30 seconds. On graceful shutdown (`fresh-gui close` / SIGTERM / Ctrl-C) it appends a structured log line with **average** and **peak** RSS in MB. PTY child shells are not included — only the backend process (server + embedded Fresh editor).

Works on older enterprise glibc (2.28+).

### From your laptop over SSH

Keep the backend on loopback and use the printed SSH tunnel command (or the equivalent):

```bash
# on the server
cd /path/to/your/project
fresh-gui

# on your laptop (from the banner)
ssh -L 7420:127.0.0.1:7420 user@server
# native host:
fresh-gui-app --backend 'http://127.0.0.1:7420/?token=…'
# or browser → the printed http://127.0.0.1:7420/?token=… URL
```

Do not bind publicly by default. Non-loopback listens still require a token and log a warning; SSH tunnel + loopback is the supported remote path.

### Windows or Linux client (auto-install over SSH)

The native host can save an SSH target, install the Linux daemon if it is missing, and open a local tunnel to ADE `/ws`. Auth is your normal OpenSSH setup (keys, agent, `~/.ssh/config`). The app does not prompt for a password: `ssh user@host` must already succeed non-interactively (`BatchMode`).

Download the host from a [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) (or build it with `pixi run gui`):

| Asset | Laptop |
|-------|--------|
| `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` | Windows x86_64 |

```bash
# Linux
tar -xzf fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz
cd fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu
./fresh-gui-app remote add lab user@server --root /path/to/project
./fresh-gui-app remote connect lab
```

The Linux archive is built on `ubuntu-latest` and needs **glibc ≥ 2.39** (Ubuntu 24.04 or newer; a newer runner image can raise this floor). It is not the daemon's glibc 2.31 zigbuild. It also needs a display (X11 or Wayland), fontconfig, and a Vulkan loader (`libvulkan.so.1`, loaded on demand) plus Wayland client libraries:

```bash
sudo apt install libvulkan1 libfontconfig1 libfreetype6 libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libxcb1
```

```bash
# on the laptop (Windows or Linux fresh-gui-app)
fresh-gui-app remote add lab user@server
# or an OpenSSH Host alias from ~/.ssh/config:
fresh-gui-app remote add lab my-server --root /path/to/project

# optional — pin the Linux daemon (default: latest GitHub linux-gnu release)
fresh-gui-app remote daemon --path ./fresh-gui-linux
fresh-gui-app remote daemon --url 'https://github.com/amirhosseindavoody/fresh-gui/releases/download/vYYYY.MMDD.N/fresh-gui-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz'
# one-shot overrides: FRESH_GUI_DAEMON_PATH, FRESH_GUI_DAEMON_URL

fresh-gui-app remote list
fresh-gui-app remote connect lab
```

On `remote connect` the host:

1. SSHs to the target and checks for a `fresh-gui` binary plus a live session (`session.json`).
2. If the binary is missing, copies a Linux daemon to `~/.local/bin/fresh-gui` (from the path, the URL, or the latest GitHub `x86_64-unknown-linux-gnu` release).
3. If no session is running, starts `fresh-gui --no-ui` (headless) and reads the token from the remote session file.
4. Opens `ssh -L 127.0.0.1:<local>:127.0.0.1:<remote>` and connects the GPUI window to `ws://127.0.0.1:<local>/ws`.

Closing the window closes the tunnel. The remote daemon keeps running; the next `remote connect` reuses it. Saved targets live in `~/.config/fresh-gui/remotes.json` (Windows: `%APPDATA%\fresh-gui\remotes.json`). Tokens are not stored there. `ssh` and `scp` must be on `PATH` (OpenSSH).

## Using the native host

Primary UI is **`fresh-gui-app`** (GPUI + [gpui-component](https://github.com/longbridge/gpui-component) via gpui-kit). Same machine as the daemon, or a laptop after the SSH tunnel:

```bash
# daemon already printed: http://127.0.0.1:7420/?token=…
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'
# or, with token in the environment:
FRESH_GUI_TOKEN=… pixi run gui -- --backend ws://127.0.0.1:7420/ws
```

No subcommand is the same as `gui`. Linux needs an X11 or Wayland display plus fontconfig / Vulkan (or a working wgpu backend).

After connect you get terminals, an explorer, and editor tabs in one shell:

| Do this | How |
|---------|-----|
| New terminal | `Ctrl+T` or **+** |
| Open a file | Click in the explorer (or `Ctrl+P` → path `[:line[:col]]`) |
| Save | `Ctrl+S` |
| Command palette | `Ctrl+Shift+P` |
| Toggle sidebar | `Ctrl+B` |
| Settings | Activity bar gear or `Ctrl+,` (opens `config.json`) |
| Next / prev tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Reconnect | `Ctrl+Shift+R` or command palette |

The Vite/React host (`crates/fresh-gui-app/ui`) is **not** the primary path. It still has splits, xterm WebGL, markdown WYSIWYG, context menus, and layout restore — use `pixi run ui` / `pixi run ui-serve` / the daemon’s `GET /` for those. Native v1 gaps vs that UI: pane splits, markdown WYSIWYG, minimap, context menus, layout v4 restore, WebGL xterm, find, palettes / theme packs, tab pin/reorder.

`Mod` in the browser UI is `Ctrl` on Linux/Windows and `Cmd` on macOS. Disconnect leaves remote sessions and PTYs running so you can reconnect.

Sessions keep shells alive across GUI disconnect. Explorer list/create/copy/move/delete and editor open are sandboxed to `--root` (default: current directory), plus directories authorized when a terminal cwd leaves that root.

## Settings

All prefs live in one JSONC file on the **backend** host:

`~/.config/fresh-gui/config.json`  
(or `$XDG_CONFIG_HOME/fresh-gui/config.json`, or `--config` / `FRESH_GUI_CONFIG`)

```jsonc
{
  "ui": {
    "theme": "system", // system | light | dark
    "terminalFontSize": 14,
    "editorFontSize": 14,
    "webgl": true,
    "showDotfiles": false, // show .* names in the explorer
    "showGitDirs": false, // show .git folders (separate from showDotfiles)
    "editorLineWrap": true // soft-wrap long lines; Alt+Z toggles
  },
  "terminal": {
    "shell": { "command": "zsh", "args": [] }
  }
}
```

Open it from the UI (**Settings** / `Mod+,`), edit, save with `Mod+S`. Theme follows the OS by default; terminal chrome tracks the same theme. Empty shell `args` keep interactive / OSC 7 setup for known shells. Dotfiles and `.git` directories are hidden in the explorer by default; enable them independently via `showDotfiles` / `showGitDirs`. Editor soft wrap follows Fresh `editor.line_wrap` (on by default); toggle with `Alt+Z` or the command palette.

## Develop from source

```bash
git clone https://github.com/amirhosseindavoody/fresh-gui.git
cd fresh-gui
git submodule update --init --recursive
pixi install

pixi run backend -- --foreground   # Linux daemon (prints Local access URL)
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'   # native host
```

The Vite UI is optional (`pixi run ui-install` once, then `pixi run ui` or `pixi run serve` which still embeds `ui/dist` on the daemon).

Useful tasks: `pixi run check`, `test`, `build`, `gui`, `ui` (Vite hot reload on `:1420`), `package` (write `.conda` under `./dist`), `package-binary` (standalone archive; pass target and version).

| Piece | Role |
|-------|------|
| `fresh-gui` | Daemon (PTY, FS, Fresh editor, optional embedded browser UI) — Linux primary, Windows binary also released |
| `fresh-gui-app` | Native GPUI host (default) + CLI (`ping` / `smoke` / `attach` / `serve-ui`) |
| `fresh-gui-app/ui` | Vite/React ADE shell (smoke / packaged `GET /`) |
| `fresh-gui-protocol` / `fresh-gui-client` | Shared wire format + client library |

Deeper design notes (architecture and behavior): [docs/DESIGN.md](./docs/DESIGN.md), [docs/FRESH.md](./docs/FRESH.md) (Fresh embedding), [docs/SECURITY.md](./docs/SECURITY.md), [docs/UI.md](./docs/UI.md), [docs/COPILOT.md](./docs/COPILOT.md) (Copilot CLI / ACP design). Backend flags and packaging: [crates/fresh-gui/README.md](./crates/fresh-gui/README.md).

## Releases

CalVer `YYYY.MMDD.N`. Pushes to `main` (and manual `workflow_dispatch`) bump the version and publish a [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) (see `.github/workflows/release-backend.yml`):

| Asset | Platform |
|-------|----------|
| `fresh-gui-*-*.conda` | linux-64 via Pixi (glibc 2.28+) |
| `fresh-gui-*-x86_64-unknown-linux-gnu.tar.gz` | Linux standalone (glibc ≥ 2.31) |
| `fresh-gui-*-x86_64-unknown-linux-musl.tar.gz` | Linux musl (Alpine / static-friendly) |
| `fresh-gui-*-x86_64-pc-windows-msvc.zip` | Windows standalone daemon |
| `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` | Linux GPUI host (`fresh-gui-app`) |
| `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` | Windows GPUI host (`fresh-gui-app.exe`) |

Daemon archives unpack to `bin/fresh-gui` + `share/fresh-gui/ui` (same layout as the conda package). The **client** archives are only the native GPUI host, for a Linux or Windows laptop that talks to a Linux daemon (see [Windows or Linux client](#windows-or-linux-client-auto-install-over-ssh)). The Linux client is built on `ubuntu-latest` and needs glibc ≥ 2.39, not the glibc 2.31 zigbuild used for the daemon. The version-bump commit rebases if `main` moved during the build. Manual bump: `pixi run update-version`.

## License

[GPL-3.0-or-later](./LICENSE) (same as Fresh).
