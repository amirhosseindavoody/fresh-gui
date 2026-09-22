//! Terminal grid for a remote PTY.
//!
//! Bytes are parsed by [`alacritty_terminal`] (Apache-2.0), the same embeddable
//! grid Zed paints. The daemon still owns the process (`portable-pty`). This
//! module is only the screen: alternate buffer, cursor, colors, and the
//! replies a shell waits on (cursor position, device attributes, palette).

use std::cell::{Cell as StdCell, RefCell};
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Color, CursorShape, NamedColor, Processor, Rgb};

const DEFAULT_COLS: usize = 80;
const DEFAULT_ROWS: usize = 24;
/// Rough cell size of `text_sm` monospace, used when the host asks for pixels.
const CELL_WIDTH_PX: u16 = 8;
const CELL_HEIGHT_PX: u16 = 18;

const DEFAULT_FG: [u8; 3] = [0xe6, 0xe6, 0xe6];
const DEFAULT_BG: [u8; 3] = [0x1e, 0x1e, 0x1e];
/// OSC 52 copy larger than this is dropped. A remote program must not be
/// able to push an unbounded blob onto the host clipboard.
const OSC52_MAX_BYTES: usize = 256 * 1024;

struct Shared {
    replies: RefCell<Vec<u8>>,
    clipboard: RefCell<Vec<String>>,
    cols: StdCell<u16>,
    rows: StdCell<u16>,
}

struct ReplyProxy {
    shared: Rc<Shared>,
}

impl EventListener for ReplyProxy {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.shared.replies.borrow_mut().extend(text.into_bytes()),
            Event::ClipboardStore(_, text) => {
                if text.len() <= OSC52_MAX_BYTES {
                    self.shared.clipboard.borrow_mut().push(text);
                }
            }
            Event::ColorRequest(index, format) => {
                let text = format(rgb_for_index(index));
                self.shared.replies.borrow_mut().extend(text.into_bytes());
            }
            Event::TextAreaSizeRequest(format) => {
                let text = format(WindowSize {
                    num_lines: self.shared.rows.get(),
                    num_cols: self.shared.cols.get(),
                    cell_width: CELL_WIDTH_PX,
                    cell_height: CELL_HEIGHT_PX,
                });
                self.shared.replies.borrow_mut().extend(text.into_bytes());
            }
            _ => {}
        }
    }
}

struct TermSize {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for TermSize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// One run of cells that share a color and cursor flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermSpan {
    pub text: String,
    /// `None` means the host theme foreground or background.
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
    pub cursor: bool,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermRow {
    pub spans: Vec<TermSpan>,
}

pub struct TermScreen {
    pub cols: usize,
    pub rows: usize,
    term: Term<ReplyProxy>,
    parser: Processor,
    shared: Rc<Shared>,
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
        let shared = Rc::new(Shared {
            replies: RefCell::new(Vec::new()),
            clipboard: RefCell::new(Vec::new()),
            cols: StdCell::new(cols as u16),
            rows: StdCell::new(rows as u16),
        });
        // Fish 4 and other TUIs ask for the kitty keyboard protocol.
        // OSC 52 copy is on; paste/read stays off (alacritty's OnlyCopy).
        let config = Config {
            scrolling_history: 2_000,
            kitty_keyboard: true,
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &TermSize {
                columns: cols,
                screen_lines: rows,
            },
            ReplyProxy {
                shared: Rc::clone(&shared),
            },
        );
        Self {
            cols,
            rows,
            term,
            parser: Processor::new(),
            shared,
        }
    }

    /// Ingest PTY bytes. Returns replies the host must write back (device
    /// attributes, cursor position, palette). ConPTY and fish both wait on these.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.parser.advance(&mut self.term, bytes);
        self.shared.replies.borrow_mut().drain(..).collect()
    }

    /// Text an OSC 52 copy sequence asked the host to place on the clipboard.
    /// Sequences over [`OSC52_MAX_BYTES`] are dropped. OSC 52 paste is not
    /// answered: a program in the shell must not read the host clipboard.
    pub fn take_clipboard_stores(&self) -> Vec<String> {
        self.shared.clipboard.borrow_mut().drain(..).collect()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.shared.cols.set(cols as u16);
        self.shared.rows.set(rows as u16);
        self.term.resize(TermSize {
            columns: cols,
            screen_lines: rows,
        });
    }

    pub fn app_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Scroll the viewport into history. Negative is older (page up).
    pub fn scroll_by(&mut self, lines: i32) {
        if lines == 0 {
            return;
        }
        let scroll = if lines < 0 {
            Scroll::Delta(lines.saturating_neg())
        } else {
            Scroll::Delta(-lines)
        };
        self.term.scroll_display(scroll);
    }

    pub fn scroll_page(&mut self, older: bool) {
        self.term.scroll_display(if older {
            Scroll::PageUp
        } else {
            Scroll::PageDown
        });
    }

    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Start a drag selection at a viewport cell `(column, row)`.
    pub fn begin_selection(&mut self, col: usize, row: usize) {
        let point = self.viewport_point(col, row);
        self.term.selection = Some(Selection::new(SelectionType::Simple, point, Side::Left));
    }

    /// Move the end of the current drag selection.
    pub fn update_selection(&mut self, col: usize, row: usize) {
        let point = self.viewport_point(col, row);
        if let Some(selection) = self.term.selection.as_mut() {
            selection.update(point, Side::Right);
        } else {
            self.begin_selection(col, row);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }

    pub fn selection_is_empty(&self) -> bool {
        self.term.selection.as_ref().is_none_or(Selection::is_empty)
    }

    /// Selected text, without trailing empty selections.
    pub fn selection_text(&self) -> Option<String> {
        self.term
            .selection_to_string()
            .filter(|text| !text.is_empty())
    }

    fn viewport_point(&self, col: usize, row: usize) -> Point {
        let col = col.min(self.cols.saturating_sub(1));
        let row = row.min(self.rows.saturating_sub(1));
        let offset = self.term.grid().display_offset();
        alacritty_terminal::term::viewport_to_point(offset, Point::new(row, Column(col)))
    }

    pub fn rows(&self) -> Vec<TermRow> {
        let content = self.term.renderable_content();
        let cursor_on = content.cursor.shape != CursorShape::Hidden;
        let cursor = content.cursor.point;
        let selection = content.selection;
        let mut rows = Vec::new();
        let mut current: Option<Line> = None;
        let mut spans: Vec<TermSpan> = Vec::new();

        for indexed in content.display_iter {
            if current != Some(indexed.point.line) {
                if current.is_some() {
                    rows.push(TermRow { spans });
                    spans = Vec::new();
                }
                current = Some(indexed.point.line);
            }
            let cell = indexed.cell;
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            let is_cursor = cursor_on && indexed.point == cursor;
            let selected = selection.is_some_and(|range| range.contains(indexed.point));
            push_cell(&mut spans, cell, content.colors, is_cursor, selected);
        }
        if current.is_some() {
            rows.push(TermRow { spans });
        }
        rows
    }

    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        self.rows()
            .into_iter()
            .map(|row| {
                let mut text = String::new();
                for span in row.spans {
                    text.push_str(&span.text);
                }
                text.trim_end().to_string()
            })
            .collect()
    }

    #[cfg(test)]
    pub fn visible_text(&self) -> String {
        self.visible_lines().join("\n")
    }

    /// Visible cursor as `(row, column)` from the top-left of the screen.
    #[cfg(test)]
    pub fn cursor_cell(&self) -> Option<(usize, usize)> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let top = -(content.display_offset as i32);
        let row = content.cursor.point.line.0 - top;
        if row < 0 {
            return None;
        }
        Some((row as usize, content.cursor.point.column.0))
    }
}

fn push_cell(
    spans: &mut Vec<TermSpan>,
    cell: &Cell,
    colors: &alacritty_terminal::term::color::Colors,
    cursor: bool,
    selected: bool,
) {
    let mut text = String::new();
    let ch = if cell.flags.contains(Flags::HIDDEN) || cell.c == '\0' {
        ' '
    } else {
        cell.c
    };
    text.push(ch);
    if let Some(extra) = cell.zerowidth() {
        text.extend(extra.iter().copied());
    }

    let mut fg = resolve_color(cell.fg, colors, true);
    let mut bg = resolve_color(cell.bg, colors, false);
    if cell.flags.contains(Flags::INVERSE) {
        let fg_c = fg.unwrap_or(DEFAULT_FG);
        let bg_c = bg.unwrap_or(DEFAULT_BG);
        fg = Some(bg_c);
        bg = Some(fg_c);
    }
    let bold = cell.flags.contains(Flags::BOLD);
    if let Some(last) = spans.last_mut()
        && last.fg == fg
        && last.bg == bg
        && last.bold == bold
        && last.cursor == cursor
        && last.selected == selected
    {
        last.text.push_str(&text);
        return;
    }
    spans.push(TermSpan {
        text,
        fg,
        bg,
        bold,
        cursor,
        selected,
    });
}

fn resolve_color(
    color: Color,
    colors: &alacritty_terminal::term::color::Colors,
    foreground: bool,
) -> Option<[u8; 3]> {
    match color {
        Color::Named(NamedColor::Foreground) if foreground => None,
        Color::Named(NamedColor::Background) if !foreground => None,
        Color::Named(named) => colors[named]
            .map(rgb_array)
            .or_else(|| named_default(named)),
        Color::Spec(rgb) => Some(rgb_array(rgb)),
        Color::Indexed(index) => colors[usize::from(index)]
            .map(rgb_array)
            .or(Some(indexed_color(index))),
    }
}

fn rgb_array(rgb: Rgb) -> [u8; 3] {
    [rgb.r, rgb.g, rgb.b]
}

fn rgb_for_index(index: usize) -> Rgb {
    let [r, g, b] = match index {
        0..=15 => ansi16(index as u8),
        256 => DEFAULT_FG,
        257 => DEFAULT_BG,
        n if n < 256 => indexed_color(n as u8),
        _ => DEFAULT_FG,
    };
    Rgb { r, g, b }
}

fn named_default(named: NamedColor) -> Option<[u8; 3]> {
    let index = named as usize;
    if index < 16 {
        Some(ansi16(index as u8))
    } else {
        None
    }
}

fn ansi16(index: u8) -> [u8; 3] {
    const TABLE: [[u8; 3]; 16] = [
        [0x1e, 0x1e, 0x1e],
        [0xf4, 0x47, 0x47],
        [0x3f, 0xb9, 0x50],
        [0xd2, 0x99, 0x22],
        [0x55, 0x99, 0xdd],
        [0xd2, 0x6a, 0xc2],
        [0x39, 0xc5, 0xcf],
        [0xd0, 0xd0, 0xd0],
        [0x80, 0x80, 0x80],
        [0xff, 0x6b, 0x68],
        [0x6b, 0xd4, 0x6b],
        [0xf0, 0xc6, 0x74],
        [0x79, 0xb8, 0xff],
        [0xff, 0x7a, 0xd9],
        [0x6e, 0xe7, 0xe7],
        [0xff, 0xff, 0xff],
    ];
    TABLE[usize::from(index.min(15))]
}

fn indexed_color(index: u8) -> [u8; 3] {
    if index < 16 {
        return ansi16(index);
    }
    if index >= 232 {
        let value = 8 + 10 * (index - 232);
        return [value, value, value];
    }
    let cube = index - 16;
    let r = cube / 36;
    let g = (cube % 36) / 6;
    let b = cube % 6;
    let level = |n: u8| if n == 0 { 0 } else { 55 + 40 * n };
    [level(r), level(g), level(b)]
}

/// Map a GPUI keystroke to PTY bytes. Returns `None` for chords the host owns.
///
/// `app_cursor` is the terminal's application-cursor mode (fish, vim, less).
pub fn keystroke_to_bytes(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
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

    let arrow = |normal: &[u8], app: &[u8]| {
        Some(if app_cursor {
            app.to_vec()
        } else {
            normal.to_vec()
        })
    };

    match key.as_str() {
        "enter" | "return" => Some(vec![b'\r']),
        "tab" => Some(vec![b'\t']),
        "escape" => Some(vec![0x1b]),
        "backspace" => Some(vec![0x7f]),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "up" => arrow(b"\x1b[A", b"\x1bOA"),
        "down" => arrow(b"\x1b[B", b"\x1bOB"),
        "right" => arrow(b"\x1b[C", b"\x1bOC"),
        "left" => arrow(b"\x1b[D", b"\x1bOD"),
        "home" => arrow(b"\x1b[H", b"\x1bOH"),
        "end" => arrow(b"\x1b[F", b"\x1bOF"),
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
        assert!(text.contains("hello"), "{text}");
        assert!(text.contains("world"), "{text}");
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
    fn cursor_is_visible_after_text() {
        let mut s = TermScreen::new(80, 24);
        s.feed(b"ab");
        assert_eq!(s.cursor_cell(), Some((0, 2)));
        assert!(
            s.rows()
                .iter()
                .flat_map(|row| row.spans.iter())
                .any(|span| span.cursor),
            "screen rows should mark the cursor cell"
        );
    }

    #[test]
    fn cursor_position_report_answers_conpty_dsr() {
        let mut s = TermScreen::new(80, 24);
        let reply = s.feed(b"\x1b[6n");
        assert!(
            reply.windows(6).any(|w| w == *b"\x1b[1;1R"),
            "reply {reply:?}"
        );
        assert!(s.visible_text().trim().is_empty());

        s.feed(b"ab");
        let reply = s.feed(b"\x1b[6n");
        assert!(
            reply.windows(6).any(|w| w == *b"\x1b[1;3R"),
            "reply {reply:?}"
        );
    }

    #[test]
    fn device_attributes_get_a_reply() {
        let mut s = TermScreen::new(80, 24);
        let primary = s.feed(b"\x1b[c");
        assert!(primary.starts_with(b"\x1b["), "{primary:?}");
        assert!(primary.contains(&b'c'), "{primary:?}");
        let secondary = s.feed(b"\x1b[>c");
        assert!(secondary.starts_with(b"\x1b["), "{secondary:?}");
        assert!(s.visible_text().trim().is_empty());
    }

    #[test]
    fn alternate_screen_restores_the_primary_buffer() {
        let mut s = TermScreen::new(20, 6);
        s.feed(b"primary");
        s.feed(b"\x1b[?1049h");
        s.feed(b"\x1b[2J\x1b[H");
        s.feed(b"altscreen");
        let alt = s.visible_text();
        assert!(alt.contains("altscreen"), "{alt}");
        assert!(!alt.contains("primary"), "{alt}");
        s.feed(b"\x1b[?1049l");
        let primary = s.visible_text();
        assert!(primary.contains("primary"), "{primary}");
        assert!(!primary.contains("altscreen"), "{primary}");
    }

    #[test]
    fn maps_ctrl_c_and_arrows() {
        assert_eq!(
            keystroke_to_bytes("c", None, true, false, false, false),
            Some(vec![0x03])
        );
        assert_eq!(
            keystroke_to_bytes("up", None, false, false, false, false),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes("up", None, false, false, false, true),
            Some(b"\x1bOA".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes("w", None, true, false, false, false),
            None
        );
    }

    #[test]
    fn drag_selection_covers_the_cells_and_clears() {
        let mut s = TermScreen::new(40, 6);
        s.feed(b"hello world");
        s.begin_selection(0, 0);
        assert!(s.selection_is_empty());
        s.update_selection(4, 0);
        assert_eq!(s.selection_text().as_deref(), Some("hello"));
        assert!(
            s.rows()
                .iter()
                .flat_map(|row| row.spans.iter())
                .any(|span| span.selected && span.text.contains('h')),
            "selected cells should be marked"
        );
        s.clear_selection();
        assert!(s.selection_text().is_none());
        assert!(s.selection_is_empty());
    }

    #[test]
    fn osc52_copy_is_captured_and_paste_is_not_answered() {
        let mut s = TermScreen::new(40, 6);
        let reply = s.feed(b"\x1b]52;c;aGVsbG8=\x07");
        assert!(reply.is_empty(), "{reply:?}");
        assert_eq!(s.take_clipboard_stores(), vec!["hello".to_string()]);
        assert!(s.visible_text().trim().is_empty());

        let query = s.feed(b"\x1b]52;c;?\x07");
        assert!(
            query.is_empty(),
            "OSC 52 paste must not read the host clipboard: {query:?}"
        );
        assert!(s.take_clipboard_stores().is_empty());
    }

    #[test]
    fn osc52_copy_over_the_size_cap_is_dropped() {
        let mut s = TermScreen::new(20, 4);
        let raw = vec![b'a'; OSC52_MAX_BYTES + 1];
        let encoded = base64_encode(&raw);
        let mut seq = b"\x1b]52;c;".to_vec();
        seq.extend(encoded);
        seq.push(0x07);
        s.feed(&seq);
        assert!(s.take_clipboard_stores().is_empty());
    }

    fn base64_encode(bytes: &[u8]) -> Vec<u8> {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = Vec::new();
        let mut i = 0;
        while i + 3 <= bytes.len() {
            let n = (u32::from(bytes[i]) << 16)
                | (u32::from(bytes[i + 1]) << 8)
                | u32::from(bytes[i + 2]);
            out.push(TABLE[((n >> 18) & 63) as usize]);
            out.push(TABLE[((n >> 12) & 63) as usize]);
            out.push(TABLE[((n >> 6) & 63) as usize]);
            out.push(TABLE[(n & 63) as usize]);
            i += 3;
        }
        if i < bytes.len() {
            let mut n = u32::from(bytes[i]) << 16;
            if i + 1 < bytes.len() {
                n |= u32::from(bytes[i + 1]) << 8;
            }
            out.push(TABLE[((n >> 18) & 63) as usize]);
            out.push(TABLE[((n >> 12) & 63) as usize]);
            if i + 1 < bytes.len() {
                out.push(TABLE[((n >> 6) & 63) as usize]);
            } else {
                out.push(b'=');
            }
            out.push(b'=');
        }
        out
    }
}
