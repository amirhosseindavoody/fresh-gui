//! Filesystem listing and mutation under a sandboxed root (+ Terax-style authorized cwds).

use std::path::{Component, Path, PathBuf};
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
        entry_for_path(&path).await
    }

    /// Create an empty file or directory named `name` under `parent`.
    pub async fn create(&self, parent: &str, name: &str, kind: FsKind) -> Result<FsEntry> {
        validate_entry_name(name)?;
        match kind {
            FsKind::File | FsKind::Dir => {}
            _ => bail!("fs_create kind must be file or dir"),
        }
        let parent_dir = self.resolve(parent).await?;
        let meta = fs::metadata(&parent_dir).await?;
        if !meta.is_dir() {
            bail!("not a directory: {}", parent_dir.display());
        }
        let target = parent_dir.join(name);
        ensure_child_of(&parent_dir, &target)?;
        if fs::symlink_metadata(&target).await.is_ok() {
            bail!("already exists: {}", target.display());
        }
        match kind {
            FsKind::Dir => {
                fs::create_dir(&target)
                    .await
                    .with_context(|| format!("create_dir {}", target.display()))?;
            }
            FsKind::File => {
                fs::File::create(&target)
                    .await
                    .with_context(|| format!("create file {}", target.display()))?;
            }
            _ => unreachable!(),
        }
        entry_for_path(&target).await
    }

    /// Copy each source into `destination` (must be a directory). Name conflicts get a unique suffix.
    pub async fn copy_into(&self, sources: &[String], destination: &str) -> Result<Vec<FsEntry>> {
        if sources.is_empty() {
            bail!("fs_copy requires at least one source");
        }
        let dest_dir = self.resolve(destination).await?;
        let meta = fs::metadata(&dest_dir).await?;
        if !meta.is_dir() {
            bail!("destination is not a directory: {}", dest_dir.display());
        }
        let mut out = Vec::with_capacity(sources.len());
        for src in sources {
            let from = self.resolve(src).await?;
            if paths_equal(&from, &dest_dir) || is_descendant(&from, &dest_dir) {
                bail!(
                    "cannot copy {} into itself or a descendant",
                    from.display()
                );
            }
            let base = from
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| anyhow::anyhow!("source has no file name: {}", from.display()))?;
            let to = unique_dest_path(&dest_dir, &base).await?;
            ensure_child_of(&dest_dir, &to)?;
            copy_path_recursive(&from, &to).await?;
            out.push(entry_for_path(&to).await?);
        }
        Ok(out)
    }

    /// Move each source into `destination` (must be a directory).
    pub async fn move_into(&self, sources: &[String], destination: &str) -> Result<Vec<FsEntry>> {
        if sources.is_empty() {
            bail!("fs_move requires at least one source");
        }
        let dest_dir = self.resolve(destination).await?;
        let meta = fs::metadata(&dest_dir).await?;
        if !meta.is_dir() {
            bail!("destination is not a directory: {}", dest_dir.display());
        }
        let mut out = Vec::with_capacity(sources.len());
        for src in sources {
            let from = self.resolve(src).await?;
            if paths_equal(&from, &dest_dir) || is_descendant(&from, &dest_dir) {
                bail!(
                    "cannot move {} into itself or a descendant",
                    from.display()
                );
            }
            let base = from
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .ok_or_else(|| anyhow::anyhow!("source has no file name: {}", from.display()))?;
            let to = unique_dest_path(&dest_dir, &base).await?;
            ensure_child_of(&dest_dir, &to)?;
            if let Some(parent) = from.parent() {
                if paths_equal(parent, &dest_dir) && paths_equal(&from, &to) {
                    out.push(entry_for_path(&from).await?);
                    continue;
                }
            }
            fs::rename(&from, &to)
                .await
                .with_context(|| format!("rename {} → {}", from.display(), to.display()))?;
            out.push(entry_for_path(&to).await?);
        }
        Ok(out)
    }

    /// Permanently delete each path (file, directory, or symlink). Refuses the
    /// primary FS root and any authorized cwd root.
    pub async fn delete_paths(&self, paths: &[String]) -> Result<Vec<String>> {
        if paths.is_empty() {
            bail!("fs_delete requires at least one path");
        }

        let mut targets = Vec::with_capacity(paths.len());
        for path in paths {
            if path.is_empty() || path == "." || path == "/" {
                bail!("cannot delete filesystem root");
            }
            let resolved = self.resolve(path).await?;
            if paths_equal(&resolved, &self.root) {
                bail!("cannot delete filesystem root: {}", resolved.display());
            }
            {
                let auth = self.authorized.lock().expect("fs authorized lock");
                for a in auth.iter() {
                    if paths_equal(&resolved, a) {
                        bail!("cannot delete authorized root: {}", resolved.display());
                    }
                }
            }
            targets.push(resolved);
        }

        let mut deleted = Vec::with_capacity(targets.len());
        for target in targets {
            let meta = fs::symlink_metadata(&target)
                .await
                .with_context(|| format!("stat {}", target.display()))?;
            if meta.file_type().is_symlink() || meta.is_file() {
                fs::remove_file(&target)
                    .await
                    .with_context(|| format!("remove_file {}", target.display()))?;
            } else if meta.is_dir() {
                fs::remove_dir_all(&target)
                    .await
                    .with_context(|| format!("remove_dir_all {}", target.display()))?;
            } else {
                bail!("cannot delete special file: {}", target.display());
            }
            deleted.push(target.display().to_string());
        }
        Ok(deleted)
    }
}

fn validate_entry_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("name must not be empty");
    }
    if name == "." || name == ".." {
        bail!("invalid name: {name}");
    }
    if name.contains('\0') || name.contains('/') || name.contains('\\') {
        bail!("name must be a single path segment");
    }
    let path = Path::new(name);
    if path.components().count() != 1 {
        bail!("name must be a single path segment");
    }
    if let Some(Component::Normal(_)) = path.components().next() {
        Ok(())
    } else {
        bail!("invalid name: {name}");
    }
}

fn ensure_child_of(parent: &Path, child: &Path) -> Result<()> {
    if child.parent() == Some(parent) {
        return Ok(());
    }
    bail!(
        "resolved path escapes parent: {} (parent {})",
        child.display(),
        parent.display()
    );
}

fn paths_equal(a: &Path, b: &Path) -> bool {
    a == b
}

fn is_descendant(ancestor: &Path, path: &Path) -> bool {
    path.starts_with(ancestor) && path != ancestor
}

async fn entry_for_path(path: &Path) -> Result<FsEntry> {
    let meta = fs::symlink_metadata(path).await?;
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

async fn unique_dest_path(dest_dir: &Path, base_name: &str) -> Result<PathBuf> {
    let candidate = dest_dir.join(base_name);
    if fs::symlink_metadata(&candidate).await.is_err() {
        return Ok(candidate);
    }
    let (stem, ext) = split_name(base_name);
    for i in 1..10_000 {
        let name = if ext.is_empty() {
            if i == 1 {
                format!("{stem} copy")
            } else {
                format!("{stem} copy {i}")
            }
        } else if i == 1 {
            format!("{stem} copy.{ext}")
        } else {
            format!("{stem} copy {i}.{ext}")
        };
        let path = dest_dir.join(&name);
        if fs::symlink_metadata(&path).await.is_err() {
            return Ok(path);
        }
    }
    bail!("could not find a free name for {base_name} in {}", dest_dir.display());
}

fn split_name(name: &str) -> (String, String) {
    if name == "." || name.starts_with('.') && !name[1..].contains('.') {
        return (name.to_owned(), String::new());
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), ext.to_owned()),
        _ => (name.to_owned(), String::new()),
    }
}

async fn copy_path_recursive(from: &Path, to: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(from).await?;
    if meta.is_symlink() {
        let target = fs::read_link(from)
            .await
            .with_context(|| format!("read_link {}", from.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&target, to)
                .with_context(|| format!("symlink {} → {}", target.display(), to.display()))?;
        }
        #[cfg(not(unix))]
        {
            bail!("symlink copy is not supported on this platform");
        }
        return Ok(());
    }
    if meta.is_dir() {
        fs::create_dir(to)
            .await
            .with_context(|| format!("create_dir {}", to.display()))?;
        let mut rd = fs::read_dir(from).await?;
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name();
            let child_from = ent.path();
            let child_to = to.join(&name);
            Box::pin(copy_path_recursive(&child_from, &child_to)).await?;
        }
        return Ok(());
    }
    fs::copy(from, to)
        .await
        .with_context(|| format!("copy {} → {}", from.display(), to.display()))?;
    Ok(())
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

    #[tokio::test]
    async fn create_copy_move() {
        let tmp = tempfile_dir();
        let root = FsRoot::new(tmp.clone()).unwrap();

        let file = root
            .create("", "hello.txt", FsKind::File)
            .await
            .unwrap();
        assert_eq!(file.name, "hello.txt");
        assert_eq!(file.kind, FsKind::File);
        stdfs::write(tmp.join("hello.txt"), b"hi").unwrap();

        let dir = root.create("", "nested", FsKind::Dir).await.unwrap();
        assert_eq!(dir.kind, FsKind::Dir);

        let copied = root
            .copy_into(&[file.path.clone()], &dir.path)
            .await
            .unwrap();
        assert_eq!(copied.len(), 1);
        assert!(tmp.join("nested/hello.txt").is_file());
        assert_eq!(stdfs::read(tmp.join("nested/hello.txt")).unwrap(), b"hi");

        let moved = root
            .move_into(&[file.path.clone()], &dir.path)
            .await
            .unwrap();
        assert_eq!(moved.len(), 1);
        assert!(!tmp.join("hello.txt").exists());
        assert!(tmp.join("nested/hello copy.txt").is_file() || moved[0].name.contains("hello"));
    }

    #[tokio::test]
    async fn delete_file_and_dir() {
        let tmp = tempfile_dir();
        let root = FsRoot::new(tmp.clone()).unwrap();

        let file = root.create("", "bye.txt", FsKind::File).await.unwrap();
        stdfs::write(tmp.join("bye.txt"), b"x").unwrap();
        let dir = root.create("", "gone", FsKind::Dir).await.unwrap();
        stdfs::write(tmp.join("gone/a.txt"), b"y").unwrap();

        let deleted = root
            .delete_paths(&[file.path.clone(), dir.path.clone()])
            .await
            .unwrap();
        assert_eq!(deleted.len(), 2);
        assert!(!tmp.join("bye.txt").exists());
        assert!(!tmp.join("gone").exists());
    }

    #[tokio::test]
    async fn delete_refuses_root() {
        let tmp = tempfile_dir();
        let root = FsRoot::new(tmp.clone()).unwrap();
        assert!(root.delete_paths(&[tmp.display().to_string()]).await.is_err());
        assert!(root.delete_paths(&["".into()]).await.is_err());
    }

    #[tokio::test]
    async fn create_rejects_bad_names() {
        let tmp = tempfile_dir();
        let root = FsRoot::new(tmp).unwrap();
        assert!(root.create("", "../x", FsKind::File).await.is_err());
        assert!(root.create("", "a/b", FsKind::File).await.is_err());
        assert!(root.create("", "", FsKind::File).await.is_err());
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fresh-gui-fs-{}", uuid::Uuid::new_v4()));
        stdfs::create_dir_all(&dir).unwrap();
        dir
    }
}
