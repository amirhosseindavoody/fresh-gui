//! Host asset source: the default gpui-kit icon bundle plus explorer file types.
//!
//! [`icon_assets!`](gpui_kit::assets::icon_assets) embeds only the Lucide glyphs
//! the explorer references. Everything else (chevrons, tab chrome, the activity
//! rail) still comes from [`gpui_kit::assets::Assets`].

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

gpui_kit::assets::icon_assets!(
    FileTypeIcons,
    [
        File,
        FileArchive,
        FileBraces,
        FileCode,
        FileCog,
        FileDiff,
        FileImage,
        FileLock,
        FileSpreadsheet,
        FileSymlink,
        FileTerminal,
        FileText,
        Folder,
        FolderCode,
        FolderCog,
        FolderGit,
        FolderOpen,
    ]
);

/// Default kit icons, then the explorer file-type set.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostAssets;

impl AssetSource for HostAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = FileTypeIcons.load(path)? {
            return Ok(Some(bytes));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(FileTypeIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}
