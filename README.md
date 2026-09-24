# fresh-gui

fresh-gui is a native desktop app for working with files, terminals, and Git in a project. Its window connects to a headless daemon on your computer or a Linux server over SSH. Releases have no browser UI.

## Install

**Windows (x86_64):** Run in PowerShell:

```powershell
irm https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.ps1 | iex
```

**Linux (x86_64):** Run in a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | sh
```

The installers download the latest [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases), check published SHA-256 files when available, and put the app and daemon in `~/.fresh-gui/bin` (Windows: `%USERPROFILE%\.fresh-gui\bin`). Open a new terminal after installation so `fresh-gui` is on your `PATH`. There is no macOS release.

If a local daemon is running during an update, the installer asks before stopping it. For an unattended update, set `FRESH_GUI_STOP_DAEMON=1` to approve stopping it; without a console or that setting, the update stops before replacing binaries. The Windows script also accepts `-StopDaemon` when run from a file.

On Linux, the desktop app needs an X11 or Wayland session, fontconfig, and Vulkan. The published Linux client needs glibc 2.39 or newer.

## Open locally

```bash
fresh-gui                   # open the current directory
fresh-gui /path/to/project  # open a project
fresh-gui status            # show the daemon session
fresh-gui close             # stop the daemon session
```

The app starts a local daemon if needed and reuses it next time. Closing the window leaves the daemon running; `fresh-gui close` stops it.

## Open a Linux server over SSH

From your Windows or Linux laptop:

```bash
fresh-gui user@host
```

The app installs the Linux daemon on the server if needed, starts or reuses its session, opens an SSH tunnel, and shows the desktop window on your laptop. If the local client is newer than the remote daemon, it asks before stopping the remote session and installing a matching daemon. Run the command in an interactive terminal to approve the upgrade. Set up OpenSSH keys or an agent first: `ssh user@host` must work without a password prompt.

For a server you visit often, save it:

```bash
fresh-gui remote add lab user@host --root /path/to/project
fresh-gui remote connect lab
```

The remote daemon keeps running after you close the window. The SSH path targets Linux servers.

## Everyday use

- Use the left rail to create and switch workspaces. Each workspace has its own files and tabs. Right-click a workspace to rename it or choose **Change location…**. Source Control follows that folder.
- Open files in the explorer. Use the **+** in the tab bar for a shell, and drag a tab to the edge of another pane to split the view horizontally or vertically. Terminal tabs split the same way. The last remaining tab cannot be dragged.
- Right-click a file or folder and choose **Copy Path** to copy its absolute path as text. For SSH sessions, this is the path on the remote server.
- `Ctrl+=` or `Ctrl++` zooms editor and terminal text. `Ctrl+-` zooms out, and `Ctrl+0` resets that zoom. `Ctrl+Shift+=` (`Ctrl+Shift++` on a US keyboard) zooms the rest of the UI text, with `Ctrl+Shift+-` and `Ctrl+Shift+0` to zoom out and reset. Those chords are in the default shortkeys list, and the command palette lists the same actions. Sidebar width and the tab strip stay a fixed size.
- Use the Source Control icon to review changes, stage files, commit, pull, and push. The pane shows the git root for the active workspace. Git runs on the daemon's machine.
- Open Settings from the gear icon or `Ctrl+,`. Settings live on the daemon's machine, including for remote sessions.
- Use **Quit Client** to close the window or **Stop Server** to end a local daemon session.

Workspaces and tabs come back when the daemon restarts. Shell tabs reopen as new shells; running shell processes do not survive a daemon restart. See [workspaces](docs/WORKSPACES.md) for details.

## Daemon-only and development

For a headless Linux server, install only the daemon:

```bash
curl -fsSL https://raw.githubusercontent.com/amirhosseindavoody/fresh-gui/main/scripts/install.sh | FRESH_GUI_COMPONENTS=daemon sh
```

This installs the daemon as `fresh-gui` without a window. The default installer gets both components for a laptop.

To build from source:

```bash
git clone https://github.com/amirhosseindavoody/fresh-gui.git
cd fresh-gui
git submodule update --init --recursive
pixi install
pixi run gui
```

`pixi run gui` builds the daemon and opens the native window. See the [daemon README](crates/fresh-gui/README.md) for server flags, [architecture](docs/DESIGN.md) for how the pieces fit, and [security notes](docs/SECURITY.md) for remote access.

## License

[GPL-3.0-or-later](LICENSE).
