//! `alacritty_terminal` backend (ADR 0001) — the screen model behind
//! [`crate::Terminal`]. Exposes the cells/colors/cursor/modes/scrollback the
//! app needs, plus tests.

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::Processor;

pub use alacritty_terminal::vte::ansi::{Color, NamedColor};

mod state;
pub use state::ScreenState;

#[derive(Clone, Default)]
struct ResponseListener(std::sync::Arc<std::sync::Mutex<Vec<Event>>>);

impl EventListener for ResponseListener {
    fn send_event(&self, event: Event) {
        if matches!(event, Event::PtyWrite(_) | Event::TextAreaSizeRequest(_)) {
            self.0.lock().unwrap().push(event);
        }
    }
}

/// A snapshot of a grid cell (owned, so the caller doesn't borrow the term).
#[derive(Clone)]
pub struct CellView {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub inverse: bool,
    pub bold: bool,
    /// SGR 2: faint text (Claude Code's suggested prompt, hints).
    pub dim: bool,
    /// Width recorded by the VT grid, rather than guessed from Unicode ranges.
    pub wide: bool,
    pub wide_spacer: bool,
}

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A terminal screen backed by `alacritty_terminal`.
pub struct ATerm {
    term: Term<ResponseListener>,
    responses: ResponseListener,
    processor: Processor,
    cols: usize,
    rows: usize,
}

impl ATerm {
    /// Alacritty's grid needs two columns for a double-width cell. Reflowing
    /// such a cell into one column never consumes it and keeps allocating rows
    /// (alacritty/alacritty#7697), including when a pane is briefly collapsed.
    pub const MIN_COLS: u16 = 2;

    pub(crate) fn normalize_size(cols: u16, rows: u16) -> (u16, u16) {
        (cols.max(Self::MIN_COLS), rows.max(1))
    }

    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let (cols, rows) = Self::normalize_size(cols, rows);
        let dims = Dims {
            cols: cols as usize,
            rows: rows as usize,
        };
        let config = Config {
            scrolling_history: scrollback,
            ..Default::default()
        };
        let responses = ResponseListener::default();
        Self {
            term: Term::new(config, &dims, responses.clone()),
            responses,
            processor: Processor::new(),
            cols: cols as usize,
            rows: rows as usize,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.expire_synchronized_output(std::time::Instant::now());
        self.processor.advance(&mut self.term, bytes);
    }

    /// The VT parser buffers a synchronized frame until ESU or this deadline.
    /// The host must wake at the deadline even when the producer goes idle.
    pub(crate) fn synchronized_output_deadline(&self) -> Option<std::time::Instant> {
        self.processor.sync_timeout().sync_timeout()
    }

    pub(crate) fn expire_synchronized_output(&mut self, now: std::time::Instant) -> bool {
        if !self
            .synchronized_output_deadline()
            .is_some_and(|deadline| deadline <= now)
        {
            return false;
        }
        self.processor.stop_sync(&mut self.term);
        true
    }

    /// A cursor query must observe writes buffered by a synchronized update.
    pub(crate) fn flush_synchronized_output(&mut self) {
        self.processor.stop_sync(&mut self.term);
    }

    /// Return protocol replies to the PTY, never to the rendered screen.
    pub(crate) fn take_responses(&self, cell_width: u16, cell_height: u16) -> Vec<String> {
        let size = WindowSize {
            num_lines: self.rows as u16,
            num_cols: self.cols as u16,
            cell_width,
            cell_height,
        };
        std::mem::take(&mut *self.responses.0.lock().unwrap())
            .into_iter()
            .filter_map(|event| match event {
                Event::PtyWrite(text) => Some(text),
                Event::TextAreaSizeRequest(format) => Some(format(size)),
                _ => None,
            })
            .collect()
    }

    pub fn size(&self) -> (u16, u16) {
        (self.rows as u16, self.cols as u16)
    }

    /// The character at a viewport cell (space for empty cells, and for cells
    /// outside the screen: a pointer in a pane's padding maps past the last
    /// row).
    pub fn cell_char(&self, row: u16, col: u16) -> char {
        if row as usize >= self.rows || col as usize >= self.cols {
            return ' ';
        }
        let off = self.term.grid().display_offset() as i32;
        shown(self.term.grid()[Line(row as i32 - off)][Column(col as usize)].c)
    }

    /// A row's text, right-trimmed.
    pub fn line_text(&self, row: u16) -> String {
        let text: String = (0..self.cols)
            .map(|col| self.cell_char(row, col as u16))
            .collect();
        text.trim_end().to_string()
    }

    /// Total buffer lines (scrollback + screen).
    pub fn total_lines(&self) -> usize {
        self.term.grid().total_lines()
    }

    /// Lines of history above the viewport.
    pub fn history_size(&self) -> usize {
        self.total_lines().saturating_sub(self.rows)
    }

    /// Text of buffer line `b` counted from the oldest (0), right-trimmed.
    /// Wide-character spacer cells are skipped, so CJK reads as typed.
    pub fn line_text_abs(&self, b: usize) -> String {
        let text: String = self
            .line_chars_abs(b)
            .into_iter()
            .map(|(_, c, _)| c)
            .collect();
        text.trim_end().to_string()
    }

    /// Characters of buffer line `b` with their starting column and cell
    /// width (2 for wide characters); spacer cells are skipped.
    pub fn line_chars_abs(&self, b: usize) -> Vec<(u16, char, u16)> {
        let line = b as i32 - self.history_size() as i32;
        let row = &self.term.grid()[Line(line)];
        (0..self.cols)
            .filter_map(|c| {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    return None;
                }
                let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                    2
                } else {
                    1
                };
                Some((c as u16, shown(cell.c), width))
            })
            .collect()
    }

    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as u16, point.column.0 as u16)
    }

    pub fn cursor_position(&self) -> (u16, u16) {
        self.cursor()
    }

    /// True when the cursor should not be drawn.
    pub fn hide_cursor(&self) -> bool {
        !self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    pub fn cell(&self, row: u16, col: u16) -> Option<CellView> {
        if row as usize >= self.rows || col as usize >= self.cols {
            return None;
        }
        let off = self.term.grid().display_offset() as i32;
        let cell = &self.term.grid()[Line(row as i32 - off)][Column(col as usize)];
        Some(CellView {
            ch: shown(cell.c),
            fg: cell.fg,
            bg: cell.bg,
            inverse: cell.flags.contains(Flags::INVERSE),
            bold: cell.flags.contains(Flags::BOLD),
            dim: cell.flags.contains(Flags::DIM),
            wide: cell.flags.contains(Flags::WIDE_CHAR),
            wide_spacer: cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER),
        })
    }

    /// A drag can start on the trailing cell of a wide character (its right
    /// half). Pull the start back one column so the whole character is copied,
    /// matching a forward drag that ends on the spacer.
    fn selection_start(&self, row: u16, col: u16) -> u16 {
        if col > 0 && self.cell(row, col).is_some_and(|cell| cell.wide_spacer) {
            col - 1
        } else {
            col
        }
    }

    /// Text between two cells (inclusive), right-trimmed per line.
    pub fn contents_between(&self, r1: u16, c1: u16, r2: u16, c2: u16) -> String {
        let mut out = String::new();
        for row in r1..=r2 {
            let start = if row == r1 {
                self.selection_start(row, c1)
            } else {
                0
            };
            let end = if row == r2 {
                c2.saturating_add(1).min(self.cols as u16)
            } else {
                self.cols as u16
            };
            let mut line = String::new();
            for col in start..end.max(start) {
                if let Some(cell) = self.cell(row, col) {
                    if !cell.wide_spacer {
                        line.push(cell.ch);
                    }
                }
            }
            out.push_str(line.trim_end());
            if row != r2 {
                out.push('\n');
            }
        }
        out
    }

    /// Selection text with SGR colour codes (for "Copy as ANSI Sequence").
    pub fn contents_ansi_between(&self, r1: u16, c1: u16, r2: u16, c2: u16) -> String {
        let mut out = String::new();
        for row in r1..=r2 {
            let start = if row == r1 {
                self.selection_start(row, c1)
            } else {
                0
            };
            let end = if row == r2 {
                c2.saturating_add(1).min(self.cols as u16)
            } else {
                self.cols as u16
            };
            let mut line = String::new();
            let (mut cur_fg, mut cur_bg) = (None, None);
            for col in start..end.max(start) {
                let Some(cell) = self.cell(row, col) else {
                    continue;
                };
                if cell.wide_spacer {
                    continue;
                }
                if cell.fg != Color::Named(NamedColor::Foreground) && Some(cell.fg) != cur_fg {
                    line.push_str(&sgr(cell.fg, false));
                    cur_fg = Some(cell.fg);
                }
                if cell.bg != Color::Named(NamedColor::Background) && Some(cell.bg) != cur_bg {
                    line.push_str(&sgr(cell.bg, true));
                    cur_bg = Some(cell.bg);
                }
                line.push(cell.ch);
            }
            if cur_fg.is_some() || cur_bg.is_some() {
                line.push_str("\x1b[0m");
            }
            out.push_str(line.trim_end());
            if row != r2 {
                out.push('\n');
            }
        }
        out
    }

    /// The main screen's last `max_lines` rows (scrollback and screen, down to
    /// the last non-blank row) as text with SGR colours and attributes, for
    /// showing a pane's contents again in a new session. Soft-wrapped rows
    /// are joined, so the text reflows at whatever width it is replayed at.
    ///
    /// With a full-screen program on the alternate screen (vim, less), the
    /// shell output underneath is read; the alternate screen is kept as it
    /// was, so this can run while the program is still drawing.
    pub fn snapshot_ansi(&mut self, max_lines: usize) -> String {
        self.with_main_screen(|term| term.main_screen_ansi(max_lines))
    }

    /// Like [`Self::snapshot_ansi`], but also returns the captured main-screen
    /// window: the index (0 = oldest buffer row) of its last non-blank row, or
    /// `None` when the captured text is empty. Inline images are saved against
    /// this index so they re-anchor to the same content on restore.
    pub fn snapshot_scrollback(&mut self, max_lines: usize) -> (String, Option<usize>) {
        self.with_main_screen(|term| term.main_screen_ansi_last(max_lines))
    }

    /// Run `f` over the main screen even while an alternate screen (vim, less)
    /// is active, restoring the alternate screen afterwards.
    fn with_main_screen<R>(&mut self, f: impl FnOnce(&Self) -> R) -> R {
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            return f(self);
        }
        let alt = self.read_screen();
        self.term.swap_alt();
        let out = f(self);
        // Switching back clears the alternate screen: write it back.
        self.term.swap_alt();
        self.write_screen(&alt);
        out
    }

    /// Index (0 = oldest buffer row) of the last non-blank main-screen row,
    /// matching the window [`Self::snapshot_ansi`] captures. Restored inline
    /// images anchor to it. `None` when the screen is blank.
    pub fn last_content_index(&self) -> Option<usize> {
        self.main_screen_last_index()
    }

    /// The last non-blank main-screen row as an index from the oldest buffer
    /// row (0-based), matching the window `snapshot_ansi` emits.
    fn main_screen_last_index(&self) -> Option<usize> {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        let rows: Vec<Line> = (-history..self.rows as i32).map(Line).collect();
        rows.iter()
            .rposition(|&l| !(0..self.cols).all(|c| blank_cell(&grid[l][Column(c)])))
    }

    fn main_screen_ansi(&self, max_lines: usize) -> String {
        self.main_screen_ansi_last(max_lines).0
    }

    fn main_screen_ansi_last(&self, max_lines: usize) -> (String, Option<usize>) {
        let grid = self.term.grid();
        let history = grid.history_size() as i32;
        // Buffer rows as alacritty lines: -history (oldest) ..= rows - 1.
        let rows: Vec<Line> = (-history..self.rows as i32).map(Line).collect();
        let Some(last) = rows
            .iter()
            .rposition(|&l| !(0..self.cols).all(|c| blank_cell(&grid[l][Column(c)])))
        else {
            return (String::new(), None);
        };
        let first = (last + 1).saturating_sub(max_lines);
        let mut out = String::new();
        let mut attrs = String::new();
        for &line in &rows[first..=last] {
            let row = &grid[line];
            let wraps = row[Column(self.cols - 1)].flags.contains(Flags::WRAPLINE);
            // A wrapped row continues on the next one: keep its full width.
            let end = if wraps {
                self.cols
            } else {
                (0..self.cols)
                    .rposition(|c| !blank_cell(&row[Column(c)]))
                    .map_or(0, |c| c + 1)
            };
            for c in 0..end {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                let want = sgr_params(cell.fg, cell.bg, cell.flags);
                if want != attrs {
                    out.push_str("\x1b[0");
                    out.push_str(&want);
                    out.push('m');
                    attrs = want;
                }
                out.push(shown(cell.c));
                if let Some(marks) = cell.zerowidth() {
                    out.extend(marks);
                }
            }
            if !wraps {
                if !attrs.is_empty() {
                    out.push_str("\x1b[0m");
                    attrs.clear();
                }
                out.push_str("\r\n");
            }
        }
        if !attrs.is_empty() {
            out.push_str("\x1b[0m");
        }
        (out, Some(last))
    }

    /// Current scrollback offset (lines scrolled up from the bottom).
    pub fn scroll_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Lines of history above the viewport (for a scrollbar).
    pub fn scrollback_len(&self) -> usize {
        self.term
            .grid()
            .total_lines()
            .saturating_sub(self.term.grid().screen_lines())
    }

    /// Scroll the viewport `n` lines back from the bottom.
    pub fn set_scrollback(&mut self, n: usize) {
        let current = self.term.grid().display_offset() as i32;
        self.term.scroll_display(Scroll::Delta(n as i32 - current));
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = Self::normalize_size(cols, rows);
        if cols as usize == self.cols && rows as usize == self.rows {
            return;
        }
        self.cols = cols as usize;
        self.rows = rows as usize;
        self.term.resize(Dims {
            cols: self.cols,
            rows: self.rows,
        });
    }

    pub fn application_cursor(&self) -> bool {
        self.term.mode().contains(TermMode::APP_CURSOR)
    }

    /// A full-screen program (vim, less, man) is on the alternate screen and
    /// alternate scroll (DECSET 1007, on by default) is set: the wheel should
    /// send it arrow keys rather than scroll a scrollback it does not have.
    /// A full-screen program (vim, less) has switched to the alternate screen.
    pub fn alternate_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn alternate_scroll(&self) -> bool {
        self.term
            .mode()
            .contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Kitty keyboard protocol flags: bit 0 disambiguate, bit 1 event types,
    /// bit 2 alternate keys, bit 3 all keys, bit 4 associated text.
    pub fn kitty_flags(&self) -> u8 {
        let mode = self.term.mode();
        let mut flags = 0;
        if mode.contains(TermMode::DISAMBIGUATE_ESC_CODES) {
            flags |= 1;
        }
        if mode.contains(TermMode::REPORT_EVENT_TYPES) {
            flags |= 2;
        }
        if mode.contains(TermMode::REPORT_ALTERNATE_KEYS) {
            flags |= 4;
        }
        if mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC) {
            flags |= 8;
        }
        if mode.contains(TermMode::REPORT_ASSOCIATED_TEXT) {
            flags |= 16;
        }
        flags
    }

    /// Kitty keyboard protocol: disambiguate escape codes (CSI-u).
    pub fn kitty_disambiguate(&self) -> bool {
        self.kitty_flags() & 1 != 0
    }

    /// Kitty keyboard protocol: report key event types (press/repeat/release).
    pub fn kitty_report_event_types(&self) -> bool {
        self.kitty_flags() & 2 != 0
    }

    /// Mouse reporting mode the application asked for, if any.
    ///
    /// Returns `(report_clicks, report_motion, report_drag, sgr_encoding)`.
    pub fn mouse_reporting(&self) -> Option<(bool, bool, bool, bool)> {
        let mode = self.term.mode();
        if !mode.intersects(TermMode::MOUSE_MODE) {
            return None;
        }
        Some((
            // Drag and all-motion protocols include button presses/releases;
            // Alacritty stores these protocols as mutually exclusive bits.
            mode.intersects(TermMode::MOUSE_MODE),
            mode.contains(TermMode::MOUSE_MOTION),
            mode.contains(TermMode::MOUSE_DRAG),
            mode.contains(TermMode::SGR_MOUSE),
        ))
    }
}

/// A cell's character as drawn. alacritty writes `\t` into the first cell a
/// tab skips (so its own copy keeps the tab); it is blank on screen, and
/// drawn or replayed as a tab it would shift the rest of the row.
fn shown(c: char) -> char {
    if c == '\t' {
        ' '
    } else {
        c
    }
}

/// True for a cell that carries nothing worth restoring: a space with the
/// default background and no visual attribute. `snapshot_ansi` trims these.
fn blank_cell(c: &alacritty_terminal::term::cell::Cell) -> bool {
    shown(c.c) == ' '
        && c.bg == Color::Named(NamedColor::Background)
        && !c.flags.intersects(Flags::INVERSE | Flags::UNDERLINE)
        && c.zerowidth().is_none()
}

/// The SGR parameters (after a reset) for a cell's colours and attributes.
fn sgr_params(fg: Color, bg: Color, flags: Flags) -> String {
    let mut out = String::new();
    for (flag, code) in [
        (Flags::BOLD, "1"),
        (Flags::DIM, "2"),
        (Flags::ITALIC, "3"),
        (Flags::UNDERLINE, "4"),
        (Flags::INVERSE, "7"),
        (Flags::STRIKEOUT, "9"),
    ] {
        if flags.contains(flag) {
            out.push(';');
            out.push_str(code);
        }
    }
    for (color, is_bg) in [(fg, false), (bg, true)] {
        let default = if is_bg {
            NamedColor::Background
        } else {
            NamedColor::Foreground
        };
        if color != Color::Named(default) {
            // `sgr` yields "\x1b[<params>m": keep the parameters.
            let one = sgr(color, is_bg);
            if let Some(params) = one.strip_prefix("\x1b[").and_then(|r| r.strip_suffix('m')) {
                out.push(';');
                out.push_str(params);
            }
        }
    }
    out
}

/// An SGR fragment for a colour (`bg` selects 48/49 instead of 38/39).
fn sgr(c: Color, bg: bool) -> String {
    match c {
        Color::Named(NamedColor::Foreground) => "\x1b[39m".into(),
        Color::Named(NamedColor::Background) => "\x1b[49m".into(),
        Color::Named(n) if (n as usize) < 16 => {
            let base = if bg { 40 } else { 30 };
            let idx = n as usize;
            // Bright colours use the 90/100 range.
            let code = if idx >= 8 {
                base + 60 + (idx - 8)
            } else {
                base + idx
            };
            format!("\x1b[{code}m")
        }
        Color::Indexed(i) => {
            let sel = if bg { 48 } else { 38 };
            format!("\x1b[{sel};5;{i}m")
        }
        Color::Spec(rgb) => {
            let sel = if bg { 48 } else { 38 };
            format!("\x1b[{sel};2;{};{};{}m", rgb.r, rgb.g, rgb.b)
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_grids_keep_room_for_wide_characters() {
        // Check construction first: on an unpatched engine this fails before
        // the dangerous reflow, rather than letting a regression exhaust CI.
        let mut term = ATerm::new(1, 0, 100);
        assert_eq!(term.size(), (1, 2));
        term.resize(40, 6);
        term.process("中文 🦀\r\n".as_bytes());
        let before = term.snapshot_ansi(100);
        for _ in 0..100 {
            term.resize(1, 6);
            assert_eq!(term.size(), (6, 2));
            term.resize(40, 6);
            assert_eq!(term.snapshot_ansi(100), before);
        }
        term.resize(0, 0);
        assert_eq!(term.size(), (1, 2));
    }

    #[test]
    fn an_unfinished_synchronized_frame_is_flushed_at_its_deadline() {
        let mut term = ATerm::new(40, 5, 100);
        term.process(b"old\x1b[?2026h\rnew");
        assert_eq!(term.line_text(0), "old");
        let deadline = term.synchronized_output_deadline().unwrap();
        assert!(!term.expire_synchronized_output(deadline - std::time::Duration::from_nanos(1)));
        assert_eq!(term.line_text(0), "old");
        assert!(term.expire_synchronized_output(deadline));
        assert_eq!(term.line_text(0), "new");
        assert_eq!(term.synchronized_output_deadline(), None);
        assert!(!term.expire_synchronized_output(deadline));
        term.process(b"\x1b[?2026l\r\nnext");
        assert_eq!(term.line_text(1), "next");
    }

    #[test]
    fn a_completed_synchronized_frame_has_no_stale_deadline() {
        let mut term = ATerm::new(40, 5, 100);
        term.process(b"\x1b[?2026hfirst\x1b[?2026l");
        assert_eq!(term.line_text(0), "first");
        assert_eq!(term.synchronized_output_deadline(), None);
        term.process(b"\x1b[?2026h\rsecond");
        assert_eq!(term.line_text(0), "first");
        term.process(b"\x1b[?2026l");
        assert_eq!(term.line_text(0), "second");
        assert_eq!(term.synchronized_output_deadline(), None);
    }

    fn replay(snapshot: &str, cols: u16) -> ATerm {
        let mut term = ATerm::new(cols, 10, 100);
        term.process(snapshot.as_bytes());
        term
    }

    #[test]
    fn snapshots_keep_colours_and_reflow_wrapped_lines() {
        let mut term = ATerm::new(10, 4, 100);
        term.process(b"one\r\n\x1b[1;31mred\x1b[0m plain\r\n0123456789abcdef\r\n$ ");
        let snap = term.snapshot_ansi(1000);
        assert!(snap.contains("\x1b[0;1;31mred\x1b[0m plain"), "{snap:?}");
        // 16 characters wrapped at 10 columns come back as one line.
        assert!(snap.contains("0123456789abcdef\r\n"), "{snap:?}");
        let wide = replay(&snap, 20);
        assert_eq!(wide.line_text(2), "0123456789abcdef");
        assert_eq!(wide.line_text(1), "red plain");
        let red = wide.cell(1, 0).unwrap();
        assert!(red.bold && red.fg == Color::Named(NamedColor::Red));
    }

    #[test]
    fn snapshots_include_scrollback_up_to_a_limit() {
        let mut term = ATerm::new(10, 3, 100);
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }
        let all = term.snapshot_ansi(1000);
        assert!(all.starts_with("line 0\r\n") && all.ends_with("line 19\r\n"));
        let last = term.snapshot_ansi(5);
        assert!(last.starts_with("line 15\r\n"), "{last:?}");
        assert_eq!(ATerm::new(10, 3, 100).snapshot_ansi(10), "");
    }

    #[test]
    fn cursor_column_lands_after_cjk_wide_characters() {
        // The IME candidate window is placed at the cursor cell, so its column
        // must be the grid column past wide characters, not a character count.
        let mut term = ATerm::new(20, 3, 100);
        term.process("目录:".as_bytes());
        // Two wide characters (4 columns) plus the ASCII ':' == column 5.
        assert_eq!(term.cursor_position(), (0, 5));
        term.process("中".as_bytes());
        // The wide character advances two columns.
        assert_eq!(term.cursor_position(), (0, 7));
    }

    #[test]
    fn snapshots_read_the_shell_output_under_a_full_screen_program() {
        let mut term = ATerm::new(20, 3, 100);
        term.process(b"$ vim notes\r\n\x1b[?1049h\x1b[H\x1b[2Jvim screen");
        let snap = term.snapshot_ansi(100);
        assert!(
            snap.contains("$ vim notes") && !snap.contains("vim screen"),
            "{snap:?}"
        );
        assert!(term.alternate_scroll(), "still on the alternate screen");
        // The program's screen and cursor are left as they were.
        assert_eq!(term.line_text(0), "vim screen");
        assert_eq!(term.cursor_position(), (0, 10));
        term.process(b"!");
        assert_eq!(term.line_text(0), "vim screen!");
    }

    #[test]
    fn snapshots_keep_wide_characters() {
        let mut term = ATerm::new(10, 3, 100);
        term.process("中文 ok\r\n".as_bytes());
        assert_eq!(term.snapshot_ansi(10), "中文 ok\r\n");
    }

    #[test]
    fn tab_cells_read_as_blank() {
        // `ls` aligns columns with tabs; the tab character in the skipped
        // cell was drawn as a wide glyph and pushed the row out of line.
        let mut term = ATerm::new(40, 3, 100);
        term.process(b"ab\tX\tY\r\n");
        assert_eq!(term.cell(0, 2).unwrap().ch, ' ');
        assert_eq!(term.cell_char(0, 2), ' ');
        assert_eq!(term.line_text(0), "ab      X       Y");
        assert_eq!(term.line_text_abs(term.history_size()), "ab      X       Y");
        assert_eq!(term.contents_between(0, 0, 0, 39), "ab      X       Y");
        let snap = term.snapshot_ansi(10);
        assert!(!snap.contains('\t'), "{snap:?}");
        // Replayed after a tab stop change, the columns stay where they were.
        let mut replay = ATerm::new(40, 3, 100);
        replay.process(b"\x1b[3g"); // clear all tab stops
        replay.process(snap.as_bytes());
        assert_eq!(replay.line_text(0), "ab      X       Y");
    }

    #[test]
    fn cells_past_the_screen_read_as_blank() {
        // A pointer below the last row (the pane's bottom padding) used to
        // index the grid out of bounds and crash the app.
        let mut term = ATerm::new(10, 3, 100);
        for i in 0..20 {
            term.process(format!("line {i}\r\n").as_bytes());
        }
        term.set_scrollback(5);
        for row in [3, 4, 50, u16::MAX] {
            assert_eq!(term.line_text(row), "", "row {row}");
            assert_eq!(term.cell_char(row, 0), ' ');
        }
        assert_eq!(term.cell_char(0, 10), ' ');
        assert!(!term.line_text(0).is_empty());
    }

    #[test]
    fn wheel_goes_to_full_screen_programs_on_the_alternate_screen() {
        let mut term = ATerm::new(20, 3, 100);
        assert!(!term.alternate_scroll(), "the main screen scrolls back");
        term.process(b"\x1b[?1049h");
        assert!(term.alternate_scroll(), "vim/less: on by default");
        term.process(b"\x1b[?1007l");
        assert!(!term.alternate_scroll(), "a program can turn it off");
        term.process(b"\x1b[?1007h\x1b[?1049l");
        assert!(!term.alternate_scroll(), "back on the main screen");
    }

    #[test]
    fn mouse_tracking_protocols_include_clicks() {
        for (protocol, motion, drag) in [
            (1000, false, false),
            (1002, false, true),
            (1003, true, false),
        ] {
            let mut term = ATerm::new(20, 3, 100);
            assert_eq!(term.mouse_reporting(), None);
            term.process(format!("\x1b[?{protocol}h\x1b[?1006h").as_bytes());
            assert_eq!(term.mouse_reporting(), Some((true, motion, drag, true)));
            term.process(format!("\x1b[?{protocol}l").as_bytes());
            assert_eq!(term.mouse_reporting(), None);
        }
    }

    #[test]
    fn parses_plain_text() {
        let mut term = ATerm::new(20, 3, 100);
        term.process(b"hello");
        assert_eq!(term.line_text(0), "hello");
        assert_eq!(term.cursor(), (0, 5));
    }

    #[test]
    fn handles_newline_and_cursor() {
        let mut term = ATerm::new(20, 3, 100);
        term.process(b"one\r\ntwo");
        assert_eq!(term.line_text(0), "one");
        assert_eq!(term.line_text(1), "two");
        assert_eq!(term.cursor(), (1, 3));
    }

    #[test]
    fn scrollback_shifts_viewport() {
        let mut t = ATerm::new(10, 3, 100);
        t.process(b"a\r\nb\r\nc\r\nd\r\ne");
        assert_eq!(t.line_text(0), "c");
        assert_eq!(t.line_text(2), "e");
        assert_eq!(t.total_lines(), 5);
        assert_eq!(t.history_size(), 2);
        assert_eq!(t.line_text_abs(0), "a");
        assert_eq!(t.line_text_abs(4), "e");
        t.set_scrollback(2);
        assert_eq!(t.line_text(0), "a");
        assert_eq!(t.line_text(2), "c");
    }

    #[test]
    fn output_keeps_a_scrolled_back_viewport_anchored() {
        let mut t = ATerm::new(10, 3, 100);
        t.process(b"a\r\nb\r\nc\r\nd\r\ne");
        t.set_scrollback(2);
        // A redraw in place (spinner) moves no line into history.
        t.process(b"\re*");
        assert_eq!(t.scroll_offset(), 2);
        assert_eq!(t.line_text(0), "a");
        // New lines push into history; the offset grows to keep "a" on top.
        t.process(b"\r\nf\r\ng");
        assert_eq!(t.scroll_offset(), 4);
        assert_eq!(t.line_text(0), "a");
        // The pane re-applies that offset next frame without moving the view.
        t.set_scrollback(t.scroll_offset());
        assert_eq!(t.line_text(0), "a");
    }

    #[test]
    fn absolute_line_text_skips_wide_spacers() {
        let mut t = ATerm::new(20, 3, 100);
        t.process("ab目录c".as_bytes());
        assert_eq!(t.line_text_abs(0), "ab目录c");
        let cells = t.line_chars_abs(0);
        assert_eq!(
            &cells[..5],
            &[
                (0, 'a', 1),
                (1, 'b', 1),
                (2, '目', 2),
                (4, '录', 2),
                (6, 'c', 1)
            ]
        );
    }

    #[test]
    fn ansi_copy_carries_colours() {
        let mut t = ATerm::new(10, 2, 10);
        t.process(b"\x1b[31mred\x1b[0m");
        let s = t.contents_ansi_between(0, 0, 0, 2);
        assert!(s.contains("\x1b[31m"), "{s:?}");
        assert!(s.contains("red"), "{s:?}");
    }

    #[test]
    fn tracks_modes() {
        let mut term = ATerm::new(20, 3, 100);
        term.process(b"\x1b[?1h"); // DECCKM (application cursor)
        assert!(term.application_cursor());
        term.process(b"\x1b[?2004h"); // bracketed paste
        assert!(term.bracketed_paste());
    }

    #[test]
    fn copying_stops_at_inclusive_selection_end() {
        let mut term = ATerm::new(30, 3, 100);
        term.process(b"prefix selected suffix\r\nsecond line");
        assert_eq!(term.contents_between(0, 7, 0, 14), "selected");
        assert_eq!(term.contents_between(0, 7, 1, 5), "selected suffix\nsecond");
        assert_eq!(term.contents_between(0, 7, 0, 7), "s");
        assert_eq!(term.contents_ansi_between(0, 7, 0, 14), "selected");
    }

    #[test]
    fn copying_cjk_does_not_insert_spacer_cells() {
        let mut term = ATerm::new(30, 3, 100);
        term.process("目录/file.txt suffix".as_bytes());
        assert_eq!(term.contents_between(0, 0, 0, 12), "目录/file.txt");
        assert_eq!(term.contents_ansi_between(0, 0, 0, 12), "目录/file.txt");
        // A drag that starts on the trailing half of a wide character copies
        // the whole character, so both drag directions agree.
        assert_eq!(term.contents_between(0, 1, 0, 2), "目录");
        assert_eq!(term.contents_between(0, 1, 0, 1), "目");
        assert_eq!(term.contents_between(0, 3, 0, 3), "录");
        assert_eq!(term.contents_ansi_between(0, 1, 0, 2), "目录");
    }
}
