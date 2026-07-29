# Windows host packaging (Tauri)

The Windows MVP host is `fresh-gui-desktop` (Tauri 2). The installer is **NSIS** (`.exe`).

## Why not MSI?

WiX/MSI requires each of major/minor ≤ **255**. Our CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`) exceeds that (`2026` and often `MMDD`). NSIS accepts CalVer filenames and is the supported Windows channel for now. MSI can return later with a mapped SemVer if needed.

## Local build (on Windows)

Prerequisites: [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#windows) (WebView2, VS Build Tools, Rust).

```powershell
cd crates\fresh-gui-desktop
npm ci
npm run build:windows
```

Artifact:

`target/release/bundle/nsis/fresh-gui_*_x64-setup.exe`

(Paths are under the **repo root** `target\`, not under the crate.)

Note: the Rust package lives at `crates/fresh-gui-desktop`; `tauri.conf.json` sets `frontendDist` to `../fresh-gui-app/ui`.

## CI

GitHub Actions workflow [`.github/workflows/windows-tauri.yml`](../.github/workflows/windows-tauri.yml) builds the NSIS installer on `windows-latest` and uploads it as an artifact.

## Scope note

Full polish (code signing, auto-update) remains Phase 4. This packaging path produces an unsigned NSIS installer suitable for MVP distribution and testing.
