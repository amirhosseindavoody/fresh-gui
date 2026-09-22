# fresh-gui

A **terminal-first IDE shell**. Run **`fresh-gui`**. It opens the native GPUI host (Zed / VS Code feel) and, on this machine, starts the headless daemon when this user does not already have one. A path opens that project. `fresh-gui user@host` installs or reuses the daemon on a Linux machine over SSH and opens the same window.

Inspired by [Terax](https://github.com/crynta/terax-ai) (layout and terminal-first product sense — not Tauri). The remote editor core is [Fresh](https://github.com/sinelaw/fresh). The host is a renderer only: it speaks `fresh-gui-protocol` over WebSocket; Fresh stays buffer authority on the daemon.

## Install

Linux (x86_64):

```bash
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | sh
```

Windows (PowerShell 5.1+, x86_64):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | iex"
```

Review the Windows script before running it:

```powershell
powershell -c "irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | more"
```

The script reads the latest [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) for this machine, checks the `.sha256` file published next to each archive, and copies binaries into `~/.fresh-gui/bin` (Windows: `%USERPROFILE%\.fresh-gui\bin`). It prepends that directory to `PATH` (shell rc / profile on Unix, the user `PATH` on Windows). Open a new terminal afterward. Published hosts are Linux x86_64 and Windows x86_64. macOS has no release build.

| On `PATH` | When |
|-----------|------|
| `fresh-gui` | Always. With the client installed this is the GPUI host: it starts or reuses the local daemon and opens the window. |
| `fresh-gui-daemon` | Client and daemon both installed. Headless ADE process the desktop command launches. |
| `fresh-gui-app` | Same program as `fresh-gui` when the client is installed (older name). |

A daemon-only install (`FRESH_GUI_COMPONENTS=daemon`, or musl) names the headless binary `fresh-gui` and does not install a window. That is the server / `remote connect` binary (`~/.local/bin/fresh-gui` after SSH bootstrap). Remote SCP still looks for `bin/fresh-gui` inside the daemon archive.

Windows assets are named `.zip`. The ones on current releases are GNU tar archives, and the installer extracts those with `tar` (included with Windows 10 and later). A real PK zip is unpacked with `Expand-Archive` / `unzip`.

The default is **both** archives when the release contains them. Linux uses the gnu builds (`x86_64-unknown-linux-gnu`). Alpine (or `FRESH_GUI_LIBC=musl`) installs the musl daemon as `fresh-gui`; the GPUI client is gnu-only, so that install skips the desktop command unless you set `FRESH_GUI_LIBC=gnu`.

| Setting | Shell | PowerShell |
|---------|-------|------------|
| Version (`latest` or `2026.921.5` / `v2026.921.5`) | `FRESH_GUI_VERSION` | `-FreshGuiVersion` or `$env:FRESH_GUI_VERSION` |
| Install prefix | `FRESH_GUI_HOME` (default `~/.fresh-gui`) | `-FreshGuiHome` or `$env:FRESH_GUI_HOME` |
| Bin directory | `FRESH_GUI_BIN_DIR` | `$env:FRESH_GUI_BIN_DIR` |
| GitHub repo | `FRESH_GUI_REPOURL` | `-FreshGuiRepourl` or `$env:FRESH_GUI_REPOURL` |
| Skip PATH edit | `FRESH_GUI_NO_PATH_UPDATE=1` | `-NoPathUpdate` or `$env:FRESH_GUI_NO_PATH_UPDATE=1` |
| What to install (`both`, `client`, `daemon`) | `FRESH_GUI_COMPONENTS` | `-FreshGuiComponents` or `$env:FRESH_GUI_COMPONENTS` |
| Linux libc (`gnu`, `musl`) | `FRESH_GUI_LIBC` | — |
| Print the plan and exit | `FRESH_GUI_DRY_RUN=1` | `-DryRun` or `$env:FRESH_GUI_DRY_RUN=1` |

`irm | iex` picks up the environment variables. Named parameters apply when you run the file (`powershell -File .\scripts\install.ps1 -FreshGuiVersion 2026.921.5 -NoPathUpdate`).

```bash
# pinned release, client only, leave PATH alone
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | \
  FRESH_GUI_VERSION=2026.921.5 FRESH_GUI_COMPONENTS=client FRESH_GUI_NO_PATH_UPDATE=1 sh
```

```powershell
$env:FRESH_GUI_VERSION = '2026.921.5'
$env:FRESH_GUI_COMPONENTS = 'client'
irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | iex
```

The Linux client needs glibc ≥ 2.39, an X11 or Wayland session, fontconfig, and a Vulkan loader (`libvulkan.so.1`). The daemon gnu build needs glibc ≥ 2.31. Pixi remains available for the daemon alone (below).

## Open the app

After the installer (client + daemon):

```bash
fresh-gui                      # this directory; starts the daemon if needed, then the window
fresh-gui /path/to/project     # that project (creates or focuses a workspace if a session exists)
fresh-gui status               # URL, token, log, pid
fresh-gui close                # stop the background session
```

If a session is already running, `fresh-gui` attaches the window to it. It does not start a second daemon. The token comes from the private session file. You do not paste a URL.

Pass an explicit backend when you already have a tunnel or a printed Local access URL:

```bash
fresh-gui --backend 'http://127.0.0.1:7420/?token=<token>'
```

## Headless daemon (servers, Pixi)

Pixi and `FRESH_GUI_COMPONENTS=daemon` install only the headless binary, still named `fresh-gui`. On that machine the command starts a background session and returns the shell. It does not open a window.

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release tag / .conda from https://github.com/amirhosseindavoody/fresh-gui/releases

cd /path/to/your/project
fresh-gui          # background session, prints the URL, returns the shell
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
    fresh-gui --backend 'http://127.0.0.1:7420/?token=<token>'

  Stop with: fresh-gui close
```

On a machine where `fresh-gui` is the desktop command, running it again attaches to this session (the token is read from the session file, not pasted). A bearer token is **always required**, including on loopback — on shared hosts every local account can reach `127.0.0.1`. When you do not pass `--token` / `FRESH_GUI_TOKEN`, the process generates a random token and stores it in the private session meta (mode `0600`) so a later `fresh-gui` / `fresh-gui status` can use it. Prefer `FRESH_GUI_TOKEN=…` over `--token` so the secret does not show up in `ps`. The daemon is headless: it does not serve a browser UI. The desktop command asks the daemon for `--json` session meta on a local pipe, so the token is not placed on the GUI argv.

Only **one background session per user** is allowed (exclusive lock under `$XDG_RUNTIME_DIR/fresh-gui/`). Closing the launching terminal does not stop the session.

While the daemon runs it samples its own resident memory (`VmRSS` / `VmHWM` via `/proc/self/status`) about every 30 seconds. On graceful shutdown (`fresh-gui close` / SIGTERM / Ctrl-C) it appends a structured log line with **average** and **peak** RSS in MB. PTY child shells are not included — only the backend process (server + embedded Fresh editor).

Works on older enterprise glibc (2.28+).

### From your laptop over SSH

```bash
fresh-gui user@server
# saved target:
fresh-gui remote add lab user@server --root /path/to/project
fresh-gui remote connect lab
# or the saved name (a directory with that name in the current folder is opened locally instead):
fresh-gui lab
```

That opens the local window, copies the Linux daemon to the server if it is missing, starts it headless when no session is running, and tunnels ADE `/ws`. You do not SSH in and start a second binary yourself.

A manual tunnel still works:

```bash
# on the server (daemon-only `fresh-gui`, or `fresh-gui --no-ui` where the desktop command is installed)
cd /path/to/your/project
fresh-gui

# on your laptop
ssh -L 7420:127.0.0.1:7420 user@server
fresh-gui --backend 'http://127.0.0.1:7420/?token=…'
```

Do not bind publicly by default. Non-loopback listens still require a token and log a warning; SSH tunnel + loopback is the supported remote path.

### Windows or Linux client (auto-install over SSH)

The desktop command can save an SSH target, install the Linux daemon if it is missing, and open a local tunnel to ADE `/ws`. Auth is your normal OpenSSH setup (keys, agent, `~/.ssh/config`). The app does not prompt for a password: `ssh user@host` must already succeed non-interactively (`BatchMode`).

The [installer](#install) places `fresh-gui` on `PATH`. You can also download the host from a [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) (or build it with `pixi run gui`):

| Asset | Laptop |
|-------|--------|
| `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` | Windows x86_64 |

```bash
# Linux (archive member is fresh-gui; older archives used fresh-gui-app)
tar -xzf fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz
cd fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu
./fresh-gui user@server
./fresh-gui remote add lab user@server --root /path/to/project
./fresh-gui remote connect lab
```

The Linux archive is built on `ubuntu-latest` and needs **glibc ≥ 2.39** (Ubuntu 24.04 or newer; a newer runner image can raise this floor). It is not the daemon's glibc 2.31 zigbuild. It also needs a display (X11 or Wayland), fontconfig, and a Vulkan loader (`libvulkan.so.1`, loaded on demand) plus Wayland client libraries:

```bash
sudo apt install libvulkan1 libfontconfig1 libfreetype6 libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libxcb1
```

```bash
# on the laptop (Windows or Linux)
fresh-gui user@server
fresh-gui remote add lab user@server
# or an OpenSSH Host alias from ~/.ssh/config:
fresh-gui remote add lab my-server --root /path/to/project

# optional — pin the Linux daemon (default: latest GitHub linux-gnu release)
fresh-gui remote daemon --path ./fresh-gui-linux
fresh-gui remote daemon --url 'https://github.com/amirhosseindavoody/fresh-gui/releases/download/vYYYY.MMDD.N/fresh-gui-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz'
# one-shot overrides: FRESH_GUI_DAEMON_PATH, FRESH_GUI_DAEMON_URL

fresh-gui remote list
fresh-gui remote connect lab
```

On `remote connect` the host:

1. SSHs to the target and checks for a `fresh-gui` binary plus a live session (`session.json`).
2. If the binary is missing, copies a Linux daemon to `~/.local/bin/fresh-gui` (from the path, the URL, or the latest GitHub `x86_64-unknown-linux-gnu` release).
3. If no session is running, starts `fresh-gui --no-ui` (headless) and reads the token from the remote session file.
4. Opens `ssh -L 127.0.0.1:<local>:127.0.0.1:<remote>` and connects the GPUI window to `ws://127.0.0.1:<local>/ws`.

Closing the window closes the tunnel. The remote daemon keeps running; the next `remote connect` reuses it. Saved targets live in `~/.config/fresh-gui/remotes.json` (Windows: `%APPDATA%\fresh-gui\remotes.json`). Tokens are not stored there. `ssh` and `scp` must be on `PATH` (OpenSSH).

## Using the native host

The window is GPUI + [gpui-component](https://github.com/longbridge/gpui-component) via gpui-kit. Same machine as the daemon, or a laptop after SSH bootstrap:

```bash
fresh-gui
# from a checkout (builds the daemon, then opens the window):
pixi run gui
# explicit URL, when you already have one:
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'
```

No subcommand is the same as opening the window. Linux needs an X11 or Wayland display plus fontconfig / Vulkan (or a working wgpu backend).

After connect you get terminals, an explorer, and editor tabs in one shell:

| Do this | How |
|---------|-----|
| New terminal | `Ctrl+T` or the **+** beside the last tab (right edge of the bar when the strip is full) |
| Split / reorder | Drag a tab to a pane edge to split; drop it on a tab to merge; drag along the strip to reorder. A split needs two tabs — the last remaining tab does not drag |
| Rename a terminal | Right-click the tab title, or **··· → Rename**. New terminals are titled `1`, `2`, `3`, … inside the focused workspace |
| Close a tab | **×** on the tab, right-click → Close, or `Ctrl+W`. Right-click also has Close Others and Close to the Right |
| New workspace | Left rail **+**, or command palette → New Workspace. The project root starts as the current workspace folder |
| Switch workspace | Click a name in the left rail. Tabs, terminals, and the explorer root swap; other workspaces keep running on the daemon |
| Open a file | Click in the explorer (or `Ctrl+P` → path `[:line[:col]]`) |
| Explorer selection | `Ctrl`/`Cmd`-click toggles; `Shift`-click selects a range. Right-click **Copy Path** copies absolute paths |
| Move / copy files | Drag a file onto a folder to move it. With the explorer focused, `Ctrl+C` / `Ctrl+V` (`Cmd` on macOS) copies via the daemon into the selected folder |
| Save | `Ctrl+S` |
| Command palette | `Ctrl+Shift+P` |
| Toggle sidebar | `Ctrl+B` |
| Settings | Activity bar gear or `Ctrl+,` (opens `config.json`) |
| Next / prev tab | `Ctrl+Tab` / `Ctrl+Shift+Tab` |
| Reconnect | `Ctrl+Shift+R` or command palette |

The product host is this GPUI client. The daemon is a headless ADE WebSocket. A later browser host would be this same GPUI UI via WebAssembly, which is not in this release. Splits and tab reorder use the gpui-component dock. Terminal titles and the in-app file clipboard last for the client session. Still absent: markdown WYSIWYG, a minimap, layout restore, mouse selection in the terminal, find, palette packs, tab pin, and editor-tab rename. Copy Path and in-app paste use absolute ADE paths. On this GPUI snapshot, Linux clipboard writes do not offer `text/uri-list`, so pasting into a file manager is not reliable; paste inside the explorer is a real `fs_copy`.

Disconnect leaves remote workspaces and PTYs running so you can reconnect. Each workspace is its own set of tabs and shells on that daemon; the left rail switches among them. See [docs/WORKSPACES.md](docs/WORKSPACES.md).

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

Open it from the UI (**Settings** / `Mod+,`), edit, save with `Mod+S`. Theme follows the OS by default; terminal chrome tracks the same theme. Empty shell `args` keep interactive / OSC 7 setup for known shells. On Unix the default shell command is `zsh`. If that binary is missing or not executable, a new terminal tries `$SHELL` when it is usable, then `bash`, then `sh`, and logs which one it started. If none of those can run, the status bar names the shells that were tried and points at `terminal.shell.command`. Windows keeps `powershell`. Dotfiles and `.git` directories are hidden in the explorer by default; enable them independently via `showDotfiles` / `showGitDirs`. Editor soft wrap follows Fresh `editor.line_wrap` (on by default); toggle with `Alt+Z` or the command palette.

## Develop from source

```bash
git clone https://github.com/amirhosseindavoody/fresh-gui.git
cd fresh-gui
git submodule update --init --recursive
pixi install

pixi run gui                 # daemon if needed, then the window
pixi run backend -- --foreground   # headless daemon in the foreground (prints the URL)
```

`pixi run serve` starts the headless daemon and returns. `pixi run gui` builds that daemon and runs the GPUI host (`cargo run -p fresh-gui-app`). The Cargo binary is still named `fresh-gui-app` so it does not overwrite `target/debug/fresh-gui`.

Useful tasks: `pixi run check`, `test`, `build`, `gui`, `package` (write `.conda` under `./dist`), `package-binary` (standalone daemon archive; pass target and version), `package-client` (GPUI host archive).

| Piece | Role |
|-------|------|
| `fresh-gui` (installer) | GPUI host. Starts or reuses `fresh-gui-daemon`, or SSH-bootstraps a Linux daemon |
| `fresh-gui` (Cargo / Pixi / daemon archive) | Headless daemon (PTY, FS, Fresh editor, ADE WebSocket) |
| `fresh-gui-app` | Cargo name of the GPUI host. Installers also keep this name as a second link to `fresh-gui` |
| `fresh-gui-protocol` / `fresh-gui-client` | Shared wire format + client library |

Deeper design notes (architecture and behavior): [docs/DESIGN.md](./docs/DESIGN.md), [docs/FRESH.md](./docs/FRESH.md) (Fresh embedding), [docs/SECURITY.md](./docs/SECURITY.md), [docs/UI.md](./docs/UI.md), [docs/COPILOT.md](./docs/COPILOT.md) (Copilot CLI / ACP design). Backend flags and packaging: [crates/fresh-gui/README.md](./crates/fresh-gui/README.md).

## Releases

CalVer `YYYY.MMDD.N`. Pushes to `main` (and manual `workflow_dispatch`) bump the version and publish a [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) (see `.github/workflows/release-backend.yml`):

| Asset | Platform |
|-------|----------|
| `fresh-gui-*-*.conda` | linux-64 headless daemon via Pixi (glibc 2.28+) |
| `fresh-gui-*-x86_64-unknown-linux-gnu.tar.gz` | Linux headless daemon (glibc ≥ 2.31) |
| `fresh-gui-*-x86_64-unknown-linux-musl.tar.gz` | Linux musl headless daemon (Alpine / static-friendly) |
| `fresh-gui-*-x86_64-pc-windows-msvc.zip` | Windows headless daemon |
| `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` | Linux GPUI host (`fresh-gui`; older archives: `fresh-gui-app`) |
| `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` | Windows GPUI host (`fresh-gui.exe`; older archives: `fresh-gui-app.exe`) |

Daemon archives are the headless `bin/fresh-gui` binary (same as the conda package). The **client** archives are the native GPUI host, for a Linux or Windows laptop that talks to a Linux daemon (see [Windows or Linux client](#windows-or-linux-client-auto-install-over-ssh)). The Linux client is built on `ubuntu-latest` and needs glibc ≥ 2.39, not the glibc 2.31 zigbuild used for the daemon. The version-bump commit rebases if `main` moved during the build. Manual bump: `pixi run update-version`.

## License

[GPL-3.0-or-later](./LICENSE) (same as Fresh).
