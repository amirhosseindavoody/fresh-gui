# Architecture

`fresh-gui` is a native GPUI desktop client paired with a headless daemon. The desktop command attaches to a local daemon or bootstraps a Linux daemon over SSH. Releases do not include a browser UI.

## Processes

- The **client** renders the workspace rail, explorer, Source Control, editor, and terminal tabs. It connects to the daemon using the ADE WebSocket protocol.
- The **daemon** owns sessions, terminal processes, workspace state, and sandboxed file operations. Linux is the remote SSH target; standalone daemon builds also exist for Windows.
- The daemon embeds Fresh's editor library for opening, editing, and saving buffers. The client displays those buffers in a native GPUI editor view.

A local `fresh-gui` command attaches to or starts the daemon and opens the window. `fresh-gui user@host` uses the system OpenSSH client to install/start a Linux daemon when needed and opens a tunneled connection. Closing the client window disconnects it; `fresh-gui close` stops the local daemon.

## Source layout

- `crates/fresh-gui`: daemon, ADE handlers, sessions, PTY, filesystem, and embedded Fresh editor.
- `crates/fresh-gui-app`: native GPUI client.
- `crates/fresh-gui-client` and `crates/fresh-gui-protocol`: client transport and shared protocol types.
- `vendor/fresh`: pinned Fresh submodule used by the daemon.

For Fresh embedding details, see [FRESH.md](./FRESH.md). For workspace persistence, see [WORKSPACES.md](./WORKSPACES.md). Access and token behavior is in [SECURITY.md](./SECURITY.md).
