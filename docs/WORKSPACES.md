# Workspaces

A daemon can hold several project workspaces. Each workspace has a name, root folder, terminal session, tab list, and expanded explorer folders. The GPUI client shows them in the workspace rail; switching changes the workspace shown without stopping its shells.

## Persistence

The daemon saves workspace names, roots, order, focused workspace, tab titles and order, active tab, editor paths, and expanded explorer folders. It does not save running processes or terminal scrollback. After a daemon restart, terminal tabs return as new shells in their workspace root.

On Linux, the file is `workspaces.json` in `$XDG_STATE_HOME/fresh-gui/` (normally `~/.local/state/fresh-gui/`). On Windows it is under `%LOCALAPPDATA%\fresh-gui\`. `FRESH_GUI_WORKSPACES_FILE` overrides the location and enables persistence in foreground mode.

Editor typing reaches daemon-owned Fresh buffers continuously when range edits are negotiated. The daemon checkpoints accepted edits in durable draft storage scoped to the workspace. Pending edits are flushed before workspace switch, client disconnect, and window close. On workspace activation or reconnect, saved drafts are listed and restored with their path or stable untitled identity and dirty state. Missing or externally changed source files retain the draft and display a review warning; choosing how to merge external changes remains separate (#143). The client negotiates `editor.draft-recovery`; older daemons cannot guarantee recovery, so dirty-buffer close actions use a native Save / Discard / Cancel prompt. It covers individual tabs, close-others/right/all, pane close, and window close. Cancel leaves the entire bulk selection open. Save waits for every selected buffer to save successfully, including sequential Save As prompts for untitled buffers, before completing the close. Failed saves and cancelled Save As leave every selected tab open. The tab menu also offers **Discard Draft and Close** for explicitly removing a retained draft. The same daemon-side store serves local and SSH clients. A crash can lose edits still in the client’s 75 ms coalescing window or queued behind an unacknowledged request. Deliberate transitions wait for retention; workspace switches wait up to five seconds, and disconnect/restart commands leave the current views open and ask for a retry while drafts are still syncing. Dirty Source Control diff views use the close prompt and block transitions, but their edits are not continuously checkpointed.

Closing the client window disconnects it and leaves the daemon and its workspaces running. `fresh-gui close` stops the daemon. The last workspace cannot be closed. A workspace with recovery entries cannot be deleted until its drafts are saved or explicitly discarded; reopening the workspace brings retained closed drafts back for review.

## Name and location

Right-click a workspace in the rail to rename it or choose **Change location…**. The daemon saves the new root and authorizes it for file operations. The explorer reloads that folder. Source Control requests git status for the same workspace, so the branch, file list, and root path follow the new location. Diff tabs that were open for the previous location close when the active workspace moves.

## Scope

Workspace roots are authorized alongside the daemon root for file operations; they are not separate OS-level sandboxes. Editor buffers are managed by the daemon process. Dock split geometry is persisted with the layout. Fresh currently deduplicates named files across its editor instance, so opening the same file in another workspace is explicitly rejected while the first workspace still holds its buffer; recovery copies remain isolated.

See the [README](../README.md) for normal use and [Architecture](./DESIGN.md) for the client/daemon split.
