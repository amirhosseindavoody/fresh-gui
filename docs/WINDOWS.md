# Windows host packaging (Tauri)

The Windows host is `fresh-gui-desktop` (Tauri 2). Installers are **NSIS** (`.exe`) and **MSI** (WiX). Builds are **unsigned**; code signing and auto-update are not part of the current packaging path.

## Version mapping (CalVer → WiX)

Cargo / pixi keep CalVer `YYYY.MMDD.N` (e.g. `2026.728.1`).

`MMDD` is `month*100+day` with **no leading zeros** (3 digits for Jan–Sep, 4 for Oct–Dec). The bump script rejects padded/malformed values (e.g. `2026.0728.1`, `2026.100.1`, `2026.132.1`).

WiX `ProductVersion` requires major/minor ≤ **255** and build ≤ **65535**, so the Tauri bundle version in `tauri.conf.json` is a mapped form:

| CalVer `YYYY.MMDD.N` | Bundle / MSI version |
|----------------------|----------------------|
| `YYYY`               | `YYYY - 2000` (major) |
| `MMDD / 100`         | month (minor) |
| `MMDD % 100`, `N`    | `day * 1000 + N` (build) |

Example: `2026.728.1` → **`26.7.28001`**.

`scripts/update-version.sh` updates Cargo/pixi CalVer and rewrites this mapped version into `crates/fresh-gui-desktop/tauri.conf.json` / `package.json`, and the UI `crates/fresh-gui-app/ui/package.json`. Tauri builds the Vite UI (`ui/dist`) via `beforeBuildCommand` (Bun).

## Local build (on Windows)

Prerequisites: [Tauri Windows prerequisites](https://v2.tauri.app/start/prerequisites/#windows) (WebView2, VS Build Tools, Rust), plus [Bun](https://bun.sh) on `PATH` for the UI build.

```powershell
cd crates\fresh-gui-app\ui
bun install --frozen-lockfile
cd ..\..\fresh-gui-desktop
npm ci
npm run build:windows
```

Artifacts (under the **repo root** `target\`):

- `target/release/bundle/nsis/fresh-gui_*_x64-setup.exe`
- `target/release/bundle/msi/fresh-gui_*_x64_en-US.msi`

## CI

GitHub Actions workflow [`.github/workflows/windows-tauri.yml`](../.github/workflows/windows-tauri.yml) builds NSIS + MSI on `windows-latest` and uploads artifacts.
