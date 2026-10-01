//! Client-side state for reviewing in-buffer search replacements.
//!
//! Search coordinates are UTF-8 byte offsets into the exact text snapshot that
//! produced the matches. The daemon owns matching and replacement expansion;
//! this module only tracks user decisions and applies reviewed edits.

use fresh_gui_protocol::SearchMatch;

/// Review state for a fixed search result set.
#[derive(Debug, Clone)]
pub struct SearchReview {
    text: String,
    matches: Vec<SearchMatch>,
    visit_order: Vec<usize>,
    visit_position: usize,
    accepted: Vec<bool>,
}

impl SearchReview {
    pub fn new(text: impl Into<String>, matches: Vec<SearchMatch>) -> Self {
        Self::new_at(text, matches, 0)
    }

    /// Create a review that starts at a match index and wraps once through the
    /// source-ordered result set. `start_index` is clamped to the result count.
    pub fn new_at(
        text: impl Into<String>,
        matches: Vec<SearchMatch>,
        start_index: usize,
    ) -> Self {
        let len = matches.len();
        let start_index = start_index.min(len);
        let visit_order = if len == 0 || start_index == len {
            (0..len).collect()
        } else {
            (start_index..len).chain(0..start_index).collect()
        };
        let accepted = vec![false; matches.len()];
        Self {
            text: text.into(),
            matches,
            visit_order,
            visit_position: 0,
            accepted,
        }
    }

    pub fn current(&self) -> Option<&SearchMatch> {
        self.visit_order
            .get(self.visit_position)
            .and_then(|&index| self.matches.get(index))
    }

    pub fn current_index(&self) -> Option<usize> {
        self.visit_order.get(self.visit_position).copied()
    }

    pub fn is_done(&self) -> bool {
        self.visit_position >= self.visit_order.len()
    }

    /// Accept the current match and advance to the next one.
    pub fn accept(&mut self) {
        if let Some(&index) = self.visit_order.get(self.visit_position) {
            let accepted = &mut self.accepted[index];
            *accepted = true;
            self.visit_position += 1;
        }
    }

    /// Leave the current match unchanged and advance to the next one.
    pub fn skip(&mut self) {
        if !self.is_done() {
            self.visit_position += 1;
        }
    }

    /// Accept every match from the current position onward.
    pub fn accept_all(&mut self) {
        for &index in &self.visit_order[self.visit_position..] {
            self.accepted[index] = true;
        }
        self.visit_position = self.visit_order.len();
    }

    /// Whether the match set still refers to this exact text snapshot.
    pub fn matches_text(&self, text: &str) -> bool {
        self.text == text
    }

    /// Return the edited text, preserving accepted decisions even if review was
    /// cancelled before reaching the end. Invalid or stale ranges return None.
    pub fn replacements(&self) -> Option<String> {
        let accepted = self
            .matches
            .iter()
            .zip(&self.accepted)
            .filter_map(|(matched, &accepted)| accepted.then_some(matched.clone()))
            .collect::<Vec<_>>();
        apply_matches(&self.text, &accepted)
    }

    /// Apply the accepted edits as a completed review. This is intentionally
    /// identical to `replacements`; callers commit the returned text as one
    /// undo group through the range-edit path.
    pub fn finish(&self) -> Option<String> {
        self.replacements()
    }

    /// Cancel review while retaining replacements already explicitly accepted.
    pub fn cancel(&self) -> Option<String> {
        self.replacements()
    }
}

/// Apply sorted, non-overlapping byte-range replacements to `text`.
///
/// Ranges must be in ascending order, lie on UTF-8 character boundaries, and
/// not overlap. Edits are applied from the end so earlier byte offsets remain
/// valid. Returns `None` for malformed ranges.
pub fn apply_matches(text: &str, matches: &[SearchMatch]) -> Option<String> {
    let mut previous_end = 0;
    for matched in matches {
        if matched.start < previous_end
            || matched.start > matched.end
            || matched.end > text.len()
            || !text.is_char_boundary(matched.start)
            || !text.is_char_boundary(matched.end)
        {
            return None;
        }
        previous_end = matched.end;
    }

    let mut result = text.to_owned();
    for matched in matches.iter().rev() {
        result.replace_range(matched.start..matched.end, &matched.replacement);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matched(start: usize, end: usize, replacement: &str) -> SearchMatch {
        SearchMatch {
            start,
            end,
            replacement: replacement.to_owned(),
        }
    }

    #[test]
    fn apply_matches_handles_deletion_and_utf8_offsets() {
        let text = "café cat";
        let matches = [matched(0, 5, ""), matched(6, 9, "dog")];
        assert_eq!(apply_matches(text, &matches).as_deref(), Some(" dog"));
    }

    #[test]
    fn apply_matches_supports_zero_length_insertions_once() {
        let text = "ab";
        let matches = [matched(1, 1, "X")];
        assert_eq!(apply_matches(text, &matches).as_deref(), Some("aXb"));

        let mut review = SearchReview::new(text, matches.to_vec());
        assert_eq!(review.current_index(), Some(0));
        review.accept();
        assert!(review.is_done());
        assert_eq!(review.current(), None);
        review.accept();
        assert_eq!(review.replacements().as_deref(), Some("aXb"));
    }

    #[test]
    fn review_accept_skip_all_cancel_and_finish_keep_expected_matches() {
        let text = "one two three";
        let matches = vec![matched(0, 3, "1"), matched(4, 7, "2"), matched(8, 13, "3")];
        let mut review = SearchReview::new(text, matches.clone());
        review.accept();
        review.skip();
        review.accept();
        assert_eq!(review.finish().as_deref(), Some("1 two 3"));

        let mut review = SearchReview::new(text, matches);
        review.accept();
        review.skip();
        review.accept_all();
        assert!(review.is_done());
        assert_eq!(review.cancel().as_deref(), Some("1 2 3"));
    }

    #[test]
    fn review_rejects_stale_text_and_invalid_ranges() {
        let review = SearchReview::new("hello", vec![matched(0, 2, "hi")]);
        assert!(review.matches_text("hello"));
        assert!(!review.matches_text("hello!"));
        assert_eq!(
            apply_matches("é", &[matched(1, 2, "x")]),
            None,
            "offset inside a UTF-8 codepoint must be rejected"
        );
        assert_eq!(
            apply_matches("abcdef", &[matched(0, 4, "x"), matched(3, 5, "y")]),
            None
        );
    }

    #[test]
    fn review_starts_at_match_then_wraps_once_in_source_order() {
        let text = "abc";
        let matches = vec![matched(0, 0, "X"), matched(1, 2, "B"), matched(2, 3, "C")];
        let mut review = SearchReview::new_at(text, matches, 1);

        assert_eq!(review.current_index(), Some(1));
        review.accept();
        assert_eq!(review.current_index(), Some(2));
        review.skip();
        assert_eq!(review.current_index(), Some(0));
        review.accept();

        assert!(review.is_done());
        assert_eq!(review.current_index(), None);
        assert_eq!(review.finish().as_deref(), Some("XaBc"));
    }
}
