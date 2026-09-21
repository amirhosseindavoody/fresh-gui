# fresh-gui-app

Native **GPUI** ADE host (`fresh-gui-app`) plus CLI helpers. This is the only host UI.

```bash
# Native host (default) — pass the daemon's printed Local access URL
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'
# or:
cargo run -p fresh-gui-app -- --backend ws://127.0.0.1:7420/ws --token "$FRESH_GUI_TOKEN"

pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach
```

**Native host:** silent connect from `--backend` (`http://…/?token=` or `ws://…/ws`). `Ctrl+T` new terminal, explorer click to open, `Ctrl+S` save, `Ctrl+Shift+P` command palette, `Ctrl+,` settings (`config.json` on the backend). Disconnect keeps the backend session.

**SSH remote:** save a Linux target and let the host install the daemon, start it headless, and tunnel `/ws`.

```bash
fresh-gui-app remote add lab user@server --root /path/to/project
fresh-gui-app remote daemon --path ./fresh-gui    # or --url <linux-gnu.tar.gz>, or omit for GitHub latest
fresh-gui-app remote connect lab                  # GPUI window; closing it closes the tunnel
```

OpenSSH only (`ssh` / `scp` on `PATH`, keys or agent). Targets are stored in `~/.config/fresh-gui/remotes.json` (`%APPDATA%\fresh-gui\remotes.json` on Windows). The ADE token is not saved.

GitHub Releases publish this host as `fresh-gui-client-*-x86_64-unknown-linux-gnu.tar.gz` (`./fresh-gui-app`) and `fresh-gui-client-*-x86_64-pc-windows-msvc.zip` (`fresh-gui-app.exe`).

```bash
tar -xzf fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu.tar.gz
cd fresh-gui-client-YYYY.MMDD.N-x86_64-unknown-linux-gnu
./fresh-gui-app remote add lab user@server --root /path/to/project
./fresh-gui-app remote connect lab
```

Linux needs glibc ≥ 2.39, an X11 or Wayland display, fontconfig, and a working wgpu/Vulkan backend (`libvulkan1` plus Wayland/XKB/XCB). The Linux release binary is built on `ubuntu-latest`. Combined with gpui-kit (Apache-2.0) the application is GPL-3.0-or-later.

Product overview: [README.md](../../README.md). Architecture: [docs/DESIGN.md](../../docs/DESIGN.md), [docs/UI.md](../../docs/UI.md).
