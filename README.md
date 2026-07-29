# fresh-gui

Terminal-first ADE (agentic / IDE-like) **GUI on the local host**, powered by a **remote [Fresh](https://github.com/sinelaw/fresh) editor backend**.

Inspired by [Terax](https://github.com/crynta/terax-ai). Repo logistics follow [pixi-mise](https://github.com/amirhosseindavoody/pixi-mise).

**Status:** Phase 0 (scaffold). See **[docs/DESIGN.md](./docs/DESIGN.md)** for architecture, MVP phases, and open decisions.

## Split

| Piece | MVP platform | Crate / binary |
|-------|--------------|----------------|
| Host GUI | Windows | `fresh-gui-app` → `fresh-gui` (Tauri 2 + xterm.js) |
| Remote backend | Linux | `fresh-gui-backend` |
| Shared protocol | all | `fresh-gui-protocol` |
| Host client lib | all | `fresh-gui-client` |

## Development

Requires [Pixi](https://pixi.sh/).

```bash
pixi install
pixi run check
pixi run test
pixi run build

# stubs
pixi run backend -- --help
pixi run app -- --help
```

On this machine, Fresh is vendored as a **git submodule** at `vendor/fresh`, pinned by commit (see DESIGN.md §10 D3). After clone: `git submodule update --init --recursive`.

## Versioning

CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`):

```bash
pixi run update-version
pixi run update-version -- --set 2026.728.2
```

## License

[GPL-2.0](./LICENSE) (same as Fresh). See DESIGN.md §9 / D5.
