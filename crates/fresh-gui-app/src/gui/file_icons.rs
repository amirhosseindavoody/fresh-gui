//! Lucide file-type glyphs for the explorer.
//!
//! gpui-component's default icon bundle only has a generic file and folder.
//! These names are the Lucide `file-*` / `folder-*` set embedded by
//! [`super::assets::HostAssets`], tinted so a folder, a Rust file, and a
//! markdown file are distinct the way a VS Code icon theme is.

use fresh_gui_protocol::FsKind;
use gpui::{Hsla, hsla};
use gpui_kit::assets::IconName;

/// One explorer glyph: a bundled Lucide icon and an HSL tint. `hue` is in
/// degrees (0–360); GPUI's `hsla` wants 0–1, so use [`Self::color`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExplorerGlyph {
    pub icon: IconName,
    pub hue: f32,
    pub saturation: f32,
    pub lightness: f32,
}

impl ExplorerGlyph {
    /// Tints are tuned for a dark sidebar. A light sidebar needs them darker,
    /// or grey and pastel glyphs wash out against the background.
    pub fn color(&self, dark: bool) -> Hsla {
        let lightness = if dark {
            self.lightness
        } else {
            (self.lightness - 0.2).max(0.25)
        };
        hsla(self.hue / 360.0, self.saturation, lightness, 1.0)
    }
}

/// Icon for `name` (a single path segment). Directories use folder glyphs;
/// an expanded directory is the open folder so the click reads as a toggle.
pub fn explorer_glyph(name: &str, kind: FsKind, expanded: bool) -> ExplorerGlyph {
    match kind {
        FsKind::Dir => folder_glyph(name, expanded),
        FsKind::Symlink => glyph(IconName::FileSymlink, 200.0, 0.35, 0.62),
        FsKind::File | FsKind::Other => file_glyph(name),
    }
}

fn folder_glyph(name: &str, expanded: bool) -> ExplorerGlyph {
    if expanded {
        return glyph(IconName::FolderOpen, 40.0, 0.8, 0.58);
    }
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        ".git" | ".github" | ".gitlab" => glyph(IconName::FolderGit, 16.0, 0.7, 0.52),
        "src" | "crates" | "lib" | "include" | "app" => {
            glyph(IconName::FolderCode, 211.0, 0.45, 0.55)
        }
        "node_modules" | "target" | "dist" | "build" | "out" | "vendor" => {
            glyph(IconName::FolderCog, 210.0, 0.12, 0.58)
        }
        _ => glyph(IconName::Folder, 38.0, 0.75, 0.52),
    }
}

fn file_glyph(name: &str) -> ExplorerGlyph {
    let name = name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "dockerfile" | "containerfile" | "makefile" | "gnumakefile" | "justfile" | "rakefile"
    ) || name.starts_with("dockerfile.")
    {
        return glyph(IconName::FileTerminal, 142.0, 0.55, 0.45);
    }
    if matches!(
        name.as_str(),
        ".gitignore" | ".gitattributes" | ".gitmodules" | ".editorconfig"
    ) {
        return glyph(IconName::FileDiff, 95.0, 0.4, 0.48);
    }
    if matches!(
        name.as_str(),
        "cargo.toml" | "package.json" | "pyproject.toml" | "composer.json" | "go.mod"
    ) {
        return glyph(IconName::FileCog, 210.0, 0.15, 0.62);
    }
    if is_prose_name(&name) {
        return glyph(IconName::FileText, 210.0, 0.25, 0.62);
    }

    match extension(&name) {
        Some("rs") => glyph(IconName::FileCode, 24.0, 0.72, 0.55),
        Some("py" | "pyi") => glyph(IconName::FileCode, 207.0, 0.7, 0.55),
        Some("go") => glyph(IconName::FileCode, 187.0, 0.65, 0.48),
        Some("js" | "jsx" | "mjs" | "cjs") => glyph(IconName::FileCode, 48.0, 0.85, 0.52),
        Some("ts" | "tsx" | "mts" | "cts") => glyph(IconName::FileCode, 211.0, 0.75, 0.56),
        Some("html" | "htm" | "vue" | "svelte") => glyph(IconName::FileCode, 18.0, 0.75, 0.52),
        Some("css" | "scss" | "sass" | "less") => glyph(IconName::FileCode, 200.0, 0.7, 0.55),
        Some(
            "c" | "h" | "cpp" | "cc" | "cxx" | "hpp" | "hh" | "java" | "kt" | "kts" | "swift"
            | "rb" | "php" | "cs" | "scala" | "lua" | "zig" | "dart" | "xml",
        ) => glyph(IconName::FileCode, 211.0, 0.55, 0.55),
        Some("json" | "jsonc" | "json5") => glyph(IconName::FileBraces, 45.0, 0.75, 0.52),
        Some("toml" | "yaml" | "yml" | "ini" | "cfg" | "conf" | "properties") => {
            glyph(IconName::FileCog, 210.0, 0.15, 0.62)
        }
        Some("md" | "markdown" | "txt" | "rst" | "adoc" | "org") => {
            glyph(IconName::FileText, 210.0, 0.25, 0.62)
        }
        Some("png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "ico" | "bmp" | "avif") => {
            glyph(IconName::FileImage, 280.0, 0.55, 0.62)
        }
        Some("sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd") => {
            glyph(IconName::FileTerminal, 142.0, 0.55, 0.45)
        }
        Some("zip" | "gz" | "tgz" | "tar" | "bz2" | "xz" | "7z" | "rar" | "jar" | "war") => {
            glyph(IconName::FileArchive, 28.0, 0.45, 0.5)
        }
        Some("lock") => glyph(IconName::FileLock, 45.0, 0.6, 0.5),
        Some("diff" | "patch") => glyph(IconName::FileDiff, 95.0, 0.4, 0.48),
        Some("csv" | "tsv" | "xls" | "xlsx") => glyph(IconName::FileSpreadsheet, 142.0, 0.45, 0.42),
        _ => glyph(IconName::File, 220.0, 0.08, 0.62),
    }
}

fn is_prose_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    matches!(
        stem,
        "readme" | "license" | "licence" | "copying" | "changelog" | "authors"
    )
}

fn extension(name: &str) -> Option<&str> {
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() || ext == name {
        None
    } else {
        Some(ext)
    }
}

fn glyph(icon: IconName, hue: f32, saturation: f32, lightness: f32) -> ExplorerGlyph {
    ExplorerGlyph {
        icon,
        hue,
        saturation,
        lightness,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_use_distinct_glyphs() {
        assert_eq!(
            explorer_glyph("main.rs", FsKind::File, false).icon,
            IconName::FileCode
        );
        assert_eq!(explorer_glyph("main.rs", FsKind::File, false).hue, 24.0);
        assert_eq!(
            explorer_glyph("pkg.json", FsKind::File, false).icon,
            IconName::FileBraces
        );
        assert_eq!(
            explorer_glyph("notes.md", FsKind::File, false).icon,
            IconName::FileText
        );
        assert_eq!(
            explorer_glyph("pic.PNG", FsKind::File, false).icon,
            IconName::FileImage
        );
        assert_eq!(
            explorer_glyph("run.sh", FsKind::File, false).icon,
            IconName::FileTerminal
        );
        assert_eq!(
            explorer_glyph("Cargo.toml", FsKind::File, false).icon,
            IconName::FileCog
        );
        assert_eq!(
            explorer_glyph("mystery.xyz", FsKind::File, false).icon,
            IconName::File
        );
    }

    #[test]
    fn tints_keep_their_hue_and_darken_on_light_themes() {
        let rust = explorer_glyph("main.rs", FsKind::File, false);
        let ts = explorer_glyph("app.ts", FsKind::File, false);
        assert!((rust.color(true).h - 24.0 / 360.0).abs() < 1e-6);
        assert_ne!(rust.color(true).h, ts.color(true).h);
        assert!(rust.color(false).l < rust.color(true).l);
    }

    #[test]
    fn folders_stay_folders_and_open_when_expanded() {
        let closed = explorer_glyph("docs", FsKind::Dir, false);
        let open = explorer_glyph("docs", FsKind::Dir, true);
        assert_eq!(closed.icon, IconName::Folder);
        assert_eq!(open.icon, IconName::FolderOpen);
        assert_ne!(
            closed.icon,
            explorer_glyph("docs.md", FsKind::File, false).icon
        );
        assert_eq!(
            explorer_glyph(".git", FsKind::Dir, false).icon,
            IconName::FolderGit
        );
        assert_eq!(
            explorer_glyph("src", FsKind::Dir, false).icon,
            IconName::FolderCode
        );
        assert_eq!(
            explorer_glyph("src", FsKind::Dir, true).icon,
            IconName::FolderOpen
        );
    }
}
