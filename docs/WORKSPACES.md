# Workspaces

A daemon can hold several project workspaces. Each workspace has a name, root folder, terminal session, tab list, and expanded explorer folders. The GPUI client shows them in the workspace rail; switching changes the workspace shown without stopping its shells.

## Persistence

The daemon saves workspace names, roots, order, focused workspace, tab titles and order, active tab, editor paths, and expanded explorer folders. It does not save running processes or terminal scrollback. After a daemon restart, terminal tabs return as new shells in their workspace root.

On Linux, the file is `workspaces.json` in `$XDG_STATE_HOME/fresh-gui/` (normally `~/.local/state/fresh-gui/`). On Windows it is under `%LOCALAPPDATA%\fresh-gui\`. `FRESH_GUI_WORKSPACES_FILE` overrides the location and enables persistence in foreground mode.

Closing the client window disconnects it and leaves the daemon and its workspaces running. `fresh-gui close` stops the daemon. The last workspace cannot be closed.

## Scope

Workspace roots are authorized alongside the daemon root for file operations; they are not separate OS-level sandboxes. Editor buffers are managed by the daemon process. Dock split geometry is not persisted.

See the [README](../README.md) for normal use and [Architecture](./DESIGN.md) for the client/daemon split.
