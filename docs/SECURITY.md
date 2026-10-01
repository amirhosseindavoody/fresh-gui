# Security

The daemon exposes a terminal and file operations, so treat access to its connection token as shell access under the daemon user's account.

## Default access

The daemon listens on loopback (`127.0.0.1:7420`) and requires a bearer token, generated for each daemon start unless configured explicitly. The desktop command reads the local session details and attaches automatically. `fresh-gui status` prints daemon status; `fresh-gui close` stops it.

For a Linux server, use `fresh-gui user@host` or a saved remote connection. The client uses OpenSSH to start or find the daemon and forwards a loopback port. The remote daemon remains running after the client disconnects. Do not expose the daemon on a public interface unless you have arranged access controls yourself.

## Token and state

Session metadata, including the token, is stored in a private runtime file so the desktop command can reconnect. On Unix it is under `$XDG_RUNTIME_DIR/fresh-gui/` (with a per-user temporary fallback); on Windows it is under `%LOCALAPPDATA%\fresh-gui\`. Prefer `FRESH_GUI_TOKEN` to a `--token` command-line argument when setting a fixed token, since command arguments may be visible in process listings.

Workspace metadata is saved separately in the daemon's state directory. It contains workspace names, roots, tab titles, and open explorer folders, but not file contents or the access token.

Settings writes follow their authority: local client UI preferences are written by GPUI on the client machine, while daemon user settings and workspace `.fresh/config.json` files are read and written by the authenticated daemon. Workspace settings paths are derived from the selected workspace root. JSONC updates retain comments and unknown keys in the existing config document. As with other daemon filesystem operations, workspace roots are authorized paths rather than separate OS sandboxes. Workspace files are saved on the daemon host; Fresh loads project and session settings for the daemon's startup directory, while fresh-gui does not re-resolve them on each active-workspace change.

`--allow-no-auth` is intended for local tests and only works with a loopback bind. The daemon rejects combining it with a non-loopback address.

For install and remote connection commands, see the [README](../README.md).

Daemon recovery copies contain unsaved editor text and are stored beside workspace state in `workspaces.drafts/`. On Unix the recovery directory is created with mode `0700` and recovery files with mode `0600`; Windows uses the state directory’s inherited ACLs. Local and SSH clients access these copies through the authenticated ADE protocol, scoped to their attached workspace. External editor change notifications also require authentication and the negotiated capability, and are filtered to that workspace.

Paged reads and viewport edits require authentication, negotiated editor capabilities, and the same attached-workspace buffer ownership checks as normal edits. Large recovery journals use separate `*.paged.json` files with the draft store's existing private-file policy; older daemons cannot interpret them as empty full-text drafts. Streamed patched saves create Unix temporary files with mode `0600` in the destination directory and restore an existing destination's permissions before rename. New paged Save As files remain private on Unix. Windows uses inherited directory ACLs.
