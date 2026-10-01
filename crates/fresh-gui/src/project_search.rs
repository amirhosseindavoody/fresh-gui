//! Daemon-side workspace search. All file enumeration and reads happen here,
//! on the machine that owns the workspace (including for SSH workspaces).

use std::{
    fs,
    io::Read as _,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::editor_worker::ProjectBufferSnapshot;
use fresh_gui_protocol::{ProjectSearchFile, ProjectSearchMatch, ProjectSearchRequest};
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::{
    Match, WalkBuilder,
    gitignore::{Gitignore, GitignoreBuilder},
};

const MAX_TOTAL_MATCHES: usize = 2_000;
const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_FILE_RESULT_BYTES: usize = 1024 * 1024;
const MAX_GLOBS: usize = 32;
const MAX_QUERY_BYTES: usize = 64 * 1024;
const MAX_GLOB_BYTES: usize = 4 * 1024;
const PREVIEW_RADIUS: usize = 120;

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub result: ProjectSearchFile,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub files: Vec<Snapshot>,
    pub truncated: bool,
    pub cancelled: bool,
    pub warnings: Vec<String>,
    warnings_omitted: usize,
}

/// Search the explicit workspace root and stream one grouped file result at a
/// time. Returning false from `emit` cancels the scan.
pub fn scan(
    root: PathBuf,
    request: &ProjectSearchRequest,
    buffers: Vec<ProjectBufferSnapshot>,
    cancel: Arc<AtomicBool>,
    mut emit: impl FnMut(ProjectSearchFile) -> bool,
) -> Result<ScanResult, String> {
    let root = fs::canonicalize(&root)
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;
    if !root.is_dir() {
        return Err("workspace root is not a directory".into());
    }
    if request.query.len() > MAX_QUERY_BYTES || request.replacement.len() > MAX_QUERY_BYTES {
        return Err("project search query and replacement are limited to 64 KiB".into());
    }
    if request.globs.len() > MAX_GLOBS
        || request.globs.iter().any(|glob| glob.len() > MAX_GLOB_BYTES)
    {
        return Err("project search accepts at most 32 path globs of 4 KiB each".into());
    }
    // Compile before touching the tree so invalid expressions fail promptly.
    let _ = crate::search::preview(
        "",
        &request.query,
        &request.replacement,
        &request.options,
        None,
    )?;
    let globs = compile_globs(&request.globs)?;
    let max_matches = request.max_matches.min(MAX_TOTAL_MATCHES);
    if request.query.is_empty() || max_matches == 0 {
        return Ok(ScanResult::default());
    }

    let mut result = ScanResult::default();
    let mut total_matches = 0usize;
    let mut cache_bytes = 0usize;
    let mut open_paths = std::collections::HashSet::<PathBuf>::new();
    let mut overlays = std::collections::HashMap::<PathBuf, ProjectBufferSnapshot>::new();
    let mut detached = Vec::<ProjectBufferSnapshot>::new();
    for buffer in buffers {
        if let Some(path) = buffer.path.as_deref() {
            if let Some(canonical) = confined_path(&root, Path::new(path)) {
                open_paths.insert(canonical.clone());
                if canonical.exists() {
                    overlays.insert(canonical, buffer);
                } else {
                    detached.push(buffer);
                }
                continue;
            }
        } else {
            detached.push(buffer);
        }
    }

    // Pathless drafts and deleted named buffers cannot be yielded by the
    // workspace walker. Existing open files are overlaid during the one pass.
    for buffer in detached {
        if should_stop(&cancel) {
            result.cancelled = true;
            return Ok(finish(result));
        }
        let relative = if let Some(path) = buffer.path.as_deref() {
            let Some(canonical) = confined_path(&root, Path::new(path)) else {
                continue;
            };
            let relative = canonical.strip_prefix(&root).unwrap_or(Path::new(""));
            if !matches_globs(&globs, relative)
                || is_ignored_open(&root, &canonical, request.include_ignored)
            {
                continue;
            }
            Some(relative.to_path_buf())
        } else {
            None
        };
        let Some((file, text)) = buffer_result(
            &root,
            relative.as_deref(),
            &buffer,
            request,
            max_matches - total_matches,
            &mut result,
        )?
        else {
            continue;
        };
        if !cache_and_emit(
            &mut result,
            &mut total_matches,
            &mut cache_bytes,
            max_matches,
            file,
            text,
            &mut emit,
        ) {
            return Ok(finish(result));
        }
    }

    if should_stop(&cancel) {
        result.cancelled = true;
        return Ok(finish(result));
    }
    let builder = workspace_walker(&root, request.include_ignored);
    for entry in builder.build() {
        if should_stop(&cancel) {
            result.cancelled = true;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warn(&mut result, error.to_string());
                continue;
            }
        };
        let path = entry.path();
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let canonical = match fs::canonicalize(path) {
            Ok(path) if path.starts_with(&root) => path,
            _ => continue,
        };
        if open_paths.contains(&canonical) {
            let Some(buffer) = overlays.remove(&canonical) else {
                continue;
            };
            let relative = canonical.strip_prefix(&root).ok();
            if !relative.is_some_and(|path| matches_globs(&globs, path)) {
                continue;
            }
            let Some((file, text)) = buffer_result(
                &root,
                relative,
                &buffer,
                request,
                max_matches - total_matches,
                &mut result,
            )?
            else {
                continue;
            };
            if !cache_and_emit(
                &mut result,
                &mut total_matches,
                &mut cache_bytes,
                max_matches,
                file,
                text,
                &mut emit,
            ) {
                break;
            }
            continue;
        }
        if !matches_globs(&globs, canonical.strip_prefix(&root).unwrap_or(&canonical)) {
            continue;
        }
        let metadata = match fs::metadata(&canonical) {
            Ok(metadata) => metadata,
            Err(error) => {
                warn(&mut result, format!("{}: {error}", path.display()));
                continue;
            }
        };
        if metadata.len() as usize > MAX_FILE_BYTES {
            warn(
                &mut result,
                format!("{} exceeds the 2 MiB search limit", path.display()),
            );
            continue;
        }
        let bytes = match fs::File::open(&canonical).and_then(|file| {
            let mut bytes = Vec::new();
            file.take(MAX_FILE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        }) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn(&mut result, format!("{}: {error}", path.display()));
                continue;
            }
        };
        if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
            continue;
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let file = make_file_result(
            Some(&canonical),
            None,
            None,
            None,
            &text,
            request,
            max_matches.saturating_sub(total_matches),
        )?;
        if file.matches.is_empty() {
            continue;
        }
        if !cache_and_emit(
            &mut result,
            &mut total_matches,
            &mut cache_bytes,
            max_matches,
            file,
            text,
            &mut emit,
        ) {
            break;
        }
    }
    Ok(finish(result))
}

fn compile_globs(globs: &[String]) -> Result<GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    for glob in globs {
        builder
            .add(Glob::new(glob).map_err(|error| format!("invalid path glob `{glob}`: {error}"))?);
    }
    builder
        .build()
        .map_err(|error| format!("invalid path glob: {error}"))
}

fn matches_globs(globs: &GlobSet, relative: &Path) -> bool {
    globs.is_empty() || globs.is_match(relative)
}

fn buffer_result(
    root: &Path,
    relative: Option<&Path>,
    buffer: &ProjectBufferSnapshot,
    request: &ProjectSearchRequest,
    remaining_matches: usize,
    result: &mut ScanResult,
) -> Result<Option<(ProjectSearchFile, String)>, String> {
    let Some(text) = buffer.text.as_deref() else {
        let state = if buffer.dirty { "dirty" } else { "open" };
        let bytes = buffer
            .total_bytes
            .map(|bytes| format!(" ({bytes} bytes)"))
            .unwrap_or_default();
        warn(
            result,
            format!(
                "{state} buffer {}{bytes}: {}",
                buffer.draft_id,
                buffer
                    .skipped_reason
                    .clone()
                    .unwrap_or_else(|| "paged buffer exceeds the 2 MiB search limit".into())
            ),
        );
        return Ok(None);
    };
    if text.len() > MAX_FILE_BYTES {
        warn(
            result,
            format!(
                "open buffer {} exceeds the 2 MiB search limit",
                buffer.draft_id
            ),
        );
        return Ok(None);
    }
    if !valid_text(text) {
        return Ok(None);
    }
    let absolute = relative.map(|path| root.join(path));
    let file = make_file_result(
        absolute.as_deref(),
        Some(buffer.buffer_id.clone()),
        Some(buffer.draft_id.clone()),
        Some(buffer.rev),
        text,
        request,
        remaining_matches,
    )?;
    if file.matches.is_empty() {
        return Ok(None);
    }
    Ok(Some((file, text.to_owned())))
}

fn cache_and_emit(
    result: &mut ScanResult,
    total_matches: &mut usize,
    cache_bytes: &mut usize,
    max_matches: usize,
    file: ProjectSearchFile,
    text: String,
    emit: &mut impl FnMut(ProjectSearchFile) -> bool,
) -> bool {
    let result_bytes = file_payload_bytes(&file, 0);
    if result_bytes > MAX_FILE_RESULT_BYTES {
        result.truncated = true;
        warn(
            result,
            format!("{} result exceeds the 1 MiB per-file result limit", file.id),
        );
        return true;
    }
    let next_cache_bytes = cache_bytes
        .saturating_add(text.len())
        .saturating_add(result_bytes);
    if next_cache_bytes > MAX_CACHE_BYTES {
        result.truncated = true;
        warn(
            result,
            "search result cache reached its 32 MiB limit".into(),
        );
        return false;
    }
    *cache_bytes = next_cache_bytes;
    *total_matches += file.matches.len();
    result.files.push(Snapshot {
        result: file.clone(),
        text,
    });
    if !emit(file) {
        result.cancelled = true;
        return false;
    }
    if *total_matches >= max_matches {
        result.truncated = true;
        return false;
    }
    true
}

pub(crate) fn workspace_walker(root: &Path, include_ignored: bool) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(!include_ignored)
        .git_ignore(!include_ignored)
        .git_exclude(!include_ignored)
        .git_global(!include_ignored)
        .ignore(!include_ignored)
        .parents(!include_ignored)
        .require_git(false)
        .follow_links(false);
    builder.filter_entry(|entry| entry.file_name() != ".git");
    builder
}

fn file_payload_bytes(file: &ProjectSearchFile, text_bytes: usize) -> usize {
    file.matches.iter().fold(text_bytes, |total, matched| {
        total
            .saturating_add(matched.replacement.len())
            .saturating_add(matched.preview.len())
    })
}

fn make_file_result(
    absolute: Option<&Path>,
    buffer_id: Option<String>,
    draft_id: Option<String>,
    rev: Option<u64>,
    text: &str,
    request: &ProjectSearchRequest,
    limit: usize,
) -> Result<ProjectSearchFile, String> {
    let (matches, _) = crate::search::preview(
        text,
        &request.query,
        &request.replacement,
        &request.options,
        None,
    )?;
    let newline_offsets = text
        .bytes()
        .enumerate()
        .filter_map(|(offset, byte)| (byte == b'\n').then_some(offset))
        .collect::<Vec<_>>();
    let matches = matches
        .into_iter()
        .take(limit)
        .map(|matched| {
            let preceding_newlines =
                newline_offsets.partition_point(|offset| *offset < matched.start);
            let line_start = preceding_newlines
                .checked_sub(1)
                .map_or(0, |index| newline_offsets[index] + 1);
            let line = preceding_newlines as u32 + 1;
            let column = (matched.start - line_start) as u32 + 1;
            // Keep snippets bounded even for a regex spanning megabytes.
            let preview_start = text[..matched.start]
                .char_indices()
                .rev()
                .nth(PREVIEW_RADIUS / 3)
                .map_or(0, |(offset, _)| offset);
            let preview_end = text[matched.start..]
                .char_indices()
                .nth(PREVIEW_RADIUS * 5 / 3)
                .map_or(text.len(), |(offset, _)| matched.start + offset);
            ProjectSearchMatch {
                start: matched.start,
                end: matched.end,
                replacement: matched.replacement,
                line,
                column,
                preview: text[preview_start..preview_end].to_owned(),
            }
        })
        .collect();
    Ok(ProjectSearchFile {
        id: buffer_id
            .as_ref()
            .map(|id| format!("buffer:{id}"))
            .unwrap_or_else(|| {
                format!(
                    "file:{}",
                    absolute
                        .map(|path| path.display().to_string())
                        .unwrap_or_default()
                )
            }),
        path: absolute.map(|path| path.to_string_lossy().into_owned()),
        buffer_id,
        draft_id,
        rev,
        matches,
    })
}

fn should_stop(cancel: &AtomicBool) -> bool {
    cancel.load(Ordering::Relaxed)
}
fn valid_text(text: &str) -> bool {
    !text.as_bytes().contains(&0)
}

fn warn(result: &mut ScanResult, message: String) {
    if result.warnings.len() < 19 {
        result.warnings.push(message);
    } else {
        result.warnings_omitted += 1;
    }
}

fn finish(mut result: ScanResult) -> ScanResult {
    if result.warnings_omitted > 0 {
        result.warnings.push(format!(
            "{} additional search warnings omitted",
            result.warnings_omitted
        ));
    }
    result
}

pub(crate) fn confined_path(root: &Path, input: &Path) -> Option<PathBuf> {
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        root.join(input)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    if !normalized.starts_with(root) {
        return None;
    }
    if let Ok(canonical) = fs::canonicalize(&normalized) {
        return canonical.starts_with(root).then_some(canonical);
    }
    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(ancestor.file_name()?.to_os_string());
        ancestor = ancestor.parent()?;
    }
    let canonical_ancestor = fs::canonicalize(ancestor).ok()?;
    if !canonical_ancestor.starts_with(root) {
        return None;
    }
    let mut confined = canonical_ancestor;
    for component in missing.into_iter().rev() {
        confined.push(component);
    }
    confined.starts_with(root).then_some(confined)
}

/// Check ignored open paths too, including deleted buffers which a filesystem
/// walker cannot yield. Rules are loaded root-to-leaf so nearer rules win.
fn is_ignored_open(root: &Path, path: &Path, include_ignored: bool) -> bool {
    if include_ignored {
        return false;
    }
    let relative = match path.strip_prefix(root) {
        Ok(path) => path,
        Err(_) => return true,
    };
    if relative
        .components()
        .any(|component| component.as_os_str().to_string_lossy().starts_with('.'))
    {
        return true;
    }
    let (global, _) = GitignoreBuilder::new(root).build_global();
    let mut ignored = is_ignore_match(global.matched(path, false));
    let repository_exclude = root.join(".git/info/exclude");
    if repository_exclude.is_file() {
        let (matcher, _) = Gitignore::new(repository_exclude);
        match matcher.matched(path, false) {
            Match::Ignore(_) => ignored = true,
            Match::Whitelist(_) => ignored = false,
            Match::None => {}
        }
    }
    let mut dirs = Vec::new();
    let mut dir = path.parent();
    while let Some(current) = dir {
        dirs.push(current);
        dir = current.parent();
    }
    dirs.reverse();
    for dir in dirs {
        for filename in [".gitignore", ".ignore"] {
            let ignore_file = dir.join(filename);
            if !ignore_file.is_file() {
                continue;
            }
            let (matcher, _) = Gitignore::new(ignore_file);
            match matcher.matched(path, false) {
                Match::Ignore(_) => ignored = true,
                Match::Whitelist(_) => ignored = false,
                Match::None => {}
            }
        }
    }
    ignored
}

fn is_ignore_match(result: Match<&ignore::gitignore::Glob>) -> bool {
    matches!(result, Match::Ignore(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresh_gui_protocol::SearchOptions;
    use uuid::Uuid;

    fn fixture() -> PathBuf {
        let root = std::env::temp_dir().join(format!("fresh-project-search-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn request() -> ProjectSearchRequest {
        ProjectSearchRequest {
            query: "needle".into(),
            replacement: "replacement".into(),
            options: SearchOptions {
                case_sensitive: true,
                whole_word: false,
                use_regex: false,
            },
            globs: Vec::new(),
            include_ignored: false,
            max_matches: 2_000,
        }
    }

    #[test]
    fn walks_workspace_respects_gitignore_and_streams_grouped_matches() {
        let root = fixture();
        fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.join("a.txt"), "needle\nneedle").unwrap();
        fs::write(root.join("ignored.txt"), "needle").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let streamed = seen.clone();
        let result = scan(
            root.clone(),
            &request(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            move |file| {
                streamed.lock().unwrap().push(file);
                true
            },
        )
        .unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].result.matches.len(), 2);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(
            seen.lock().unwrap()[0]
                .path
                .as_deref()
                .unwrap()
                .ends_with("a.txt")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_snapshots_replace_disk_and_pathless_drafts_are_searched() {
        let root = fixture();
        let path = root.join("open.txt");
        fs::write(&path, "disk has needle").unwrap();
        let buffers = vec![
            ProjectBufferSnapshot {
                buffer_id: "open-id".into(),
                draft_id: "open-draft".into(),
                rev: 7,
                path: Some(path.display().to_string()),
                text: Some("draft has needle".into()),
                total_bytes: None,
                dirty: true,
                skipped_reason: None,
            },
            ProjectBufferSnapshot {
                buffer_id: "draft-id".into(),
                draft_id: "draft-id".into(),
                rev: 2,
                path: None,
                text: Some("orphan needle".into()),
                total_bytes: None,
                dirty: true,
                skipped_reason: None,
            },
        ];
        let result = scan(
            root.clone(),
            &request(),
            buffers,
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        assert_eq!(result.files.len(), 2);
        assert!(
            result
                .files
                .iter()
                .any(|file| file.text == "draft has needle")
        );
        assert!(
            result
                .files
                .iter()
                .any(|file| file.result.buffer_id.as_deref() == Some("draft-id"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cancels_before_enumeration_and_rejects_invalid_patterns() {
        let root = fixture();
        let cancelled = scan(
            root.clone(),
            &request(),
            Vec::new(),
            Arc::new(AtomicBool::new(true)),
            |_| panic!("must not emit"),
        )
        .unwrap();
        assert!(cancelled.cancelled);
        let mut invalid = request();
        invalid.options.use_regex = true;
        invalid.query = "[".into();
        assert!(
            scan(
                root.clone(),
                &invalid,
                Vec::new(),
                Arc::new(AtomicBool::new(false)),
                |_| true
            )
            .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn open_buffers_obey_ignore_globs_and_replace_missing_disk_entries() {
        let root = fixture();
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "disk needle").unwrap();
        fs::write(root.join("src.rs"), "disk needle").unwrap();
        let make = |buffer_id: &str, path: &str, text: Option<&str>| ProjectBufferSnapshot {
            buffer_id: buffer_id.into(),
            draft_id: format!("draft-{buffer_id}"),
            path: Some(root.join(path).display().to_string()),
            rev: 4,
            text: text.map(str::to_owned),
            total_bytes: None,
            dirty: true,
            skipped_reason: text.is_none().then(|| "paged".into()),
        };
        let buffers = vec![
            make("ignored", "ignored.rs", Some("dirty needle")),
            make("missing", "missing.rs", Some("deleted needle")),
            make("paged", "src.rs", None),
        ];
        let mut scoped = request();
        scoped.globs = vec!["*.rs".into()];
        let result = scan(
            root.clone(),
            &scoped,
            buffers.clone(),
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        assert_eq!(
            result.files.len(),
            1,
            "ignored and paged buffers are excluded"
        );
        assert!(result.files[0].text.contains("deleted needle"));
        let mut include = scoped;
        include.include_ignored = true;
        let result = scan(
            root.clone(),
            &include,
            buffers,
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        assert!(result.files.iter().any(|file| file.text == "dirty needle"));
        assert!(
            result
                .files
                .iter()
                .any(|file| file.text == "deleted needle")
        );
        assert!(
            !result.files.iter().any(|file| file.text == "disk needle"),
            "named paged buffer suppresses stale disk contents"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn include_ignored_and_stream_cancellation_and_global_limit_work() {
        let root = fixture();
        fs::write(root.join(".gitignore"), "hidden.txt\n").unwrap();
        fs::write(root.join("hidden.txt"), "needle").unwrap();
        let mut request = request();
        request.include_ignored = true;
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = cancel.clone();
        let result = scan(root.clone(), &request, Vec::new(), cancel, move |_| {
            stop.store(true, Ordering::Relaxed);
            false
        })
        .unwrap();
        assert!(result.cancelled);
        assert_eq!(result.files.len(), 1);

        fs::write(root.join("many.txt"), "needle needle needle").unwrap();
        request.max_matches = 1;
        let result = scan(
            root.clone(),
            &request,
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        assert!(result.truncated);
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.result.matches.len())
                .sum::<usize>(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unicode_offsets_and_preview_are_bounded() {
        let root = fixture();
        let text = format!("é{}needle{}", "x".repeat(500), "y".repeat(500));
        fs::write(root.join("unicode.txt"), &text).unwrap();
        let result = scan(
            root.clone(),
            &request(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        let matched = &result.files[0].result.matches[0];
        assert_eq!(matched.column, ("é".len() + 500) as u32 + 1);
        assert_eq!(matched.start, "é".len() + 500);
        assert!(matched.preview.len() < 500);
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_cannot_escape_workspace() {
        use std::os::unix::fs::symlink;
        let root = fixture();
        let outside = fixture();
        fs::write(outside.join("secret.txt"), "needle").unwrap();
        symlink(&outside, root.join("outside")).unwrap();
        let result = scan(
            root.clone(),
            &request(),
            Vec::new(),
            Arc::new(AtomicBool::new(false)),
            |_| true,
        )
        .unwrap();
        assert!(result.files.is_empty());
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
