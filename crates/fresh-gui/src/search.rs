//! Bounded Fresh-compatible in-buffer search preview.

use fresh_gui_protocol::{
    ByteRange, MAX_SEARCH_DRAFT_BYTES, MAX_SEARCH_MATCHES, SearchMatch, SearchOptions,
};

#[allow(dead_code)] // The pinned Fresh module exposes more helpers than ADE needs.
mod fresh_regex_replace {
    use super::MAX_SEARCH_DRAFT_BYTES;
    include!("../../../vendor/fresh/crates/fresh-editor/src/app/regex_replace.rs");

    pub fn collect_bounded(
        regex: &regex::bytes::Regex,
        haystack: &str,
        template: &str,
        limit: usize,
    ) -> Result<Vec<ReplaceMatch>, String> {
        let escaped = interpret_escapes(template);
        let normalized = normalize_replacement(&escaped);
        let references = normalized.bytes().filter(|byte| *byte == b'$').count();
        let mut output_bytes = 0usize;
        let mut result = Vec::new();
        for captures in regex
            .captures_iter(haystack.as_bytes())
            .filter(|captures| {
                let matched = captures.get(0).expect("capture set has whole match");
                haystack.is_char_boundary(matched.start())
                    && haystack.is_char_boundary(matched.end())
            })
            .take(limit)
        {
            let matched = captures.get(0).expect("capture set has whole match");
            // Bound expansion before allocating it. Each `$` could expand to
            // any capture up to the whole match length, so this is conservative.
            let upper_bound = references
                .checked_mul(matched.len())
                .and_then(|extra| normalized.len().checked_add(extra))
                .ok_or_else(|| "replacement preview exceeds the 8 MiB output limit".to_owned())?;
            if upper_bound > MAX_SEARCH_DRAFT_BYTES.saturating_sub(output_bytes) {
                return Err("replacement preview exceeds the 8 MiB output limit".into());
            }
            let mut expanded = Vec::new();
            captures.expand(normalized.as_bytes(), &mut expanded);
            output_bytes += upper_bound;
            result.push(ReplaceMatch {
                offset: matched.start(),
                len: matched.len(),
                replacement: String::from_utf8_lossy(&expanded).into_owned(),
            });
        }
        Ok(result)
    }
}

pub fn preview(
    text: &str,
    query: &str,
    replacement: &str,
    options: &SearchOptions,
    scope: Option<ByteRange>,
) -> Result<(Vec<SearchMatch>, bool), String> {
    if text.len() > MAX_SEARCH_DRAFT_BYTES {
        return Err(format!(
            "search draft exceeds the {} MiB limit",
            MAX_SEARCH_DRAFT_BYTES / (1024 * 1024)
        ));
    }
    let (base, haystack) = match scope {
        Some(range) => {
            let end = range
                .start
                .checked_add(range.len)
                .ok_or_else(|| "search scope overflows buffer length".to_owned())?;
            if end > text.len()
                || !text.is_char_boundary(range.start)
                || !text.is_char_boundary(end)
            {
                return Err("search scope must be a valid UTF-8 byte range".into());
            }
            (range.start, &text[range.start..end])
        }
        None => (0, text),
    };
    if query.is_empty() {
        return Ok((Vec::new(), false));
    }

    let find = fresh_regex_replace::build_search_regex(
        query,
        options.use_regex,
        options.whole_word,
        options.case_sensitive,
    )?;
    let limit = MAX_SEARCH_MATCHES + 1;
    let mut matches = Vec::with_capacity(MAX_SEARCH_MATCHES.min(256));
    if options.use_regex {
        let replace = fresh_regex_replace::build_regex(
            query,
            true,
            options.whole_word,
            options.case_sensitive,
        )
        .ok_or_else(|| "invalid regular expression".to_owned())?;
        for matched in fresh_regex_replace::collect_bounded(&replace, haystack, replacement, limit)?
        {
            matches.push(SearchMatch {
                start: base + matched.offset,
                end: base + matched.offset + matched.len,
                replacement: matched.replacement,
            });
        }
    } else {
        let mut output_bytes = 0usize;
        for matched in find.find_iter(haystack).take(limit) {
            if replacement.len() > MAX_SEARCH_DRAFT_BYTES.saturating_sub(output_bytes) {
                return Err("replacement preview exceeds the 8 MiB output limit".into());
            }
            output_bytes += replacement.len();
            matches.push(SearchMatch {
                start: base + matched.start(),
                end: base + matched.end(),
                replacement: replacement.to_owned(),
            });
        }
    }
    let capped = matches.len() > MAX_SEARCH_MATCHES;
    matches.truncate(MAX_SEARCH_MATCHES);
    Ok((matches, capped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_worker::EditorHandle;
    use fresh_gui_client::{edit_sync::contiguous_diff, search::SearchReview};
    use fresh_gui_protocol::{ByteSelection, EditorAction};
    use std::path::PathBuf;

    fn test_editor(label: &str) -> (PathBuf, EditorHandle) {
        let root =
            std::env::temp_dir().join(format!("fresh-search-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            root.join("recovery"),
        )
        .expect("Fresh worker starts");
        (root, editor)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn opts(use_regex: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive: true,
            whole_word: false,
            use_regex,
        }
    }

    #[test]
    fn literal_replacement_and_empty_deletion_are_literal() {
        let (found, capped) = preview("a.b a.b", "a.b", "$1\\n", &opts(false), None).unwrap();
        assert!(!capped);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].replacement, "$1\\n");
        let (deleted, _) = preview("foo foo", "foo", "", &opts(false), None).unwrap();
        assert!(deleted.iter().all(|item| item.replacement.is_empty()));
    }

    #[test]
    fn regex_captures_and_replacement_escapes_follow_fresh() {
        let (found, _) = preview("ab ac", "a(.)", "$1\\n", &opts(true), None).unwrap();
        assert_eq!(found[0].replacement, "b\n");
        assert_eq!(found[1].replacement, "c\n");
    }

    #[test]
    fn zero_width_word_boundary_capture_uses_original_haystack_context() {
        let (found, _) = preview("é x", r"(\b)", "<$1>", &opts(true), None).unwrap();
        assert!(!found.is_empty());
        assert!(found.iter().all(|item| item.replacement == "<>"));
    }

    #[test]
    fn zero_width_unicode_matches_never_report_interior_byte_offsets() {
        let (found, _) = preview("éx", "()", "x", &opts(true), None).unwrap();
        assert_eq!(
            found.iter().map(|item| item.start).collect::<Vec<_>>(),
            [0, 2, 3]
        );
    }

    #[test]
    fn unicode_offsets_selection_scope_and_anchors_are_respected() {
        let options = SearchOptions {
            whole_word: false,
            ..opts(true)
        };
        let text = "é\nfoo\nfoo";
        let (found, _) = preview(
            text,
            "^foo$",
            "x",
            &options,
            Some(ByteRange { start: 3, len: 4 }),
        )
        .unwrap();
        assert_eq!((found[0].start, found[0].end), (3, 6));
        assert!(
            preview(
                text,
                "foo",
                "x",
                &options,
                Some(ByteRange { start: 1, len: 2 })
            )
            .is_err()
        );
    }

    #[test]
    fn unicode_case_folding_and_whole_word_matching_follow_fresh_options() {
        let mut options = opts(false);
        options.case_sensitive = false;
        let (case_match, _) = preview("Ångström", "ång", "x", &options, None).unwrap();
        assert_eq!((case_match[0].start, case_match[0].end), (0, 4));

        options.whole_word = true;
        let (word_matches, _) = preview("promote mot", "mot", "x", &options, None).unwrap();
        assert_eq!(word_matches.len(), 1);
        assert_eq!(word_matches[0].start, 8);
    }

    #[test]
    fn zero_width_matches_are_reported_and_result_is_capped() {
        let (found, _) = preview("éx", r"^|$", "!", &opts(true), None).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].start, 3);
        let (found, capped) = preview(
            &"x".repeat(MAX_SEARCH_MATCHES + 1),
            "x",
            "",
            &opts(false),
            None,
        )
        .unwrap();
        assert_eq!(found.len(), MAX_SEARCH_MATCHES);
        assert!(capped);
    }

    #[test]
    fn invalid_regex_and_oversized_drafts_return_errors() {
        assert!(preview("text", "[", "x", &opts(true), None).is_err());
        let text = "x".repeat(MAX_SEARCH_DRAFT_BYTES + 1);
        assert!(preview(&text, "x", "", &opts(false), None).is_err());
    }

    #[test]
    fn expanded_replacement_output_has_a_separate_memory_budget() {
        let text = "x".repeat(10_000);
        let replacement = format!("{}$1", "z".repeat(1024));
        let error = preview(&text, "(x)", &replacement, &opts(true), None).unwrap_err();
        assert!(error.contains("8 MiB output limit"));
    }

    #[test]
    fn reviewed_search_is_one_fresh_undo_group_and_rejects_stale_revision() {
        let (root, editor) = test_editor("review");
        let path = root.join("review.txt");
        let initial = "café aa xx aa yy aa";
        std::fs::write(&path, initial).unwrap();
        runtime().block_on(async {
            let opened = editor.open(path.clone(), false).await.unwrap();
            assert_eq!(opened.text, initial);

            // Scope to the selected suffix and use Fresh's regex capture/escape rules.
            let scope_start = "café ".len();
            let scope = ByteRange {
                start: scope_start,
                len: initial.len() - scope_start,
            };
            let options = SearchOptions {
                use_regex: true,
                ..SearchOptions::default()
            };
            let (matches, capped) =
                preview(initial, "(aa)", r"$1\t", &options, Some(scope)).unwrap();
            assert!(!capped);
            assert_eq!(matches.len(), 3);
            assert!(matches.iter().all(|matched| matched.replacement == "aa\t"));

            let mut review = SearchReview::new(initial, matches);
            review.accept();
            review.skip();
            review.accept_all();
            // Cancel finalizes only explicit accepts: the skipped middle match stays unchanged.
            let edited = review.cancel().unwrap();
            assert_eq!(edited, "café aa\t xx aa yy aa\t");
            let edits = contiguous_diff(initial, &edited);
            assert_eq!(edits.len(), 1);
            let applied = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "search-review-test".into(),
                    opened.rev,
                    edits,
                    None,
                    ByteSelection {
                        anchor: edited.len(),
                        head: edited.len(),
                    },
                )
                .await
                .unwrap();
            assert!(applied.accepted);
            assert_eq!(applied.text, edited);

            let stale = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "search-review-test".into(),
                    opened.rev,
                    vec![],
                    None,
                    applied.selection,
                )
                .await
                .unwrap();
            assert!(!stale.accepted);
            assert_eq!(stale.text, edited);

            let undone = editor
                .action(
                    opened.buffer_id.clone(),
                    "search-review-test".into(),
                    applied.rev,
                    EditorAction::Undo,
                    applied.selection,
                )
                .await
                .unwrap();
            assert!(undone.accepted);
            assert_eq!(
                undone.text, initial,
                "all accepted matches undo as one group"
            );
            let redone = editor
                .action(
                    opened.buffer_id,
                    "search-review-test".into(),
                    undone.rev,
                    EditorAction::Redo,
                    undone.selection,
                )
                .await
                .unwrap();
            assert!(redone.accepted);
            assert_eq!(redone.text, edited);
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn paged_search_edits_only_loaded_page_and_reports_undo_limitation() {
        let (root, editor) = test_editor("paged");
        let path = root.join("large.txt");
        let marker = 2 * 1024 * 1024 + 100;
        let mut contents = vec![b'a'; 3 * 1024 * 1024];
        contents[marker..marker + b"target".len()].copy_from_slice(b"target");
        let mut expected_contents = contents.clone();
        expected_contents.splice(
            marker..marker + b"target".len(),
            b"replacement".iter().copied(),
        );
        std::fs::write(&path, contents).unwrap();
        runtime().block_on(async {
            let opened = editor.open(path.clone(), false).await.unwrap();
            assert!(opened.text.is_empty());
            let page_start = marker - 32;
            let page = editor
                .read_page(opened.buffer_id.clone(), page_start, 128)
                .await
                .unwrap();
            let (matches, _) = preview(
                &page.text,
                "target",
                "replacement",
                &SearchOptions::default(),
                None,
            )
            .unwrap();
            assert_eq!(matches.len(), 1);
            let edited_page =
                fresh_gui_client::search::apply_matches(&page.text, &matches).unwrap();
            let edits = contiguous_diff(&page.text, &edited_page)
                .into_iter()
                .map(|edit| fresh_gui_protocol::RangeEdit {
                    start: page.start + edit.start,
                    end: page.start + edit.end,
                    text: edit.text,
                })
                .collect();
            let selection = ByteSelection {
                anchor: page.start + page.text.len(),
                head: page.start + page.text.len(),
            };
            let applied = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "paged-search-test".into(),
                    opened.rev,
                    edits,
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    selection,
                )
                .await
                .unwrap();
            assert!(applied.accepted);
            assert!(applied.page.as_ref().unwrap().text.contains("replacement"));

            let undo = editor
                .action(
                    opened.buffer_id.clone(),
                    "paged-search-test".into(),
                    applied.rev,
                    EditorAction::Undo,
                    applied.selection,
                )
                .await;
            assert!(
                undo.is_err(),
                "paged undo is not implemented by Fresh's ADE path"
            );
            let unchanged = editor
                .read_page(opened.buffer_id.clone(), page.start, 128)
                .await
                .unwrap();
            assert_eq!(unchanged.text, edited_page);

            // Saving proves the replacement touched only its global byte range;
            // the entire 3 MiB source must otherwise remain byte-for-byte intact.
            editor
                .save(opened.buffer_id, applied.rev, None)
                .await
                .unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), expected_contents);
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }
}
