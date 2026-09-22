# fresh-gui (GPUI host)

Native **GPUI** ADE host. Cargo builds it as `fresh-gui-app` so the file does not collide with the daemon binary. Installers and `scripts/package-client.sh` put it on `PATH` as **`fresh-gui`**.

```bash
# Starts the local daemon when this user has none, then opens the window.
# The token comes from the session file (daemon --json), not from the command line.
pixi run gui
fresh-gui
fresh-gui /path/to/project

# Explicit backend, when you already have a URL:
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'
cargo run -p fresh-gui-app -- --backend ws://127.0.0.1:7420/ws --token "$FRESH_GUI_TOKEN"

pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach
fresh-gui status
fresh-gui close
```

`pixi run gui` builds the daemon (`cargo build -p fresh-gui`) first. In `target/debug` the host finds the sibling `fresh-gui` daemon. An installed layout looks for `fresh-gui-daemon` beside the host, then on `PATH`. `FRESH_GUI_DAEMON` overrides that lookup.

**Native host:** silent connect. A left rail lists workspaces on the daemon (create, switch, rename, close). New Workspace starts in the current workspace folder. Each workspace has its own dock tabs and terminals. `Ctrl+T` new terminal (titled `1`, `2`, `3`, … inside that workspace; the **+** sits beside the last tab). Right-click or **··· → Rename**. Each tab has **×**; right-click also closes that tab, the others, or the tabs to the right. `Ctrl+W` closes the active tab. Windows paths are shown without the `\\?\` prefix. Drag a tab to a pane edge to split, onto a tab to merge, or along the strip to reorder. Explorer: Ctrl/Cmd-click and Shift-click to multi-select, right-click **Copy Path** (absolute), drag onto a folder to move, `Ctrl+C` / `Ctrl+V` to copy inside the tree. `Ctrl+S` save, `Ctrl+Shift+P` command palette, `Ctrl+,` settings (`config.json` on the backend). Disconnect keeps the daemon and its workspaces; tab titles and the in-app file clipboard do not. `fresh-gui /path` while a session is already running focuses or creates a workspace at that root.

**SSH remote:** one command installs the Linux daemon if needed, starts it headless, tunnels `/ws`, and opens the window.

```bash
fresh-gui user@server
fresh-gui remote add lab user@server --root /path/to/project
fresh-gui remote daemon --path ./fresh-gui    # or --url <linux-gnu.tar.gz>, or omit for GitHub latest
fresh-gui remote connect lab                  # window; closing it closes the tunnel
fresh-gui lab                                 # saved name, when no local directory has that name
```

OpenSSH only (`ssh` / `scp` on `PATH`, keys or agent). Targets are stored in `~/.config/fresh-gui/remotes.json` (`%APPDATA%\fresh-gui\remotes.json` on Windows). The ADE token is not saved. `--no-ui` starts or reuses the local daemon and does not open a window (what the remote probe runs).

GitHub Releases publish this host as `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` and `fresh-gui-client-*-x86_64-pc-windows-msvc.zip`. New archives name the file `fresh-gui` / `fresh-gui.exe`. Older archives name it `fresh-gui-app`. The repo installer accepts both and installs the command as `fresh-gui`. See the root [README](../../README.md#install).

```bash
tar -xzf fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz
cd fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu
./fresh-gui user@server
./fresh-gui remote connect lab
```

Linux needs glibc ≥ 2.39, an X11 or Wayland display, fontconfig, and a working wgpu/Vulkan backend (`libvulkan1` plus Wayland/XKB/XCB). The Linux release binary is built on `ubuntu-latest`. Combined with gpui-kit (Apache-2.0) the application is GPL-3.0-or-later.

Product overview: [README.md](../../README.md). Architecture: [docs/DESIGN.md](../../docs/DESIGN.md), [docs/UI.md](../../docs/UI.md).
