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

Symbol destinations are opaque LSP URIs on the client. The authenticated daemon decodes file URIs and opens them through the existing authorized editor path in the attached workspace; non-file URIs are rejected. `lsp.navigation` is negotiated separately from the generic request bridge, so older daemons are handled explicitly. No client filesystem access or separate language-server process path is introduced.

Diagnostics, formatting, on-save actions, and language-server controls use the same daemon authority. Selection formatting and lifecycle requests require the additive `lsp.controls` capability and attached-workspace buffer ownership. Project and session settings can disable daemon-configured servers but cannot introduce commands or re-enable disabled servers. Formatter commands run on the daemon host through Fresh. Language logs are bounded; ambiguous log and progress notifications are withheld when Fresh does not identify their workspace.

Project search and replacement require the authenticated `project.search.v1` capability and are confined to the attached workspace root. Enumeration, reads, conflict checks, and writes run on the daemon host for both local and SSH clients. Search streams bounded results and supports cancellation; replacement uses revision checks for open drafts and disk-generation checks for unopened files. Binary or invalid UTF-8 files and files over the 2 MiB per-file limit are skipped. Workspace roots remain authorized paths, not separate operating-system sandboxes.

LSP workspace edits use the additive `lsp.workspace-edits` capability and daemon-issued expiring, single-use tokens. The daemon rechecks buffer revisions, per-server LSP document versions, and closed-file disk generations at apply time. It rejects paths outside daemon authorization. Resource operations reject open buffers, and closed-file edits reject paths with recovery drafts in any workspace. Closed text edits are limited to regular UTF-8 files of at most 2 MiB, with at most 64 files and 8 MiB per transaction. Resource operations reject overwrites, directories, and symlinks. Closed-file staging attempts rollback after errors, but cannot guarantee crash-atomic replacement across multiple files or exclude races with external writers; failed rollback reports affected paths. Server `workspace/applyEdit` requests are previewed, though Fresh's current callback acknowledges before the user accepts because it exposes no delayed response mechanism.
