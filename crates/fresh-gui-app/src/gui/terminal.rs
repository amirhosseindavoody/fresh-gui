//! Terminal grid for a remote PTY.
//!
//! Bytes are parsed by [`alacritty_terminal`] (Apache-2.0), the same embeddable
//! grid Zed paints. The daemon still owns the process (`portable-pty`). This
//! module is only the screen: alternate buffer, cursor, colors, and the
//! replies a shell waits on (cursor position, device attributes, palette).

use std::cell::{Cell as StdCell, RefCell};
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

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

/// Return the character range around a terminal column for a double-click.
/// Path punctuation stays attached so `src/foo-bar.rs:12` selects as one unit.
pub fn word_bounds_at_column(line: &str, column: usize) -> Option<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let is_word = |ch: char| {
        ch.is_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-' | '~' | ':' | '\\')
    };
    if column >= chars.len() || !is_word(chars[column]) {
        return None;
    }
    let mut start = column;
    let mut end = column + 1;
    while start > 0 && is_word(chars[start - 1]) {
        start -= 1;
    }
    while end < chars.len() && is_word(chars[end]) {
        end += 1;
    }
    Some((start, end))
}

#[cfg(test)]
mod word_bounds_tests {
    use super::word_bounds_at_column;

    #[test]
    fn double_click_keeps_path_punctuation_together() {
        let line = "open ./src/my-file_2.rs:17 now";
        let at = line.find("file").unwrap();
        assert_eq!(word_bounds_at_column(line, at), Some((5, 26)));
        assert_eq!(&line[5..26], "./src/my-file_2.rs:17");
    }

    #[test]
    fn whitespace_and_punctuation_are_not_word_starts() {
        assert_eq!(word_bounds_at_column("one, two", 3), None);
        assert_eq!(word_bounds_at_column("one, two", 5), Some((5, 8)));
    }
}

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

/// One visible terminal glyph or a run of blank grid cells.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermSpan {
    pub text: String,
    /// Number of grid columns occupied, independent of glyph measurement.
    pub cells: usize,
    /// `None` means the host theme foreground or background.
    pub fg: Option<[u8; 3]>,
    pub bg: Option<[u8; 3]>,
    pub bold: bool,
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

    /// Deadline for an unfinished synchronized update (DECSET 2026).
    pub fn sync_deadline(&self) -> Option<Instant> {
        self.parser.sync_timeout().sync_timeout()
    }

    /// Release a stalled update and return any device replies it generated.
    pub fn stop_sync_if_expired(&mut self) -> Vec<u8> {
        if self.sync_deadline().is_some_and(|deadline| deadline <= Instant::now()) {
            self.parser.stop_sync(&mut self.term);
        }
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

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// DECSET mouse tracking currently enabled by the program in the PTY.
    pub fn mouse_tracking(&self) -> MouseTracking {
        let mode = *self.term.mode();
        MouseTracking {
            clicks: mode.contains(TermMode::MOUSE_REPORT_CLICK),
            drag: mode.contains(TermMode::MOUSE_DRAG),
            motion: mode.contains(TermMode::MOUSE_MOTION),
            sgr: mode.contains(TermMode::SGR_MOUSE),
            utf8: mode.contains(TermMode::UTF8_MOUSE),
        }
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

    /// Reset view state before forwarding a key. Redraw only when it changes.
    pub fn prepare_for_input(&mut self) -> bool {
        let changed = self.term.selection.is_some() || self.term.grid().display_offset() != 0;
        self.clear_selection();
        self.scroll_to_bottom();
        changed
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
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                if let Some(last) = spans.last_mut() {
                    last.cells += 1;
                } else {
                    push_cell(&mut spans, cell, content.colors, false);
                }
                continue;
            }
            if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                push_cell(&mut spans, cell, content.colors, false);
                continue;
            }
            let selected = selection.is_some_and(|range| range.contains(indexed.point));
            push_cell(&mut spans, cell, content.colors, selected);
        }
        if current.is_some() {
            rows.push(TermRow { spans });
        }
        rows
    }

    /// Viewport row text, trailing spaces removed. `row` is 0 at the top.
    pub fn line_text(&self, row: usize) -> Option<String> {
        let row = self.rows().into_iter().nth(row)?;
        let mut text = String::new();
        for span in row.spans {
            text.push_str(&span.text);
        }
        Some(text.trim_end().to_string())
    }

    /// Return row text and translate a terminal grid cell to its Unicode
    /// scalar column. A wide glyph occupies two cells but only one scalar;
    /// combining marks can add scalars without occupying cells.
    pub fn line_text_and_char_column(&self, row: usize, column: usize) -> Option<(String, usize)> {
        let row = self.rows().into_iter().nth(row)?;
        let char_column = char_column_for_cell(&row.spans, column);
        let mut text = String::new();
        for span in row.spans {
            text.push_str(&span.text);
        }
        let text = text.trim_end().to_string();
        Some((text, char_column.min(text.chars().count())))
    }

    /// Select a scalar range in a visible row, translating text positions to
    /// the terminal's cell-based selection coordinates.
    pub fn select_char_range(&mut self, row: usize, range: Range<usize>) -> bool {
        if range.is_empty() {
            return false;
        }
        let Some(row_data) = self.rows().into_iter().nth(row) else {
            return false;
        };
        let Some(start_cell) = cell_for_char_column(&row_data.spans, range.start) else {
            return false;
        };
        let Some(end_cell) = cell_for_char_column(&row_data.spans, range.end - 1) else {
            return false;
        };
        self.begin_selection(start_cell, row);
        self.update_selection(end_cell, row);
        true
    }

    #[cfg(test)]
    pub fn visible_lines(&self) -> Vec<String> {
        (0..self.rows)
            .filter_map(|row| self.line_text(row))
            .collect()
    }

    #[cfg(test)]
    pub fn visible_text(&self) -> String {
        self.visible_lines().join("\n")
    }

    /// Visible cursor as `(row, column, shape)` in the viewport.
    pub fn cursor_cell(&self) -> Option<(usize, usize, CursorShape)> {
        let content = self.term.renderable_content();
        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }
        let top = -(content.display_offset as i32);
        let row = content.cursor.point.line.0 - top;
        let col = content.cursor.point.column.0;
        if row < 0 || row as usize >= self.rows || col >= self.cols {
            return None;
        }
        Some((row as usize, col, content.cursor.shape))
    }
}

/// Convert a cell column to a scalar offset in the visible row text.
fn char_column_for_cell(spans: &[TermSpan], column: usize) -> usize {
    let mut cells = 0;
    let mut chars = 0;
    for span in spans {
        let span_chars = span.text.chars().count();
        if column < cells.saturating_add(span.cells) {
            // Blank runs contain one scalar per cell. A visible glyph span
            // represents one glyph plus optional zero-width scalars, and any
            // wide-character spacer cells still point at its first scalar.
            return chars
                + if span.text.chars().all(|ch| ch == ' ') {
                    (column - cells).min(span_chars)
                } else {
                    0
                };
        }
        cells += span.cells;
        chars += span_chars;
    }
    chars
}

/// Map one text scalar to the cell that displays it. Zero-width trailing
/// scalars stay attached to their glyph; blank runs map one cell per scalar.
fn cell_for_char_column(spans: &[TermSpan], column: usize) -> Option<usize> {
    let mut cells = 0;
    let mut chars = 0;
    for span in spans {
        let span_chars = span.text.chars().count();
        if column < chars + span_chars {
            let local = column - chars;
            if span.text.chars().all(|ch| ch == ' ') {
                return Some(cells + local.min(span.cells.saturating_sub(1)));
            }
            return Some(cells);
        }
        chars += span_chars;
        cells += span.cells;
    }
    None
}

#[cfg(test)]
mod cell_column_tests {
    use super::{TermSpan, cell_for_char_column, char_column_for_cell};

    fn span(text: &str, cells: usize) -> TermSpan {
        TermSpan {
            text: text.into(),
            cells,
            fg: None,
            bg: None,
            bold: false,
            selected: false,
        }
    }

    #[test]
    fn wide_glyph_spacer_maps_to_glyph_scalar() {
        let spans = [span("a", 1), span("界", 2), span("b", 1)];
        assert_eq!(char_column_for_cell(&spans, 0), 0);
        assert_eq!(char_column_for_cell(&spans, 1), 1);
        assert_eq!(char_column_for_cell(&spans, 2), 1);
        assert_eq!(char_column_for_cell(&spans, 3), 2);
    }

    #[test]
    fn combining_scalars_do_not_shift_later_cells() {
        let spans = [span("e\u{301}", 1), span("x", 1)];
        assert_eq!(char_column_for_cell(&spans, 0), 0);
        assert_eq!(char_column_for_cell(&spans, 1), 2);
    }

    #[test]
    fn scalar_ranges_map_back_to_cells() {
        let spans = [span("界\u{301}", 2), span("x", 1)];
        assert_eq!(cell_for_char_column(&spans, 0), Some(0));
        assert_eq!(cell_for_char_column(&spans, 1), Some(0));
        assert_eq!(cell_for_char_column(&spans, 2), Some(2));
        assert_eq!(cell_for_char_column(&spans, 3), None);
    }
}

/// Which mouse reports the PTY asked for.
///
/// `alacritty_terminal` treats 1000, 1002, and 1003 as mutually exclusive
/// flags. 1002 and 1003 still include presses and releases. 1006 (SGR) and
/// 1005 (UTF-8) choose the encoding. Private mode 1015 (urxvt) is not parsed
/// by that library, so it is not reported here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseTracking {
    clicks: bool,
    drag: bool,
    motion: bool,
    sgr: bool,
    utf8: bool,
}

impl MouseTracking {
    pub fn active(self) -> bool {
        self.clicks || self.drag || self.motion
    }

    /// Button-down motion (1002 cell motion, or 1003 any motion).
    pub fn reports_drag(self) -> bool {
        self.drag || self.motion
    }

    /// Motion with no button (1003 only).
    pub fn reports_move(self) -> bool {
        self.motion
    }

    /// Bytes to write to the PTY, or `None` when this mode does not want `kind`.
    pub fn encode(
        self,
        col: usize,
        row: usize,
        kind: TermMouseKind,
        mods: TermMouseMods,
    ) -> Option<Vec<u8>> {
        if !self.active() {
            return None;
        }
        match kind {
            TermMouseKind::Move if !self.reports_move() => return None,
            TermMouseKind::Drag(_) if !self.reports_drag() => return None,
            _ => {}
        }
        let protocol = if self.sgr {
            MouseProtocol::Sgr
        } else if self.utf8 {
            MouseProtocol::Utf8
        } else {
            MouseProtocol::Normal
        };
        Some(encode_mouse_bytes(protocol, col, row, kind, mods))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermMouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermMouseKind {
    Down(TermMouseButton),
    Up(TermMouseButton),
    Drag(TermMouseButton),
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermMouseMods {
    pub shift: bool,
    pub alt: bool,
    pub control: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MouseProtocol {
    Sgr,
    Utf8,
    Normal,
}

fn button_base(button: TermMouseButton) -> u32 {
    match button {
        TermMouseButton::Left => 0,
        TermMouseButton::Middle => 1,
        TermMouseButton::Right => 2,
    }
}

fn button_code(kind: TermMouseKind, mods: TermMouseMods) -> (u32, bool) {
    let (code, release) = match kind {
        TermMouseKind::Down(button) => (button_base(button), false),
        TermMouseKind::Up(button) => (button_base(button), true),
        TermMouseKind::Drag(button) => (button_base(button) + 32, false),
        TermMouseKind::Move => (3 + 32, false),
        TermMouseKind::ScrollUp => (64, false),
        TermMouseKind::ScrollDown => (65, false),
    };
    let mut code = code;
    if mods.shift {
        code += 4;
    }
    if mods.alt {
        code += 8;
    }
    if mods.control {
        code += 16;
    }
    (code, release)
}

fn encode_mouse_bytes(
    protocol: MouseProtocol,
    col: usize,
    row: usize,
    kind: TermMouseKind,
    mods: TermMouseMods,
) -> Vec<u8> {
    let (code, release) = button_code(kind, mods);
    // Protocols report 1-based cells.
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);
    match protocol {
        MouseProtocol::Sgr => {
            let end = if release { b'm' } else { b'M' };
            let mut out = format!("\x1b[<{code};{cx};{cy}").into_bytes();
            out.push(end);
            out
        }
        MouseProtocol::Normal => {
            // Legacy normal tracking: one byte each, biased by 32. Release is
            // button 3 and does not name which button went up. Coords cap at 223.
            let cb = legacy_button(code, release).saturating_add(32);
            let cx = (cx.min(223) as u32).saturating_add(32);
            let cy = (cy.min(223) as u32).saturating_add(32);
            vec![0x1b, b'[', b'M', cb as u8, cx as u8, cy as u8]
        }
        MouseProtocol::Utf8 => {
            let cb = legacy_button(code, release).saturating_add(32);
            let mut out = vec![0x1b, b'[', b'M'];
            push_utf8(&mut out, cb);
            push_utf8(&mut out, cx.saturating_add(32) as u32);
            push_utf8(&mut out, cy.saturating_add(32) as u32);
            out
        }
    }
}

/// X10 / UTF-8 button byte before the +32 bias. Drag's motion bit is already
/// in `code`. Release collapses to button 3 and keeps only the modifier bits.
fn legacy_button(code: u32, release: bool) -> u32 {
    if release { 3 + (code & !0b11) } else { code }
}

fn push_utf8(out: &mut Vec<u8>, value: u32) {
    let mut buf = [0u8; 4];
    let ch = char::from_u32(value).unwrap_or('\u{FFFD}');
    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
}

fn push_cell(
    spans: &mut Vec<TermSpan>,
    cell: &Cell,
    colors: &alacritty_terminal::term::color::Colors,
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
    // Blank cells can share a span: their measured glyph advance is invisible,
    // while the span still occupies their exact grid width. This keeps large
    // empty terminal areas cheap to render.
    if text == " "
        && let Some(last) = spans.last_mut()
        && last.text.starts_with(' ')
        && last.text.len() == last.cells
        && last.fg == fg
        && last.bg == bg
        && last.bold == bold
        && last.selected == selected
    {
        last.text.push(' ');
        last.cells += 1;
        return;
    }
    // Render visible glyphs one cell at a time. Grouping them into a text run
    // lets font shaping accumulate fractional or fallback-font advances, so
    // content drifts away from the terminal cursor's grid column.
    spans.push(TermSpan {
        text,
        cells: 1,
        fg,
        bg,
        bold,
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

/// ANSI yellow, bright colors and OSC palette colors can disappear on the
/// light terminal canvas. Darken only explicit foregrounds painted on a light
/// background, preserving the palette and hue on dark surfaces.
pub fn readable_light_foreground(rgb: [u8; 3], background: Option<[u8; 3]>) -> [u8; 3] {
    fn luminance(rgb: [u8; 3]) -> f32 {
        let channel = |value: u8| {
            let c = f32::from(value) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(rgb[0]) + 0.7152 * channel(rgb[1]) + 0.0722 * channel(rgb[2])
    }
    let bg = background.unwrap_or([0xf7, 0xf7, 0xf7]);
    let bg_l = luminance(bg);
    if bg_l < 0.5 || (bg_l + 0.05) / (luminance(rgb) + 0.05) >= 4.5 {
        return rgb;
    }
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..8 {
        let scale = (lo + hi) / 2.0;
        let candidate = rgb.map(|v| (f32::from(v) * scale).round() as u8);
        if (bg_l + 0.05) / (luminance(candidate) + 0.05) >= 4.5 {
            lo = scale;
        } else {
            hi = scale;
        }
    }
    rgb.map(|v| (f32::from(v) * lo).floor() as u8)
}

fn normalize_key(key: &str) -> String {
    match key.to_lowercase().as_str() {
        "arrowup" => "up".to_string(),
        "arrowdown" => "down".to_string(),
        "arrowleft" => "left".to_string(),
        "arrowright" => "right".to_string(),
        other => other.to_string(),
    }
}

/// Ctrl-Left is `\x1b[1;5D`, Alt-Right is `\x1b[1;3C`. Fish and readline use these.
fn modified_arrow(key: &str, ctrl: bool, alt: bool) -> Option<Vec<u8>> {
    let letter = match key {
        "up" => b'A',
        "down" => b'B',
        "right" => b'C',
        "left" => b'D',
        _ => return None,
    };
    let modifier = match (ctrl, alt) {
        (true, false) => b'5',
        (false, true) => b'3',
        _ => return None,
    };
    Some(vec![0x1b, b'[', b'1', b';', modifier, letter])
}

/// Bytes for a clipboard paste. Bracketed mode wraps the text so fish and
/// readline insert it literally instead of running completions.
pub fn paste_payload(text: &str, bracketed: bool) -> Vec<u8> {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let text = text.replace('\u{1b}', "");
    if !bracketed {
        return text.into_bytes();
    }
    let mut out = b"\x1b[200~".to_vec();
    out.extend(text.into_bytes());
    out.extend_from_slice(b"\x1b[201~");
    out
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
    let key = normalize_key(key);
    if matches!(
        key.as_str(),
        "control" | "shift" | "alt" | "meta" | "win" | "super"
    ) {
        return None;
    }
    let key = key.as_str();

    // Host chords. Everything else with Ctrl is a terminal control character
    // (fish Ctrl-E / Ctrl-F / Ctrl-A) or a modified cursor key.
    if ctrl && !alt && matches!(key, "w" | "s" | "t" | "p" | "b" | "tab" | "v") {
        return None;
    }
    if ctrl && !alt {
        if let Some(bytes) = modified_arrow(key, true, false) {
            return Some(bytes);
        }
        if key.len() == 1 {
            let c = key.as_bytes()[0];
            if c.is_ascii_lowercase() {
                return Some(vec![c & 0x1f]);
            }
        }
        return None;
    }

    if alt && !ctrl {
        if let Some(bytes) = modified_arrow(key, false, true) {
            return Some(bytes);
        }
        if let Some(ch) = key_char.filter(|s| !s.is_empty()) {
            let mut bytes = vec![0x1b];
            bytes.extend(ch.as_bytes());
            return Some(bytes);
        }
        if key.len() == 1 {
            let mut bytes = vec![0x1b];
            bytes.push(key.as_bytes()[0]);
            return Some(bytes);
        }
        return None;
    }

    let arrow = |normal: &[u8], app: &[u8]| {
        Some(if app_cursor {
            app.to_vec()
        } else {
            normal.to_vec()
        })
    };

    match key {
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
    fn light_theme_foregrounds_keep_ansi_yellow_readable() {
        let yellow = readable_light_foreground(ansi16(11), None);
        assert!(yellow[0] < 0xb0 && yellow[1] < 0xb0);
        assert_eq!(
            readable_light_foreground(ansi16(11), Some([0x1e; 3])),
            ansi16(11)
        );
        assert_eq!(
            readable_light_foreground([0x20, 0x30, 0x40], None),
            [0x20, 0x30, 0x40]
        );
    }

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
        assert_eq!(s.cursor_cell(), Some((0, 2, CursorShape::Block)));
        s.feed(b"\x1b[D");
        assert_eq!(s.cursor_cell(), Some((0, 1, CursorShape::Block)));
        s.feed(b"\x1b[?25l");
        assert_eq!(s.cursor_cell(), None);
        s.feed(b"\x1b[?25h\x1b[5 q");
        assert_eq!(s.cursor_cell(), Some((0, 1, CursorShape::Beam)));
    }

    #[test]
    fn cursor_stays_in_viewport_and_blank_cell_has_a_column() {
        let mut s = TermScreen::new(4, 2);
        assert_eq!(s.cursor_cell(), Some((0, 0, CursorShape::Block)));
        assert_eq!(s.rows()[0].spans[0].cells, 4);
        s.feed(b"one\r\ntwo\r\nthree");
        s.scroll_by(-1);
        assert_eq!(s.cursor_cell(), None);
    }

    #[test]
    fn wide_glyph_uses_two_grid_columns_before_the_cursor() {
        let mut s = TermScreen::new(8, 2);
        s.feed("好x".as_bytes());
        assert_eq!(s.cursor_cell(), Some((0, 3, CursorShape::Block)));
        assert_eq!(s.rows()[0].spans.iter().map(|span| span.cells).sum::<usize>(), 8);
    }

    #[test]
    fn stalled_synchronized_update_flushes_at_deadline() {
        let mut s = TermScreen::new(20, 3);
        s.feed(b"\x1b[?2026hhi\x1b[6n");
        assert!(s.sync_deadline().is_some());
        std::thread::sleep(std::time::Duration::from_millis(170));
        let reply = s.stop_sync_if_expired();
        assert!(reply.windows(6).any(|w| w == *b"\x1b[1;3R"), "{reply:?}");
        assert!(s.visible_text().contains("hi"));
        assert!(s.sync_deadline().is_none());
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
        assert_eq!(
            keystroke_to_bytes("e", None, true, false, false, false),
            Some(vec![0x05])
        );
        assert_eq!(
            keystroke_to_bytes("f", None, true, false, false, false),
            Some(vec![0x06])
        );
        assert_eq!(
            keystroke_to_bytes("right", None, false, false, false, false),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes("arrowright", None, false, false, false, false),
            Some(b"\x1b[C".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes("right", None, true, false, false, false),
            Some(b"\x1b[1;5C".to_vec())
        );
        assert_eq!(
            keystroke_to_bytes("tab", None, false, false, false, false),
            Some(vec![b'\t'])
        );
        assert_eq!(keystroke_to_bytes("v", None, true, false, false, false), None);
        assert_eq!(paste_payload("a\r\nb", false), b"a\nb".to_vec());
        assert_eq!(paste_payload("hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
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
    fn glyph_spans_preserve_grid_columns() {
        let mut screen = TermScreen::new(20, 2);
        screen.feed("a界b││".as_bytes());
        let row = &screen.rows()[0];
        let glyphs: Vec<_> = row
            .spans
            .iter()
            .take(5)
            .map(|span| (span.text.as_str(), span.cells))
            .collect();
        assert_eq!(
            glyphs,
            [("a", 1), ("界", 2), ("b", 1), ("│", 1), ("│", 1)]
        );
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

    fn report(screen: &TermScreen, col: usize, row: usize, kind: TermMouseKind) -> Option<Vec<u8>> {
        screen
            .mouse_tracking()
            .encode(col, row, kind, TermMouseMods::default())
    }

    #[test]
    fn mouse_modes_encode_press_drag_move_and_wheel() {
        let mut s = TermScreen::new(80, 24);
        assert!(!s.mouse_tracking().active());
        assert!(report(&s, 0, 0, TermMouseKind::Down(TermMouseButton::Left)).is_none());

        // 1000: clicks and wheel, normal encoding, no motion.
        s.feed(b"\x1b[?1000h");
        let tracking = s.mouse_tracking();
        assert!(tracking.active());
        assert!(!tracking.reports_drag());
        assert!(!tracking.reports_move());
        // Cell (0, 0) is 1-based 1;1, button 0, each field biased by 32.
        assert_eq!(
            report(&s, 0, 0, TermMouseKind::Down(TermMouseButton::Left)).as_deref(),
            Some(&b"\x1b[M !!"[..])
        );
        assert!(report(&s, 1, 1, TermMouseKind::Drag(TermMouseButton::Left)).is_none());
        assert!(report(&s, 1, 1, TermMouseKind::Move).is_none());
        assert_eq!(
            report(&s, 0, 0, TermMouseKind::ScrollDown).as_deref(),
            Some(&[0x1b, b'[', b'M', 65 + 32, 33, 33][..])
        );

        // 1006 SGR, still click-only until 1002/1003.
        s.feed(b"\x1b[?1006h");
        assert_eq!(
            report(&s, 9, 4, TermMouseKind::Down(TermMouseButton::Left)).as_deref(),
            Some(b"\x1b[<0;10;5M".as_slice())
        );
        assert_eq!(
            report(&s, 9, 4, TermMouseKind::Up(TermMouseButton::Left)).as_deref(),
            Some(b"\x1b[<0;10;5m".as_slice())
        );
        assert_eq!(
            report(&s, 9, 4, TermMouseKind::ScrollUp).as_deref(),
            Some(b"\x1b[<64;10;5M".as_slice())
        );

        // 1002 replaces 1000 and reports drags, not buttonless moves.
        s.feed(b"\x1b[?1002h");
        let tracking = s.mouse_tracking();
        assert!(tracking.reports_drag());
        assert!(!tracking.reports_move());
        assert_eq!(
            report(&s, 9, 4, TermMouseKind::Drag(TermMouseButton::Left)).as_deref(),
            Some(b"\x1b[<32;10;5M".as_slice())
        );
        assert!(report(&s, 9, 4, TermMouseKind::Move).is_none());

        // 1003 reports every move.
        s.feed(b"\x1b[?1003h");
        assert!(s.mouse_tracking().reports_move());
        assert_eq!(
            report(&s, 9, 4, TermMouseKind::Move).as_deref(),
            Some(b"\x1b[<35;10;5M".as_slice())
        );

        let shifted = s.mouse_tracking().encode(
            9,
            4,
            TermMouseKind::Down(TermMouseButton::Right),
            TermMouseMods {
                shift: true,
                alt: false,
                control: true,
            },
        );
        // 2 + 4 (shift) + 16 (control)
        assert_eq!(shifted.as_deref(), Some(b"\x1b[<22;10;5M".as_slice()));

        s.feed(b"\x1b[?1003l\x1b[?1006l");
        assert!(!s.mouse_tracking().active());
    }

    #[test]
    fn utf8_mouse_encodes_wide_coordinates() {
        let mut s = TermScreen::new(250, 30);
        s.feed(b"\x1b[?1000h\x1b[?1005h");
        // Column 200 → 1-based 201 + 32 = 233 = U+00E9 = UTF-8 C3 A9.
        let bytes = report(&s, 200, 0, TermMouseKind::Down(TermMouseButton::Left)).unwrap();
        assert_eq!(bytes, b"\x1b[M \xc3\xa9!");
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
