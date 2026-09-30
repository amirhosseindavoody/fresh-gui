//! Local filesystem adapter for the Fresh editor.
//!
//! Fresh's default `write_patched` implementation flattens the whole output
//! into a `Vec`. This adapter preserves Fresh's piece-tree recipe while
//! streaming copy operations into a same-directory temporary file.

use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use fresh::model::filesystem::{
    DirEntry, FileMetadata, FilePermissions, FileReader, FileSearchCursor, FileSearchOptions,
    FileSystem, FileWriter, SearchMatch, StdFileSystem, WalkEntry, WalkOptions, WriteOp,
};

pub(crate) struct EditorFileSystem {
    inner: StdFileSystem,
}

impl EditorFileSystem {
    pub(crate) fn new() -> Self {
        Self {
            inner: StdFileSystem,
        }
    }

    fn patch_temp_path(dst_path: &Path) -> PathBuf {
        let parent = dst_path.parent().unwrap_or_else(|| Path::new("."));
        let name = dst_path
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("fresh-save"))
            .to_string_lossy();
        parent.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4()))
    }
}

impl FileSystem for EditorFileSystem {
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.read_file(path)
    }

    fn read_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.inner.read_range(path, offset, len)
    }

    fn count_line_feeds_in_range(&self, path: &Path, offset: u64, len: usize) -> io::Result<usize> {
        self.inner.count_line_feeds_in_range(path, offset, len)
    }

    fn write_file(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        self.inner.write_file(path, data)
    }

    fn create_file(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.create_file(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FileReader>> {
        self.inner.open_file(path)
    }

    fn open_file_for_write(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.open_file_for_write(path)
    }

    fn open_file_for_append(&self, path: &Path) -> io::Result<Box<dyn FileWriter>> {
        self.inner.open_file_for_append(path)
    }

    fn set_file_length(&self, path: &Path, len: u64) -> io::Result<()> {
        self.inner.set_file_length(path, len)
    }

    fn write_patched(
        &self,
        src_path: &Path,
        dst_path: &Path,
        ops: &[WriteOp<'_>],
    ) -> io::Result<()> {
        let original_metadata = self.inner.metadata_if_exists(dst_path);
        let temp_path = Self::patch_temp_path(dst_path);
        let result = (|| {
            let mut source = if ops.iter().any(|op| matches!(op, WriteOp::Copy { .. })) {
                Some(self.inner.open_file(src_path)?)
            } else {
                None
            };
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            let mut chunk = [0u8; 64 * 1024];
            for op in ops {
                match op {
                    WriteOp::Copy { offset, len } => {
                        let reader = source.as_mut().expect("copy recipe opened source");
                        reader.seek(SeekFrom::Start(*offset))?;
                        let mut remaining = *len;
                        while remaining > 0 {
                            let requested = usize::try_from(remaining.min(chunk.len() as u64))
                                .expect("chunk length fits usize");
                            let count = reader.read(&mut chunk[..requested])?;
                            if count == 0 {
                                return Err(io::Error::new(
                                    io::ErrorKind::UnexpectedEof,
                                    "source file was truncated during patched save",
                                ));
                            }
                            output.write_all(&chunk[..count])?;
                            remaining -= count as u64;
                        }
                    }
                    WriteOp::Insert { data } => output.write_all(data)?,
                }
            }
            if let Some(metadata) = original_metadata.as_ref() {
                if let Some(permissions) = metadata.permissions.as_ref() {
                    self.inner.set_permissions(&temp_path, permissions)?;
                }
            }
            output.sync_all()?;
            drop(output);
            self.inner.rename(&temp_path, dst_path)
        })();
        if result.is_err() {
            let _ = self.inner.remove_file(&temp_path);
        }
        result.map_err(|error| {
            if error.kind() == io::ErrorKind::PermissionDenied {
                // Fresh's pinned save_to_file retries PermissionDenied by
                // flattening the recipe into a full Vec for sudo. Keep the
                // paged save bounded and report the underlying cause instead.
                io::Error::other(format!(
                    "streaming patched save failed (sudo fallback is unavailable): {error}"
                ))
            } else {
                error
            }
        })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.inner.rename(from, to)
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<u64> {
        self.inner.copy(from, to)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_file(path)
    }

    fn remove_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.remove_dir(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.metadata(path)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<FileMetadata> {
        self.inner.symlink_metadata(path)
    }

    fn is_dir(&self, path: &Path) -> io::Result<bool> {
        self.inner.is_dir(path)
    }

    fn is_file(&self, path: &Path) -> io::Result<bool> {
        self.inner.is_file(path)
    }

    fn is_writable(&self, path: &Path) -> bool {
        self.inner.is_writable(path)
    }

    fn set_permissions(&self, path: &Path, permissions: &FilePermissions) -> io::Result<()> {
        self.inner.set_permissions(path, permissions)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }

    fn create_dir(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir(path)
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        self.inner.create_dir_all(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }

    fn current_uid(&self) -> u32 {
        self.inner.current_uid()
    }

    fn search_file(
        &self,
        path: &Path,
        pattern: &str,
        opts: &FileSearchOptions,
        cursor: &mut FileSearchCursor,
    ) -> io::Result<Vec<SearchMatch>> {
        self.inner.search_file(path, pattern, opts, cursor)
    }

    fn sudo_write(
        &self,
        path: &Path,
        data: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> io::Result<()> {
        self.inner.sudo_write(path, data, mode, uid, gid)
    }

    fn walk(
        &self,
        root: &Path,
        opts: &WalkOptions<'_>,
        cancel: &AtomicBool,
        on_entry: &mut dyn FnMut(WalkEntry<'_>) -> bool,
    ) -> io::Result<()> {
        self.inner.walk(root, opts, cancel, on_entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("editor-fs-{name}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn patched_write_streams_same_path_and_different_destination() {
        let root = temp_dir("patch");
        let source = root.join("source.txt");
        let destination = root.join("copy.txt");
        std::fs::write(&source, vec![b'a'; 2 * 1024 * 1024]).unwrap();
        let fs = EditorFileSystem::new();
        let ops = [
            WriteOp::Copy {
                offset: 0,
                len: 1024 * 1024,
            },
            WriteOp::Insert { data: b"insert" },
            WriteOp::Copy {
                offset: 1024 * 1024,
                len: 1024 * 1024,
            },
        ];
        fs.write_patched(&source, &source, &ops).unwrap();
        let actual = std::fs::read(&source).unwrap();
        assert_eq!(actual.len(), 2 * 1024 * 1024 + 6);
        assert_eq!(&actual[1024 * 1024..1024 * 1024 + 6], b"insert");

        let copy_ops = [WriteOp::Copy {
            offset: 0,
            len: actual.len() as u64,
        }];
        fs.write_patched(&source, &destination, &copy_ops).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), actual);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn truncated_source_preserves_destination_and_cleans_temporary_file() {
        let root = temp_dir("truncate");
        let source = root.join("source.txt");
        let destination = root.join("dest.txt");
        std::fs::write(&source, b"short").unwrap();
        std::fs::write(&destination, b"old destination").unwrap();
        let fs = EditorFileSystem::new();
        let ops = [WriteOp::Copy {
            offset: 0,
            len: 1024,
        }];
        let error = fs.write_patched(&source, &destination, &ops).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(std::fs::read(&destination).unwrap(), b"old destination");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn patched_write_preserves_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("permissions");
        let path = root.join("source.txt");
        std::fs::write(&path, b"before").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let fs = EditorFileSystem::new();
        let ops = [WriteOp::Insert { data: b"after" }];
        fs.write_patched(&path, &path, &ops).unwrap();
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        assert_eq!(permissions.mode() & 0o777, 0o640);
        assert_eq!(std::fs::read(&path).unwrap(), b"after");
        let _ = std::fs::remove_dir_all(root);
    }
}
