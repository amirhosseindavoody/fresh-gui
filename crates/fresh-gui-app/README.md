# fresh-gui-app

Native **GPUI** ADE host (`fresh-gui-app`) plus CLI helpers. Optional Vite + TypeScript UI lives under `ui/` for browser smoke tests.

```bash
# Native host (default) — pass the daemon's printed Local access URL
pixi run gui -- --backend 'http://127.0.0.1:7420/?token=…'
# or:
cargo run -p fresh-gui-app -- --backend ws://127.0.0.1:7420/ws --token "$FRESH_GUI_TOKEN"

pixi run app -- ping
pixi run app -- smoke
pixi run app -- attach

# Vite UI (not the primary path)
pixi run ui-install  # once
pixi run ui          # Vite on :1420
pixi run ui-build
pixi run ui-serve    # serve ui/dist on :1420
```

**Native host:** silent connect from `--backend` (`http://…/?token=` or `ws://…/ws`). `Ctrl+T` new terminal, explorer click to open, `Ctrl+S` save, `Ctrl+Shift+P` command palette, `Ctrl+,` settings (`config.json` on the backend). Disconnect keeps the backend session.

Linux needs an X11 or Wayland display, fontconfig, and a working wgpu/Vulkan backend. Combined with gpui-kit (Apache-2.0) the application is GPL-2.0.

Product overview: [README.md](../../README.md). Architecture: [docs/DESIGN.md](../../docs/DESIGN.md), [docs/UI.md](../../docs/UI.md).
