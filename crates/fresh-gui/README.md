# Daemon

This crate builds the headless `fresh-gui` daemon. It runs on Linux and Windows; the supported SSH remote workflow starts a Linux daemon. The desktop host is a separate native GPUI application, and installed releases expose it as the `fresh-gui` command.

The daemon owns the user's workspaces and terminal sessions. In normal use, the desktop command starts or attaches to it automatically. To manage a local daemon directly:

```bash
cargo run -p fresh-gui --                 # start the daemon in the background
cargo run -p fresh-gui -- status          # show session details
cargo run -p fresh-gui -- close           # stop the session
```

For foreground development and debugging:

```bash
cargo run -p fresh-gui -- --foreground --listen 127.0.0.1:7420 --root /path/to/project
```

The daemon exposes a health endpoint and an authenticated WebSocket endpoint at `/ws`. It does not serve a browser interface. A bearer token is generated when one is not configured; use `FRESH_GUI_TOKEN` for local development rather than passing a secret on the command line.

The daemon persists workspace names, roots, tab metadata, and open explorer folders. Terminal processes are recreated when the daemon restarts. Its config file is `~/.config/fresh-gui/config.json` on Linux and `%APPDATA%\fresh-gui\config.json` on Windows. The `--config` flag selects another path.

Useful development options:

- `--foreground`: keep the daemon attached to the terminal.
- `--listen ADDRESS`: choose the bind address (default `127.0.0.1:7420`).
- `--root PATH`: set the filesystem and editor root (default: current directory).
- `--token TOKEN` or `FRESH_GUI_TOKEN`: set the authentication token.
- `--no-editor`: disable Fresh editor support.

See the [project README](../../README.md) for installation and daily use.
