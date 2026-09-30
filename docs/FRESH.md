# Fresh editor integration

The daemon embeds Fresh's editor library for buffer open, edit, and save. The GPUI client does not link Fresh; it exchanges editor snapshots and edits with the daemon over ADE.

## Build setup

Fresh is a pinned git submodule in `vendor/fresh`. Initialize it when cloning:

```sh
git clone --recurse-submodules https://github.com/amirhosseindavoody/fresh-gui.git
```

The daemon crate depends on `fresh-editor` with its `runtime` feature. Fresh's web UI, GUI, and plugins are not part of the desktop host. The daemon runs Fresh's `!Send` editor on its own thread and sends editor operations to that thread.

To update the pin, check out the desired commit in `vendor/fresh` and update `vendor/fresh.rev` to the same SHA. The package recipe uses that revision when the submodule is unavailable.

## Runtime boundary

Fresh provides the editor buffer and save behavior. `fresh-gui` provides the ADE protocol, client connection, PTYs, file explorer sandbox, workspace state, and GPUI rendering. Normal editor views continuously send revisioned range transactions when the daemon advertises `editor.range-edits`. The existing `BufferEdit` full-text CAS path remains available for older peers; the protocol version stays `0.4.0`. Saving writes through Fresh after pending transactions have been acknowledged.

Language servers use the same daemon-side Fresh `LspManager` and its local Authority. The worker pumps Fresh's editor tick, applies ADE edits through Fresh events so servers receive `didChange`, and closes Fresh buffers so they receive `didClose`. The GUI's `lsp` configuration is passed into Fresh before editor construction; the ADE bridge polls merged diagnostics and carries explicit format requests/results. Server commands resolve on the daemon host, including when a Windows GUI connects to a Linux daemon. Built-in Fresh LSP defaults are not started by Fresh GUI unless configured in its `config.json`.

The daemon can run without the editor using `--no-editor`; terminal and filesystem features remain available. See [Architecture](./DESIGN.md) for process boundaries and [WORKSPACES.md](./WORKSPACES.md) for workspace behavior.

## Edit transactions and actions

The bridge uses the pinned Fresh revision `14f7d28b7ab1` without changing it. `Editor::log_and_apply_event` applies an `Event::Batch` containing `Delete`, `Insert`, and `MoveCursor`; Fresh owns the event log, inverse events, atomic undo groups, buffer side effects, and LSP notifications. `Editor::handle_undo` and `handle_redo` execute actions on the explicitly selected buffer. The bridge does not maintain another undo stack.

`BufferRangeEdit` carries a buffer ID, client view ID, base revision, ordered replacement ranges, and the post-edit primary selection. `BufferAction` carries the same identities, base revision, and pre-action primary selection. All ranges and selections are **UTF-8 byte offsets**, on scalar boundaries; ranges are half-open and each range indexes the text after preceding ranges. GPUI Kit 0.6.6 exposes byte-based `cursor()` and `selected_range()`, so these positions already match Fresh. LSP diagnostic columns and `didChange` positions are **UTF-16 code units** from the line start, converted by Fresh; emoji and CJK are covered by bridge tests. Other existing line/column fields retain their documented conventions.

Fresh's pinned batch LSP collector calculates sub-event positions against the original buffer. The worker therefore validates the entire ordered transaction first, then translates its net effect into one contiguous range replacement. This makes the delete/insert coordinates correct both for Fresh and for LSP's sequential change arrays, including undo/redo. It can replace the unchanged text between distant edits; a public Fresh remote-transaction API with sequential position conversion would allow finer multi-range notifications later.

The client coalesces changes over a fixed 75 ms window, with at most one outstanding transaction per view. The timer does not restart on every keystroke. Later typing stays in the visible draft and is diffed against the last acknowledgement. Save, format, undo, and redo flush preceding changes and wait for their replies. Native GPUI remains responsible for input and presentation; negotiated undo/redo actions use Fresh's history.

The worker observes Fresh's current text before revision comparisons, including asynchronous formatting. A stale transaction returns `BufferEditResult` with `accepted=false`, current revision, text, selection, Fresh modified state, and both identities. `BufferSync` fetches the authoritative state after a transport reconnect; periodic state polling also reconciles server-originated changes. The reusable client `EditSync` holds the acknowledged base, sent text, and a divergent server snapshot. It keeps the local draft when both sides changed, pauses automatic writes, and lets the user choose **Keep my edits** or **Use server text**. Clean views adopt server changes. A lost acknowledgement can be recognized by comparing the snapshot to the exact text sent.

## API boundaries and follow-ups

Fresh already exposes the event and undo APIs needed for this bridge. A stable public `apply_remote_transaction(buffer, view, edits, selection)` facade would avoid active-buffer switching and expose richer view/multi-cursor semantics; the current bridge carries one primary selection and serializes operations on the worker. View IDs correlate requests and replies; they do not create independent Fresh splits. Standalone cursor movements are presented locally; edit and action requests transfer their selection positions. GPUI Kit has local undo behavior and change events, but its arbitrary range replacement and transaction APIs are private. Authoritative action snapshots use its public text setter and restore the primary selection. General editor commands and multi-cursor parity remain follow-up work. The SCM split diff editor retains its existing full-text save path; the continuous action bridge currently applies to normal editor tabs.

This bridge retains current live editor views during a transport reconnect. It does not add persistent draft storage, close/restart recovery (#142), or external-file reload policy (#143). Untitled drafts remain visible but paused after reconnect until a safe daemon buffer reattachment is available. Manual native GUI and Windows-to-Linux SSH sessions are not exercised by the automated tests.
