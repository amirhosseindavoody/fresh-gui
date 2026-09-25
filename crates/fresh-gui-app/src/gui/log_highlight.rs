//! Theme-aware highlighting for plain log files in the host editor.

use std::ops::Range;
use std::rc::Rc;
use std::sync::OnceLock;

use gpui::{Context, HighlightStyle, SharedString, Window};
use gpui_kit::component::input::{
    EditorState, FoldRange, HighlightStyleResolver, InputEdit, InputHighlighter,
    InputHighlighterFactory, Rope,
};
use regex::Regex;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Token {
    range: Range<usize>,
    style: &'static str,
}

fn patterns() -> &'static [(Regex, &'static str)] {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        [
            // Quoted content wins over keywords and numbers inside it.
            (r#"(?:\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])*')"#, "string"),
            // File references are kept together, including optional line/column.
            (r"(?:[A-Za-z]:[\\/]|\.{1,2}[\\/]|~[\\/]|[\\/])?[\w.~-]+(?:[\\/][\w.~-]+)*\.[A-Za-z][A-Za-z0-9]*(?::[0-9]+){1,2}", "label"),
            (r"\b\d{4}[-/]\d{1,2}[-/]\d{1,2}(?:[T ]\d{1,2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:Z|[+-]\d{2}:?\d{2})?)?\b", "constant"),
            (r"\b\d{1,2}:\d{2}(?::\d{2}(?:\.\d+)?)?\b", "constant"),
            (r"(?i)\b(?:fail|failed|failure|error|fatal|critical|panic)\b", "keyword"),
            (r"(?i)\b(?:warn|warning)\b", "constant"),
            (r"(?i)\b(?:info|notice|success)\b", "function"),
            (r"(?i)\b(?:debug|trace)\b", "comment"),
            (r"\b[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?\b", "number"),
        ]
        .into_iter()
        .map(|(pattern, style)| (Regex::new(pattern).expect("valid log token pattern"), style))
        .collect()
    })
}

fn tokens(text: &str) -> Vec<Token> {
    let mut occupied = vec![false; text.len()];
    let mut found = Vec::new();
    for (pattern, style) in patterns() {
        for match_ in pattern.find_iter(text) {
            let range = match_.range();
            if occupied[range.clone()].iter().any(|used| *used) {
                continue;
            }
            occupied[range.clone()].fill(true);
            found.push(Token { range, style: *style });
        }
    }
    found.sort_by_key(|token| token.range.start);
    found
}

#[derive(Default)]
struct LogHighlighter {
    text_len: usize,
    tokens: Vec<Token>,
}

impl InputHighlighter for LogHighlighter {
    fn language(&self) -> SharedString { "log".into() }

    fn update(
        &mut self,
        _: Option<InputEdit>,
        text: &Rope,
        _: bool,
        _: &mut Window,
        _: &mut Context<EditorState>,
    ) {
        let text = text.to_string();
        self.text_len = text.len();
        self.tokens = tokens(&text);
    }

    fn styles(
        &self,
        range: &Range<usize>,
        resolver: &dyn HighlightStyleResolver,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut result = Vec::new();
        let end = range.end.min(self.text_len);
        let mut cursor = range.start.min(end);
        for token in &self.tokens {
            if token.range.end <= cursor || token.range.start >= end { continue; }
            let start = token.range.start.max(cursor);
            if cursor < start {
                result.push((cursor..start, HighlightStyle::default()));
            }
            let token_end = token.range.end.min(end);
            if start < token_end {
                result.push((start..token_end, resolver.style(token.style).unwrap_or_default()));
                cursor = token_end;
            }
        }
        if cursor < end {
            result.push((cursor..end, HighlightStyle::default()));
        }
        result
    }

    fn fold_ranges(&self, _: &Rope) -> Vec<FoldRange> { Vec::new() }
}

pub fn factory() -> InputHighlighterFactory {
    Rc::new(|language| (language == "log").then(|| Box::<LogHighlighter>::default() as Box<dyn InputHighlighter>))
}

#[cfg(test)]
mod tests {
    use super::tokens;

    #[test]
    fn highlights_requested_log_forms_without_overlap() {
        let line = "2026-09-24 12:34:56 ERROR myfile.txt:110 value=1.2e-3 'warn 123' DEBUG";
        let found = tokens(line);
        let values: Vec<_> = found.iter().map(|token| (&line[token.range.clone()], token.style)).collect();
        assert!(values.contains(&("2026-09-24 12:34:56", "constant")));
        assert!(values.contains(&("ERROR", "keyword")));
        assert!(values.contains(&("myfile.txt:110", "label")));
        assert!(values.contains(&("1.2e-3", "number")));
        assert!(values.contains(&("'warn 123'", "string")));
        assert!(values.contains(&("DEBUG", "comment")));
        assert!(found.windows(2).all(|pair| pair[0].range.end <= pair[1].range.start));
    }
}
