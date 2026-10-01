//! Bounded Fresh-compatible in-buffer search preview.

use fresh_gui_protocol::{
    ByteRange, SearchMatch, SearchOptions, MAX_SEARCH_DRAFT_BYTES, MAX_SEARCH_MATCHES,
};

#[path = "../../../vendor/fresh/crates/fresh-editor/src/app/regex_replace.rs"]
mod fresh_regex_replace;

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
        for captures in replace.captures_iter(haystack.as_bytes()).take(limit) {
            let matched = captures.get(0).expect("capture set has whole match");
            matches.push(SearchMatch {
                start: base + matched.start(),
                end: base + matched.end(),
                replacement: fresh_regex_replace::expand_replacement(
                    &replace,
                    matched.as_bytes(),
                    replacement,
                ),
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
    fn unicode_offsets_selection_scope_and_anchors_are_respected() {
        let options = SearchOptions { whole_word: false, ..opts(true) };
        let text = "é\nfoo\nfoo";
        let (found, _) = preview(text, "^foo$", "x", &options, Some(ByteRange { start: 3, len: 4 })).unwrap();
        assert_eq!((found[0].start, found[0].end), (3, 6));
        assert!(preview(text, "foo", "x", &options, Some(ByteRange { start: 1, len: 2 })).is_err());
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
