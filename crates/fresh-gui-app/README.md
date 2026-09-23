# Desktop host

This crate builds the native GPUI desktop host. The installed command is `fresh-gui`; it starts or attaches to the local daemon and opens a window.

From a checkout:

```bash
pixi run gui
```

Common installed commands:

```bash
fresh-gui                 # open the local desktop
fresh-gui /path/to/project # open a project as a workspace
fresh-gui user@host        # connect to a Linux daemon over SSH
fresh-gui status           # show the local daemon status
fresh-gui close            # stop the local daemon
```

The host uses the `fresh-gui-client` crate to connect to the daemon and `fresh-gui-protocol` for shared message types. See the [project README](../../README.md) for installation and user guidance.
