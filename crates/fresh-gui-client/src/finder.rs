//! Pure presentation helpers for the native quick finder. File ranking and IO
//! stay on the daemon; line previews operate on the current editor snapshot.

/// Resolve a one-based `line[:column]` to a UTF-8 byte position. Columns count
/// characters, as the native editor does. Clamp positions to the loaded text.
pub fn line_position(text: &str, query: &str) -> Option<usize> {
    let (line, column) = query.split_once(':').unwrap_or((query, "1"));
    let line = line.parse::<usize>().ok()?.checked_sub(1)?;
    let column = column.parse::<usize>().ok()?.checked_sub(1)?;
    let start = text
        .match_indices('\n')
        .nth(line.saturating_sub(1))
        .map(|(offset, _)| offset + 1);
    let start = if line == 0 {
        0
    } else {
        start.unwrap_or(text.len())
    };
    let content = text[start..]
        .split('\n')
        .next()
        .unwrap_or("")
        .trim_end_matches('\r');
    Some(
        start
            + content
                .char_indices()
                .nth(column)
                .map_or(content.len(), |(offset, _)| offset),
    )
}

/// Rank in-memory labels using the same matcher as daemon file discovery.
#[cfg(not(doctest))]
pub fn ranked<T>(
    query: &str,
    items: impl IntoIterator<Item = T>,
    label: impl Fn(&T) -> &str,
) -> Vec<T> {
    let mut matcher = crate::fuzzy::FuzzyMatcher::new(query);
    let mut matches: Vec<_> = items
        .into_iter()
        .filter_map(|item| {
            let matched = matcher.match_target(label(&item));
            matched.matched.then_some((matched.score, item))
        })
        .collect();
    matches.sort_by(|(a_score, a), (b_score, b)| {
        b_score.cmp(a_score).then_with(|| label(a).cmp(label(b)))
    });
    matches.into_iter().map(|(_, item)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn multi_term_labels_use_fresh_scores() {
        let labels = vec!["Open Settings", "Open Default Settings", "New File"];
        let ranked = ranked("op set", labels.clone(), |label| label);
        let mut matcher = crate::fuzzy::FuzzyMatcher::new("op set");
        assert_eq!(ranked.len(), 2);
        assert!(matcher.match_target(ranked[0]).score >= matcher.match_target(ranked[1]).score);
    }
    #[test]
    fn preview_positions_are_one_based_and_unicode_safe() {
        let text = "first\r\né🐈last\n";
        assert_eq!(line_position(text, "2:3"), Some(13));
        assert_eq!(line_position(text, "2:99"), Some(17));
        assert_eq!(line_position(text, "99"), Some(text.len()));
        for query in ["0", "1:0", "no", "1:2:3", ""] {
            assert_eq!(line_position(text, query), None);
        }
    }
}
