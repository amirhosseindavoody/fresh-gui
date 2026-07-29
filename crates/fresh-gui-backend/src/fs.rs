//! Read-only filesystem listing under a sandboxed root (+ Terax-style authorized cwds).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use fresh_gui_protocol::{FsEntry, FsKind};
use tokio::fs;

#[derive(Debug, Clone)]
pub struct FsRoot {
    root: PathBuf,
    /// Extra directories the host may list/open after `fs_authorize` (terminal cwd sync).
    authorized: Arc<Mutex<Vec<PathBuf>>>,
}

impl FsRoot {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("canonicalize FS root {}", root.display()))?;
        if !root.is_dir() {
            bail!("FS root is not a directory: {}", root.display());
        }
        Ok(Self {
            root,
            authorized: Arc::new(Mutex::new(Vec::new())),
        })
    }

    pub fn root_display(&self) -> String {
        self.root.display().to_string()
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }

    fn is_allowed(&self, canon: &Path) -> bool {
        if canon.starts_with(&self.root) {
            return true;
        }
        let guard = self.authorized.lock().expect("fs authorized lock");
        guard.iter().any(|p| canon.starts_with(p))
    }

    /// Terax-style: allow listing/opening under `path` (absolute directory) for this process.
    /// Safe relative to PTY access — the shell can already read these paths.
    pub async fn authorize(&self, path: &str) -> Result<PathBuf> {
        if path.is_empty() || path == "." || path == "/" {
            return Ok(self.root.clone());
        }
        let candidate = Path::new(path);
        if !candidate.is_absolute() {
            bail!("fs_authorize requires an absolute path, got {path}");
        }
        let canon = fs::canonicalize(candidate)
            .await
            .with_context(|| format!("canonicalize {}", candidate.display()))?;
        let meta = fs::metadata(&canon).await?;
        if !meta.is_dir() {
            bail!("not a directory: {}", canon.display());
        }
        if self.is_allowed(&canon) {
            return Ok(canon);
        }
        {
            let mut guard = self.authorized.lock().expect("fs authorized lock");
            if !guard.iter().any(|p| p == &canon) {
                guard.push(canon.clone());
            }
        }
        Ok(canon)
    }

    /// Resolve a client path to an absolute path inside the root or an authorized cwd.
    /// Empty, `.`, or `/` → primary root. Absolute paths may be under root or authorized dirs.
    pub async fn resolve(&self, path: &str) -> Result<PathBuf> {
        let candidate = if path.is_empty() || path == "." || path == "/" {
            self.root.clone()
        } else {
            let p = Path::new(path);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                self.root.join(p)
            }
        };

        let canon = fs::canonicalize(&candidate)
            .await
            .with_context(|| format!("canonicalize {}", candidate.display()))?;

        if !self.is_allowed(&canon) {
            bail!("path escapes FS root: {}", canon.display());
        }
        Ok(canon)
    }

    pub async fn list(&self, path: &str) -> Result<(String, Vec<FsEntry>)> {
        let dir = self.resolve(path).await?;
        let meta = fs::metadata(&dir).await?;
        if !meta.is_dir() {
            bail!("not a directory: {}", dir.display());
        }

        let mut entries = Vec::new();
        let mut rd = fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name().to_string_lossy().into_owned();
            let path = ent.path();
            let meta = match ent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let kind = if meta.is_dir() {
                FsKind::Dir
            } else if meta.is_symlink() {
                FsKind::Symlink
            } else if meta.is_file() {
                FsKind::File
            } else {
                FsKind::Other
            };
            let size = if meta.is_file() {
                Some(meta.len())
            } else {
                None
            };
            entries.push(FsEntry {
                name,
                path: path.display().to_string(),
                kind,
                size,
            });
        }

        entries.sort_by(|a, b| {
            let dir_cmp = (a.kind != FsKind::Dir).cmp(&(b.kind != FsKind::Dir));
            dir_cmp.then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        Ok((dir.display().to_string(), entries))
    }

    pub async fn stat(&self, path: &str) -> Result<FsEntry> {
        let path = self.resolve(path).await?;
        let meta = fs::symlink_metadata(&path).await?;
        let kind = if meta.is_dir() {
            FsKind::Dir
        } else if meta.is_symlink() {
            FsKind::Symlink
        } else if meta.is_file() {
            FsKind::File
        } else {
            FsKind::Other
        };
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Ok(FsEntry {
            name,
            path: path.display().to_string(),
            kind,
            size: if meta.is_file() {
                Some(meta.len())
            } else {
                None
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs as stdfs;

    #[tokio::test]
    async fn list_and_block_escape() {
        let tmp = tempfile_dir();
        stdfs::write(tmp.join("a.txt"), b"hi").unwrap();
        stdfs::create_dir(tmp.join("sub")).unwrap();

        let root = FsRoot::new(tmp.clone()).unwrap();
        let (path, entries) = root.list("").await.unwrap();
        assert!(
            path.contains(tmp.file_name().unwrap().to_str().unwrap())
                || path == tmp.display().to_string()
                || entries.len() >= 2
        );
        assert!(entries.iter().any(|e| e.name == "a.txt"));
        assert!(entries.iter().any(|e| e.name == "sub" && e.kind == FsKind::Dir));

        let escape = root.resolve("../").await;
        assert!(escape.is_err());
    }

    #[tokio::test]
    async fn authorize_outside_root_allows_list() {
        let a = tempfile_dir();
        let b = tempfile_dir();
        stdfs::write(b.join("out.txt"), b"x").unwrap();

        let root = FsRoot::new(a).unwrap();
        assert!(root.list(&b.display().to_string()).await.is_err());
        let auth = root.authorize(&b.display().to_string()).await.unwrap();
        assert_eq!(auth, b.canonicalize().unwrap());
        let (_path, entries) = root.list(&b.display().to_string()).await.unwrap();
        assert!(entries.iter().any(|e| e.name == "out.txt"));
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fresh-gui-fs-{}", uuid::Uuid::new_v4()));
        stdfs::create_dir_all(&dir).unwrap();
        dir
    }
}
