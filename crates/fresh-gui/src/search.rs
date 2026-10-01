//! Bounded Fresh-compatible in-buffer search preview.

use fresh_gui_protocol::{
    ByteRange, SearchMatch, SearchOptions, MAX_SEARCH_DRAFT_BYTES, MAX_SEARCH_MATCHES,
};

#[allow(dead_code)] // The pinned Fresh module exposes more helpers than ADE needs.
mod fresh_regex_replace {
    include!("../../../vendor/fresh/crates/fresh-editor/src/app/regex_replace.rs");

    pub fn collect_bounded(
        regex: &regex::bytes::Regex,
        haystack: &str,
        template: &str,
        limit: usize,
    ) -> Vec<ReplaceMatch> {
        let escaped = interpret_escapes(template);
        let normalized = normalize_replacement(&escaped);
        regex
            .captures_iter(haystack.as_bytes())
            .filter(|captures| {
                let matched = captures.get(0).expect("capture set has whole match");
                haystack.is_char_boundary(matched.start()) && haystack.is_char_boundary(matched.end())
            })
            .take(limit)
            .map(|captures| {
                let matched = captures.get(0).expect("capture set has whole match");
                let mut expanded = Vec::new();
                captures.expand(normalized.as_bytes(), &mut expanded);
                ReplaceMatch {
                    offset: matched.start(),
                    len: matched.len(),
                    replacement: String::from_utf8_lossy(&expanded).into_owned(),
                }
            })
            .collect()
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
        for matched in fresh_regex_replace::collect_bounded(
            &replace,
            haystack,
            replacement,
            limit,
        ) {
            matches.push(SearchMatch {
                start: base + matched.offset,
                end: base + matched.offset + matched.len,
                replacement: matched.replacement,
            });
        }
    } else {
        for matched in find.find_iter(haystack).take(limit) {
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
        assert_eq!(found.iter().map(|item| item.start).collect::<Vec<_>>(), [0, 2, 3]);
    }

    #[test]
    fn unicode_offsets_selection_scope_and_anchors_are_respected() {
        let options = SearchOptions { whole_word: false, ..opts(true) };
        let text = "é\nfoo\nfoo";
        let (found, _) = preview(text, "^foo$", "x", &options, Some(ByteRange { start: 3, len: 4 })).unwrap();
        assert_eq!((found[0].start, found[0].end), (3, 6));
        assert!(preview(text, "foo", "x", &options, Some(ByteRange { start: 1, len: 2 })).is_err());
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
        let (found, capped) = preview(&"x".repeat(MAX_SEARCH_MATCHES + 1), "x", "", &opts(false), None).unwrap();
        assert_eq!(found.len(), MAX_SEARCH_MATCHES);
        assert!(capped);
    }

    #[test]
    fn invalid_regex_and_oversized_drafts_return_errors() {
        assert!(preview("text", "[", "x", &opts(true), None).is_err());
        let text = "x".repeat(MAX_SEARCH_DRAFT_BYTES + 1);
        assert!(preview(&text, "x", "", &opts(false), None).is_err());
    }
}
