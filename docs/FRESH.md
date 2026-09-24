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

Fresh provides the editor buffer and save behavior. `fresh-gui` provides the ADE protocol, client connection, PTYs, file explorer sandbox, workspace state, and GPUI rendering. The client sends full buffer edits with a revision token; the daemon applies them to Fresh and saves through Fresh.

Language servers use the same daemon-side Fresh `LspManager` and its local Authority. The worker pumps Fresh's editor tick, applies ADE edits through Fresh events so servers receive `didChange`, and closes Fresh buffers so they receive `didClose`. The GUI's `lsp` configuration is passed into Fresh before editor construction; the ADE bridge polls merged diagnostics and carries explicit format requests/results. Server commands resolve on the daemon host, including when a Windows GUI connects to a Linux daemon. Built-in Fresh LSP defaults are not started by Fresh GUI unless configured in its `config.json`.

The daemon can run without the editor using `--no-editor`; terminal and filesystem features remain available. See [Architecture](./DESIGN.md) for process boundaries and [WORKSPACES.md](./WORKSPACES.md) for workspace behavior.
