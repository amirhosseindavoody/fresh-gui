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

The app installs the Linux daemon on the server if needed, starts or reuses its session, opens an SSH tunnel, and shows the desktop window on your laptop. Set up OpenSSH keys or an agent first: `ssh user@host` must work without a password prompt.

For a server you visit often, save it:

```bash
fresh-gui remote add lab user@host --root /path/to/project
fresh-gui remote connect lab
```

The remote daemon keeps running after you close the window. The SSH path targets Linux servers.

## Everyday use

- Use the left rail to create and switch workspaces. Each workspace has its own files and tabs.
- Open files in the explorer. Use the **+** in the tab bar for a shell, and drag tabs to split the view.
- Use the Source Control icon to review changes, stage files, commit, pull, and push. Git runs on the daemon's machine.
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
