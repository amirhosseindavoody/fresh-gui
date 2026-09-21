//! Minimal VTE-backed PTY screen for the native host.
//!
//! This is a view of remote PTY bytes, not a second editor core. Fresh still
//! owns buffers on the daemon; terminals here are portable-pty children there.

use vte::{Params, Parser, Perform};

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
const SCROLLBACK: usize = 2000;

pub struct TermScreen {
    pub cols: usize,
    pub rows: usize,
    /// Scrollback + current screen rows (each row is `cols` cells, space-padded).
    rows_data: Vec<Vec<char>>,
    cursor_col: usize,
    cursor_row: usize, // index into rows_data of the current line
    parser: Parser,
}

impl Default for TermScreen {
    fn default() -> Self {
        Self::new(DEFAULT_COLS, DEFAULT_ROWS)
    }
}

impl TermScreen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut rows_data = Vec::with_capacity(rows);
        for _ in 0..rows {
            rows_data.push(vec![' '; cols]);
        }
        Self {
            cols,
            rows,
            rows_data,
            cursor_col: 0,
            cursor_row: 0,
            parser: Parser::new(),
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        let mut parser = Parser::new();
        std::mem::swap(&mut self.parser, &mut parser);
        parser.advance(self, bytes);
        self.parser = parser;
        self.trim_scrollback();
    }

    pub fn visible_lines(&self) -> Vec<String> {
        let start = self.rows_data.len().saturating_sub(self.rows);
        self.rows_data[start..]
            .iter()
            .map(|row| {
                let s: String = row.iter().collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    pub fn visible_text(&self) -> String {
        self.visible_lines().join("\n")
    }

    fn current_line(&mut self) -> &mut Vec<char> {
        if self.cursor_row >= self.rows_data.len() {
            self.rows_data.push(vec![' '; self.cols]);
        }
        &mut self.rows_data[self.cursor_row]
    }

    fn ensure_cursor(&mut self) {
        while self.rows_data.len() <= self.cursor_row {
            self.rows_data.push(vec![' '; self.cols]);
        }
        if self.cursor_col >= self.cols {
            self.newline();
        }
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;
        while self.rows_data.len() <= self.cursor_row {
            self.rows_data.push(vec![' '; self.cols]);
        }
    }

    fn trim_scrollback(&mut self) {
        let max = SCROLLBACK + self.rows;
        if self.rows_data.len() > max {
            let drop = self.rows_data.len() - max;
            self.rows_data.drain(0..drop);
            self.cursor_row = self.cursor_row.saturating_sub(drop);
        }
    }

    fn screen_origin(&self) -> usize {
        self.rows_data.len().saturating_sub(self.rows)
    }

    fn cup(&mut self, row: usize, col: usize) {
        let origin = self.screen_origin();
        self.cursor_row = origin + row.min(self.rows - 1);
        self.cursor_col = col.min(self.cols - 1);
        self.ensure_cursor();
    }

    fn erase_line_from_cursor(&mut self) {
        self.ensure_cursor();
        let col = self.cursor_col;
        let cols = self.cols;
        let line = self.current_line();
        for c in line.iter_mut().skip(col).take(cols.saturating_sub(col)) {
            *c = ' ';
        }
    }

    fn erase_below(&mut self) {
        self.erase_line_from_cursor();
        let start = self.cursor_row + 1;
        for row in self.rows_data.iter_mut().skip(start) {
            for c in row.iter_mut() {
                *c = ' ';
            }
        }
    }

    fn erase_above(&mut self) {
        let end = self.cursor_row;
        for row in self.rows_data.iter_mut().take(end) {
            for c in row.iter_mut() {
                *c = ' ';
            }
        }
        self.ensure_cursor();
        let col = self.cursor_col;
        let line = self.current_line();
        for c in line.iter_mut().take(col + 1) {
            *c = ' ';
        }
    }

    fn erase_line_to_cursor(&mut self) {
        self.ensure_cursor();
        let col = self.cursor_col;
        let line = self.current_line();
        for c in line.iter_mut().take(col + 1) {
            *c = ' ';
        }
    }

    fn erase_line(&mut self) {
        self.ensure_cursor();
        for c in self.current_line().iter_mut() {
            *c = ' ';
        }
    }

    fn erase_display(&mut self) {
        let origin = self.screen_origin();
        for row in self.rows_data.iter_mut().skip(origin) {
            for c in row.iter_mut() {
                *c = ' ';
            }
        }
        self.cursor_row = origin;
        self.cursor_col = 0;
    }
}

fn raw_param(params: &Params, idx: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(idx)
        .and_then(|p| p.first().copied())
        .unwrap_or(default)
}

impl Perform for TermScreen {
    fn print(&mut self, c: char) {
        self.ensure_cursor();
        let col = self.cursor_col;
        {
            let line = self.current_line();
            if col < line.len() {
                line[col] = c;
            }
        }
        self.cursor_col += 1;
        if self.cursor_col >= self.cols {
            self.newline();
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.cursor_col = 0,
            b'\t' => {
                self.cursor_col = ((self.cursor_col / 8) + 1) * 8;
                if self.cursor_col >= self.cols {
                    self.newline();
                }
            }
            0x08 => self.cursor_col = self.cursor_col.saturating_sub(1),
            0x07 => {}
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        let first = |idx: usize, default: u16| {
            params
                .iter()
                .nth(idx)
                .and_then(|p| p.first().copied())
                .filter(|v| *v != 0)
                .unwrap_or(default)
        };
        match action {
            'A' => {
                let n = first(0, 1) as usize;
                let origin = self.screen_origin();
                self.cursor_row = self.cursor_row.saturating_sub(n).max(origin);
            }
            'B' => {
                let n = first(0, 1) as usize;
                self.cursor_row = (self.cursor_row + n).min(self.screen_origin() + self.rows - 1);
            }
            'C' => {
                let n = first(0, 1) as usize;
                self.cursor_col = (self.cursor_col + n).min(self.cols - 1);
            }
            'D' => {
                let n = first(0, 1) as usize;
                self.cursor_col = self.cursor_col.saturating_sub(n);
            }
            'H' | 'f' => {
                let row = first(0, 1).saturating_sub(1) as usize;
                let col = first(1, 1).saturating_sub(1) as usize;
                self.cup(row, col);
            }
            'J' => match raw_param(params, 0, 0) {
                0 => self.erase_below(),
                1 => self.erase_above(),
                _ => self.erase_display(),
            },
            'K' => match raw_param(params, 0, 0) {
                0 => self.erase_line_from_cursor(),
                1 => self.erase_line_to_cursor(),
                _ => self.erase_line(),
            },
            'm' => {}
            _ => {}
        }
    }
}

/// Map a GPUI keystroke to PTY bytes. Returns `None` for chords the host owns.
pub fn keystroke_to_bytes(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Vec<u8>> {
    let key = key.to_lowercase();
    if matches!(
        key.as_str(),
        "control" | "shift" | "alt" | "meta" | "win" | "super"
    ) {
        return None;
    }

    if ctrl {
        return match key.as_str() {
            "c" => Some(vec![0x03]),
            "d" => Some(vec![0x04]),
            "z" => Some(vec![0x1a]),
            "l" => Some(vec![0x0c]),
            "u" => Some(vec![0x15]),
            "w" | "s" | "t" | "p" | "b" | "tab" => None, // host shortcuts
            _ => None,
        };
    }

    if alt {
        return None;
    }

    match key.as_str() {
        "enter" | "return" => Some(vec![b'\r']),
        "tab" => Some(vec![b'\t']),
        "escape" => Some(vec![0x1b]),
        "backspace" => Some(vec![0x7f]),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "up" => Some(b"\x1b[A".to_vec()),
        "down" => Some(b"\x1b[B".to_vec()),
        "right" => Some(b"\x1b[C".to_vec()),
        "left" => Some(b"\x1b[D".to_vec()),
        "home" => Some(b"\x1b[H".to_vec()),
        "end" => Some(b"\x1b[F".to_vec()),
        "pageup" => Some(b"\x1b[5~".to_vec()),
        "pagedown" => Some(b"\x1b[6~".to_vec()),
        "space" => Some(vec![b' ']),
        _ => {
            if let Some(ch) = key_char.filter(|s| !s.is_empty()) {
                return Some(ch.as_bytes().to_vec());
            }
            if key.len() == 1 {
                let mut c = key.chars().next().unwrap();
                if shift {
                    c = c.to_ascii_uppercase();
                }
                Some(vec![c as u8])
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_and_newlines() {
        let mut s = TermScreen::new(40, 8);
        s.feed(b"hello\r\nworld");
        let text = s.visible_text();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
    }

    #[test]
    fn erase_below_keeps_earlier_lines() {
        let mut s = TermScreen::new(40, 8);
        s.feed(b"hello\r\nworld\x1b[0J");
        let text = s.visible_text();
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("world"), "{text}");
    }

    #[test]
    fn strips_sgr() {
        let mut s = TermScreen::new(40, 4);
        s.feed(b"\x1b[31mred\x1b[0m");
        assert!(s.visible_text().contains("red"));
        assert!(!s.visible_text().contains("[31"));
    }

    #[test]
    fn maps_ctrl_c_and_arrows() {
        assert_eq!(
            keystroke_to_bytes("c", None, true, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            keystroke_to_bytes("up", None, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(keystroke_to_bytes("w", None, true, false, false), None);
    }
}
