# Changelog

## 2026-07-30

- Publish standalone release binaries (linux-gnu glibc ≥ 2.31, linux-musl, windows-msvc) alongside the linux-64 `.conda` package via `scripts/package-binary.sh` and the release workflow.
- Split the daemon session module into unix/windows backends (Fresh-aligned detach/spawn) so Windows binaries build and run; document Windows session/config paths.
