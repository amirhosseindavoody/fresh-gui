# Architecture

`fresh-gui` is a native GPUI desktop client paired with a headless daemon. The desktop command attaches to a local daemon or bootstraps a Linux daemon over SSH. Releases do not include a browser UI.

## Processes

- The **client** renders the workspace rail, explorer, Source Control, editor, and terminal tabs. It connects to the daemon using the ADE WebSocket protocol.
- The **daemon** owns sessions, terminal processes, workspace state, and sandboxed file operations. Linux is the remote SSH target; standalone daemon builds also exist for Windows.
- The daemon embeds Fresh's editor library for opening, editing, and saving buffers. The client displays those buffers in a native GPUI editor view.

A local `fresh-gui` command attaches to or starts the daemon and opens the window. `fresh-gui user@host` uses the system OpenSSH client to install/start a Linux daemon when needed and opens a tunneled connection. Closing the client window disconnects it; `fresh-gui close` stops the local daemon.

## Client chrome

The GPUI window follows the daemon's theme setting (`system`, `light`, or `dark`). On a light theme the client darkens borders and split handles so they stay visible on a bright background. The split git diff draws a center rule with that same border color.

The title bar shows the SSH destination for remote sessions, or a short local/direct host label. The status bar shows connection state and the active workspace; protocol capability names remain internal feature gates. Explorer **Copy Path** puts the daemon's absolute path on the client's text clipboard, including when a Windows client is connected to a Linux daemon. Terminal copy and paste use that same clipboard. Tab in a focused terminal is delivered to the PTY; the window's Tab focus cycle does not take it. **File → New Tab** opens a shell and **File → New File** opens an unsaved buffer. **File → Save** and Ctrl+S write the active buffer; an unsaved buffer asks for a path and is created there. The tab-strip **+** offers New Tab and New File.

`Ctrl+=` / `Ctrl+-` / `Ctrl+0` zoom editor and terminal text. `Ctrl+Shift+=` / `Ctrl+Shift+-` / `Ctrl+Shift+0` zoom rem-based UI text; panel text follows both scales. Those chords are default shortkeys in the daemon config. Fixed pixel chrome, including the sidebar width and dock tab strip, does not scale.

Windows embeds `assets/fresh-gui.ico` as icon resource 1. Linux sets the X11 window icon from `assets/fresh-gui.png` and writes a user desktop entry so a Wayland dock can resolve the `fresh-gui` app id.

## Source layout

- `crates/fresh-gui`: daemon, ADE handlers, sessions, PTY, filesystem, and embedded Fresh editor.
- `crates/fresh-gui-app`: native GPUI client.
- `crates/fresh-gui-client` and `crates/fresh-gui-protocol`: client transport and shared protocol types.
- `vendor/fresh`: pinned Fresh submodule used by the daemon.

For Fresh embedding details, see [FRESH.md](./FRESH.md). For workspace persistence, see [WORKSPACES.md](./WORKSPACES.md). Access and token behavior is in [SECURITY.md](./SECURITY.md).
