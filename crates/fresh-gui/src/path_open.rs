//! Resolve paths for editor open / Ctrl+click using Fresh detectors.
//!
//! Detection and `:line:col` parsing come from Fresh (`path_link`,
//! `parse_path_line_col`, `expand_tilde`). Candidate order matches Fresh
//! `terminal_link.rs`: absolute (after `~`), then terminal OSC 7 cwd, then
//! the FS sandbox root.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fresh::input::quick_open::parse_path_line_col;
use fresh::primitives::path_utils::expand_tilde;
use fresh::services::terminal::path_link::{detect_link_at, DetectedLink};

use crate::fs::FsRoot;

#[derive(Debug, Clone)]
pub struct ResolvedOpen {
    pub path: PathBuf,
    pub line: Option<u32>,
    pub column: Option<u32>,
}

/// Parse `path` (optional `:line:col`) and resolve against `cwd` / sandbox.
pub async fn resolve_path_open(
    fs_root: &FsRoot,
    path: &str,
    cwd: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> Result<ResolvedOpen> {
    let (path_part, parsed_line, parsed_col) = parse_path_line_col(path);
    if path_part.is_empty() {
        bail!("empty path");
    }
    let line = line.or(parsed_line.map(|n| n as u32));
    let column = column.or(parsed_col.map(|n| n as u32));
    let path = resolve_existing_file(fs_root, &path_part, cwd).await?;
    Ok(ResolvedOpen { path, line, column })
}

/// Detect a path link in `line_text` at `column` (Fresh `detect_link_at`), then resolve.
pub async fn resolve_link_open(
    fs_root: &FsRoot,
    line_text: &str,
    column: u32,
    cwd: Option<&str>,
) -> Result<ResolvedOpen> {
    let link: DetectedLink = detect_link_at(line_text, column as usize)
        .with_context(|| format!("no path link at column {column}"))?;
    let path = resolve_existing_file(fs_root, &link.path, cwd).await?;
    Ok(ResolvedOpen {
        path,
        line: link.line.map(|n| n as u32),
        column: link.column.map(|n| n as u32),
    })
}

async fn resolve_existing_file(
    fs_root: &FsRoot,
    raw: &str,
    cwd: Option<&str>,
) -> Result<PathBuf> {
    if let Some(cwd) = cwd.filter(|s| !s.is_empty()) {
        // Terax/Fresh: relative paths follow the terminal cwd, which may lie
        // outside `--root` — authorize it like explorer re-root does.
        let _ = fs_root.authorize(cwd).await;
    }

    let expanded = expand_tilde(raw);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if expanded.is_absolute() {
        candidates.push(expanded);
    } else {
        if let Some(cwd) = cwd.filter(|s| !s.is_empty()) {
            candidates.push(Path::new(cwd).join(&expanded));
        }
        candidates.push(fs_root.root_path().join(&expanded));
    }

    let mut last_err = None;
    for candidate in candidates {
        match allow_file(fs_root, &candidate).await {
            Ok(path) => return Ok(path),
            Err(err) => last_err = Some(err),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("file not found: {raw}")))
}

async fn allow_file(fs_root: &FsRoot, candidate: &Path) -> Result<PathBuf> {
    if !candidate.is_file() {
        bail!("not a file: {}", candidate.display());
    }
    let abs = candidate
        .canonicalize()
        .with_context(|| format!("canonicalize {}", candidate.display()))?;

    // Prefer normal sandbox resolve; if the file sits under an absolute parent
    // outside `--root`, authorize that parent (same affordance as fs_authorize).
    let as_str = abs.display().to_string();
    match fs_root.resolve(&as_str).await {
        Ok(p) => Ok(p),
        Err(_) => {
            if let Some(parent) = abs.parent() {
                let _ = fs_root.authorize(&parent.display().to_string()).await;
            }
            fs_root.resolve(&as_str).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn resolves_relative_under_cwd() {
        let tmp = std::env::temp_dir().join(format!(
            "fresh-gui-path-open-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src/main.rs"), b"fn main() {}\n").unwrap();
        let root = FsRoot::new(tmp.clone()).unwrap();

        let got = resolve_path_open(&root, "src/main.rs:2:1", Some(tmp.to_str().unwrap()), None, None)
            .await
            .unwrap();
        assert!(got.path.ends_with("src/main.rs"));
        assert_eq!(got.line, Some(2));
        assert_eq!(got.column, Some(1));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn detect_link_in_compiler_line() {
        let tmp = std::env::temp_dir().join(format!(
            "fresh-gui-path-link-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("src/lib.rs"), b"ok\n").unwrap();
        let root = FsRoot::new(tmp.clone()).unwrap();

        let line = "error: src/lib.rs:1:1: boom";
        let col = line.find("src").unwrap();
        let got = resolve_link_open(&root, line, col as u32, Some(tmp.to_str().unwrap()))
            .await
            .unwrap();
        assert!(got.path.ends_with("src/lib.rs"));
        assert_eq!(got.line, Some(1));
        assert_eq!(got.column, Some(1));

        let _ = fs::remove_dir_all(&tmp);
    }
}
