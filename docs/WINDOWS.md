# Windows host packaging (Tauri)

The Windows MVP host is `fresh-gui-desktop` (Tauri 2). Installers are **NSIS** (`.exe`) and **MSI** (WiX).

## Local build (on Windows)

Prerequisites: [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#windows) (WebView2, VS Build Tools, Rust).

```powershell
cd crates\fresh-gui-desktop
npm ci
npm run build:windows
```

Artifacts land under the workspace target directory:

`target/release/bundle/nsis/`  
`target/release/bundle/msi/`

Note: the Rust package lives at `crates/fresh-gui-desktop`; `tauri.conf.json` sets `frontendDist` to `../fresh-gui-app/ui`.

## CI

GitHub Actions workflow [`.github/workflows/windows-tauri.yml`](../.github/workflows/windows-tauri.yml) builds NSIS + MSI on `windows-latest` and uploads artifacts.

## Scope note

Full polish (code signing, auto-update) remains Phase 4. This packaging path produces unsigned installers suitable for MVP distribution and testing.
