# Security

The daemon exposes a terminal and file operations, so treat access to its connection token as shell access under the daemon user's account.

## Default access

The daemon listens on loopback (`127.0.0.1:7420`) and requires a bearer token, generated for each daemon start unless configured explicitly. The desktop command reads the local session details and attaches automatically. `fresh-gui status` prints daemon status; `fresh-gui close` stops it.

For a Linux server, use `fresh-gui user@host` or a saved remote connection. The client uses OpenSSH to start or find the daemon and forwards a loopback port. The remote daemon remains running after the client disconnects. Do not expose the daemon on a public interface unless you have arranged access controls yourself.

## Token and state

Session metadata, including the token, is stored in a private runtime file so the desktop command can reconnect. On Unix it is under `$XDG_RUNTIME_DIR/fresh-gui/` (with a per-user temporary fallback); on Windows it is under `%LOCALAPPDATA%\fresh-gui\`. Prefer `FRESH_GUI_TOKEN` to a `--token` command-line argument when setting a fixed token, since command arguments may be visible in process listings.

Workspace metadata is saved separately in the daemon's state directory. It contains workspace names, roots, tab titles, and open explorer folders, but not file contents or the access token.

`--allow-no-auth` is intended for local tests and only works with a loopback bind. The daemon rejects combining it with a non-loopback address.

For install and remote connection commands, see the [README](../README.md).
