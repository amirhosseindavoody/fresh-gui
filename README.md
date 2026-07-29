# fresh-gui

A **terminal-first IDE shell** for a remote Linux machine. Run the backend on your server; open shells, edit files, and browse the tree from a browser (or the Windows host app).

Inspired by [Terax](https://github.com/crynta/terax-ai). The remote editor core is [Fresh](https://github.com/sinelaw/fresh).

## Install the backend (Linux)

On the machine that holds your project:

```bash
pixi global install --git https://github.com/amirhosseindavoody/fresh-gui.git
# or a release tag / .conda from https://github.com/amirhosseindavoody/fresh-gui/releases

cd /path/to/your/project
fresh-gui-backend
```

The process prints something like:

```text
  UI:  http://127.0.0.1:7420/
  WS:  ws://127.0.0.1:7420/ws
```

Open the **UI** URL in a browser and click **Connect**. (The `ws://` line is for the client — you do not browse it.)

Works on older enterprise glibc (2.28+).

### From your laptop over SSH

```bash
# on the server
fresh-gui-backend --listen 127.0.0.1:7420 --token secret --root "$PWD"

# on your laptop
ssh -L 7420:127.0.0.1:7420 user@server
# browser → http://127.0.0.1:7420/  (paste the same token if you set one)
```

Non-loopback binds require `--token` / `FRESH_GUI_TOKEN`. On loopback, a token is optional unless you set one.

## Using the UI

After connect you get terminals, an explorer, and editor tabs in one shell:

| Do this | How |
|---------|-----|
| New terminal | `Mod+T` or **+** |
| Split terminal | `Mod+D` / `Mod+Shift+D` |
| Open a file | Click or double-click in the tree |
| Save | `Mod+S` |
| Find | `Mod+F` |
| Copy selected terminal text | Select with the mouse, then `Mod+C` (no selection → interrupt) |
| Paste into terminal | `Mod+V` |
| Command palette | `Mod+P` |
| Settings | Activity bar gear or `Mod+,` (opens `config.json`) |
| Copy a path | Right-click a tab or tree row |

`Mod` is `Ctrl` on Linux/Windows and `Cmd` on macOS. Disconnect leaves remote sessions and PTYs running so you can reconnect.

Sessions keep shells alive across GUI disconnect. File listing and editor open are sandboxed to `--root` (default: current directory).

## Settings

All prefs live in one JSONC file on the **backend** host:

`~/.config/fresh-gui/config.json`  
(or `$XDG_CONFIG_HOME/fresh-gui/config.json`, or `--config` / `FRESH_GUI_CONFIG`)

```jsonc
{
  "ui": {
    "theme": "system", // system | light | dark
    "terminalFontSize": 14,
    "editorFontSize": 14,
    "webgl": true,
    "showDotfiles": false, // show .* names in the explorer
    "showGitDirs": false // show .git folders (separate from showDotfiles)
  },
  "terminal": {
    "shell": { "command": "zsh", "args": [] }
  }
}
```

Open it from the UI (**Settings** / `Mod+,`), edit, save with `Mod+S`. Theme follows the OS by default; terminal chrome tracks the same theme. Empty shell `args` keep interactive / OSC 7 setup for known shells. Dotfiles and `.git` directories are hidden in the explorer by default; enable them independently via `showDotfiles` / `showGitDirs`.

## Windows host app

A Tauri desktop wrapper can load the same UI. Build installers on Windows (or via CI):

```powershell
cd crates\fresh-gui-desktop
npm ci
npm run build:windows
```

Details: [docs/WINDOWS.md](./docs/WINDOWS.md).

## Develop from source

```bash
git clone https://github.com/amirhosseindavoody/fresh-gui.git
cd fresh-gui
git submodule update --init --recursive
pixi install
pixi run ui-install   # once

pixi run serve        # build UI + start backend (prints UI / WS URLs)
```

Useful tasks: `pixi run check`, `test`, `build`, `ui` (Vite hot reload on `:1420`), `package` (write `.conda` under `./dist`).

| Piece | Role |
|-------|------|
| `fresh-gui-backend` | Linux daemon (PTY, FS, Fresh editor, embedded UI) |
| `fresh-gui-app` / `ui/` | Browser UI + small CLI |
| `fresh-gui-desktop` | Windows Tauri host |
| `fresh-gui-protocol` / `fresh-gui-client` | Shared wire format + client library |

Deeper design notes: [docs/DESIGN.md](./docs/DESIGN.md), [docs/UI.md](./docs/UI.md). Backend flags and packaging: [crates/fresh-gui-backend/README.md](./crates/fresh-gui-backend/README.md).

## Releases

CalVer `YYYY.MMDD.N`. Pushes to `main` bump the version, build the linux-64 package, and publish a [GitHub Release](https://github.com/amirhosseindavoody/fresh-gui/releases) (see `.github/workflows/release-backend.yml`). The version-bump commit rebases if `main` moved during the build. Manual bump: `pixi run update-version`.

## License

[GPL-2.0](./LICENSE) (same as Fresh).
