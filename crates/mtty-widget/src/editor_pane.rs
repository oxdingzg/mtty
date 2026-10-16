//! The editor pane (ADR 0034, phase E2): a `mtty-editor` document shown on the
//! same monospace cell grid as a terminal and drawn by the same renderer.
//!
//! Everything here works in cells (rows and columns of the pane's text area)
//! and is free of window and GPU code, so it is unit-tested; the host turns
//! the cell quads into pixels and feeds the spans to `TermRenderer`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use mtty_editor::large::LargeFile;
use mtty_editor::{layout, motion, Document, Highlight, Motion, Range, Selection, Syntax};
use mtty_render::Span;
use mtty_ui::input::KeyKind;

/// Files larger than this open in view mode: read in place, a window of
/// lines at a time, so memory stays small whatever the size. Switching such
/// a file to editing loads all of it (the user is told what that costs).
pub const MAX_PANE_BYTES: u64 = 64 << 20;

/// Lines (and at most bytes) a view-mode window holds around the screen.
const WINDOW_LINES: usize = 3000;
const WINDOW_BYTES: usize = 8 << 20;

/// A file's identity on disk, for noticing edits made outside mtty. Length
/// and modification time together: either changing means the file changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskStamp {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// The current stamp of `path`, or `None` when it cannot be read (missing,
/// permission denied).
pub fn disk_stamp(path: &Path) -> Option<DiskStamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(DiskStamp {
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// What a remote pane should do about a freshly probed remote stamp. Derived
/// purely from the stamps and whether the pane has unsaved edits, so it is
/// unit-tested apart from ssh and the UI.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemotePollOutcome {
    /// Nothing to do: the probe failed or found the same stamp (or this is
    /// the pane's first observation, used to seed the baseline).
    Ignore,
    /// The file changed while the pane is unmodified: read it and reload.
    Reload,
    /// The file changed while the pane has unsaved edits: keep them, silently.
    KeepLocal,
}

/// Decide what a remote poll means for a pane whose last observed stamp is
/// `known`, given the stamp `current` the background probe just found and
/// whether the document has unsaved edits.
pub fn remote_poll_outcome(
    known: Option<DiskStamp>,
    current: Option<DiskStamp>,
    modified: bool,
) -> RemotePollOutcome {
    let (Some(known), Some(current)) = (known, current) else {
        // A failed or missing probe is a silent no-op; the first successful
        // probe only seeds the baseline, it never reloads.
        return RemotePollOutcome::Ignore;
    };
    if known == current {
        return RemotePollOutcome::Ignore;
    }
    if modified {
        RemotePollOutcome::KeepLocal
    } else {
        RemotePollOutcome::Reload
    }
}

/// A file in view mode: `doc` holds lines `base..` of it (a read-only
/// window that moves as the view scrolls).
pub struct LargeWindow {
    pub file: Arc<LargeFile>,
    pub base: usize,
    /// A line asked for before indexing reached it (a restored session):
    /// gone to once it is known.
    pub pending_line: Option<usize>,
}

/// A rectangle on the cell grid, in text-area coordinates (the gutter is at
/// negative columns from the host's point of view, see [`EditorPane::draw`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRect {
    pub row: usize,
    pub col: usize,
    pub width: usize,
}

/// What the host should paint for this pane, all in cells of the pane.
pub struct EditorDraw {
    /// Glyph rows for `TermRenderer`; columns include the gutter.
    pub rows: Vec<Vec<Span>>,
    /// Selected cells.
    pub selection: Vec<CellRect>,
    /// The current line's band (behind the selection).
    pub current_line: Option<usize>,
    /// A line to flash after a jump to it (row in the text area).
    pub flash_line: Option<usize>,
    /// Where carets go (row, col including the gutter); a caret is a bar.
    pub carets: Vec<(usize, usize)>,
    /// Width of the gutter in cells.
    pub gutter: usize,
    /// Diagnostic underlines and their severity (1 error … 4 hint).
    pub underlines: Vec<(CellRect, u8)>,
    /// Changed lines of a pending agent proposal (ADR 0040, A1).
    pub proposal: Vec<CellRect>,
    /// Deleted text is drawn in ghost rows, outside the document and selection.
    pub deletions: Vec<CellRect>,
}

/// A pending agent edit (ADR 0040, A1): applied as one undoable transaction
/// and tinted until the user accepts (typing, saving, another edit) or rejects
/// (`undo`).
#[derive(Clone, Debug)]
pub struct Proposal {
    pub label: Option<String>,
    /// Changed file lines (0-based).
    pub lines: Vec<usize>,
    /// Old text, anchored before a line in the resulting document.
    pub deleted: Vec<(usize, String)>,
    /// Ghost rows skipped before the current top document line.
    scroll_offset: usize,
    /// The document revision right after it was applied; any change ends it.
    revision: u64,
}

/// A language server's diagnostic in this pane's text (ADR 0034, E5): a
/// char range, as of when it was reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneDiagnostic {
    pub from: usize,
    pub to: usize,
    /// 1 error, 2 warning, 3 information, 4 hint.
    pub severity: u8,
    pub message: String,
}

/// Colours the pane draws with (taken from the terminal theme).
#[derive(Clone, Copy)]
pub struct Palette {
    pub fg: (u8, u8, u8),
    pub gutter: (u8, u8, u8),
    pub gutter_current: (u8, u8, u8),
}

impl Palette {
    /// A syntax highlight's colour (One Dark, as the built-in editor used).
    pub fn highlight(&self, h: Highlight) -> (u8, u8, u8) {
        match h {
            Highlight::Keyword => (0xc6, 0x78, 0xdd),
            Highlight::String | Highlight::Heading => (0x98, 0xc3, 0x79),
            Highlight::Escape | Highlight::Operator => (0x56, 0xb6, 0xc2),
            Highlight::Comment => (0x7f, 0x84, 0x8e),
            Highlight::Number | Highlight::Constant | Highlight::Attribute => (0xd1, 0x9a, 0x66),
            Highlight::Function | Highlight::Link => (0x61, 0xaf, 0xef),
            Highlight::Type => (0xe5, 0xc0, 0x7b),
            Highlight::Property | Highlight::Variable | Highlight::Tag => (0xe0, 0x6c, 0x75),
            Highlight::Punctuation => self.fg,
        }
    }
}

/// A command a key press maps to. Kept separate from applying it so the
/// keymap is tested on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Move(Motion, bool),
    Backspace,
    Delete,
    DeleteWordBackward,
    DeleteToLineStart,
    Newline,
    Indent,
    Outdent,
    SelectAll,
    SelectLine,
    SelectNextOccurrence,
    /// Select every occurrence of the selection (or the word at the caret).
    SelectAllOccurrences,
    /// A caret at the end of each selected line.
    CursorsAtLineEnds,
    AddCursor(bool),
    /// Ask for a line to go to (the host shows the prompt).
    GoToLine,
    /// Ask for a symbol in the file (the host shows the outline picker).
    GoToSymbol,
    /// Collapse the fold at the caret.
    Fold,
    /// Expand the fold at the caret.
    Unfold,
    /// Collapse or expand the fold at the caret.
    ToggleFold,
    /// Collapse every fold.
    FoldAll,
    /// Expand every fold.
    UnfoldAll,
    /// Find with the replace field open (the host's Find bar).
    FindReplace,
    /// Ask the language server for completions (Ctrl+Space).
    Complete,
    /// Go to the definition of the symbol at the caret (F12).
    GoToDefinition,
    /// The next (or previous) diagnostic (F8 / ⇧F8).
    NextProblem(bool),
    Undo,
    Redo,
    Escape,
}

/// The macOS text conventions (ADR 0034): ⌥ moves by word, ⌘ by line or
/// document, ⇧ extends. On Linux/Windows Ctrl plays ⌥'s word role and Home/End
/// cover line moves. Multiple cursors and find follow VS Code: ⇧⌘L every
/// occurrence, ⇧⌥I carets at line ends, ⌃G go to line, ⌥⌘F replace (Ctrl+G
/// and Ctrl+H elsewhere). Returns `None` for keys the pane leaves to text
/// input or to the app (⌘S, ⌘W, …). `key` is the key without modifiers, so
/// ⇧⌥I is `i`, not the dead key ⌥ types.
pub fn keymap(key: KeyKind, shift: bool, alt: bool, cmd: bool, ctrl: bool) -> Option<Command> {
    let mac = cfg!(target_os = "macos");
    // The "word" modifier and the "line/document" modifier per platform.
    let word = if mac { alt } else { ctrl };
    let line = mac && cmd;
    let primary = if mac { cmd } else { ctrl };
    let page = 20;
    Some(match key {
        // Before ⌘↑/⌘↓, which would otherwise take ⌥⌘↑/⌥⌘↓.
        KeyKind::Up if mac && alt && cmd => Command::AddCursor(false),
        KeyKind::Down if mac && alt && cmd => Command::AddCursor(true),
        KeyKind::Up if !mac && alt && ctrl => Command::AddCursor(false),
        KeyKind::Down if !mac && alt && ctrl => Command::AddCursor(true),
        KeyKind::Left if line => Command::Move(Motion::LineStart, shift),
        KeyKind::Right if line => Command::Move(Motion::LineEnd, shift),
        KeyKind::Up if line => Command::Move(Motion::DocStart, shift),
        KeyKind::Down if line => Command::Move(Motion::DocEnd, shift),
        KeyKind::Left if word => Command::Move(Motion::WordLeft, shift),
        KeyKind::Right if word => Command::Move(Motion::WordRight, shift),
        KeyKind::Left => Command::Move(Motion::Left, shift),
        KeyKind::Right => Command::Move(Motion::Right, shift),
        KeyKind::Up => Command::Move(Motion::Up, shift),
        KeyKind::Down => Command::Move(Motion::Down, shift),
        KeyKind::Home if ctrl && !mac => Command::Move(Motion::DocStart, shift),
        KeyKind::End if ctrl && !mac => Command::Move(Motion::DocEnd, shift),
        KeyKind::Home => Command::Move(Motion::LineStart, shift),
        KeyKind::End => Command::Move(Motion::LineEnd, shift),
        KeyKind::PageUp => Command::Move(Motion::PageUp(page), shift),
        KeyKind::PageDown => Command::Move(Motion::PageDown(page), shift),
        KeyKind::Backspace if line => Command::DeleteToLineStart,
        KeyKind::Backspace if word => Command::DeleteWordBackward,
        KeyKind::Backspace => Command::Backspace,
        KeyKind::Delete => Command::Delete,
        KeyKind::Enter if !primary => Command::Newline,
        KeyKind::Tab if shift => Command::Outdent,
        KeyKind::Tab if !primary => Command::Indent,
        KeyKind::Escape => Command::Escape,
        KeyKind::F(12) if !cmd && !ctrl && !alt => Command::GoToDefinition,
        KeyKind::F(8) if !cmd && !ctrl && !alt => Command::NextProblem(!shift),
        KeyKind::Char(' ') if ctrl && !cmd && !alt => Command::Complete,
        KeyKind::Char(c) if shift && alt && !cmd && !ctrl && c.eq_ignore_ascii_case(&'i') => {
            Command::CursorsAtLineEnds
        }
        KeyKind::Char(c) if mac && ctrl && !cmd && !alt && c.eq_ignore_ascii_case(&'g') => {
            Command::GoToLine
        }
        KeyKind::Char(c) if mac && cmd && alt && !ctrl && c.eq_ignore_ascii_case(&'f') => {
            Command::FindReplace
        }
        KeyKind::Char(c) if mac && cmd && alt && !ctrl && c == '[' => Command::Fold,
        KeyKind::Char(c) if mac && cmd && alt && !ctrl && c == ']' => Command::Unfold,
        KeyKind::Char(c) if !mac && ctrl && shift && !alt && (c == '[' || c == '{') => {
            Command::Fold
        }
        KeyKind::Char(c) if !mac && ctrl && shift && !alt && (c == ']' || c == '}') => {
            Command::Unfold
        }
        KeyKind::Char(c) if primary && !alt => match c.to_ascii_lowercase() {
            'a' => Command::SelectAll,
            'l' if shift => Command::SelectAllOccurrences,
            'l' => Command::SelectLine,
            'd' => Command::SelectNextOccurrence,
            'g' if !mac && !shift => Command::GoToLine,
            'h' if !mac && !shift => Command::FindReplace,
            'r' if !shift => Command::GoToSymbol,
            'z' if shift => Command::Redo,
            'z' => Command::Undo,
            'y' if !mac => Command::Redo,
            _ => return None,
        },
        _ => return None,
    })
}

/// A file open in an editor pane.
/// A file edited over ssh: `path` on `dest` (ADR 0034, E3). The pane's
/// [`EditorPane::path`] holds the remote path, so syntax detection and the
/// title work unchanged; reads and writes go through `mtty_ui::ssh` on
/// the host, never the local filesystem.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RemoteRef {
    pub dest: String,
    pub path: String,
}

pub struct EditorPane {
    pub id: String,
    /// Single-surface Markdown writing UI (source remains in `doc`).
    pub markdown: Option<crate::live_markdown::LiveMarkdown>,
    pub doc: Document,
    pub path: PathBuf,
    /// `Some` when this pane edits a file over ssh.
    pub remote: Option<RemoteRef>,
    /// A background remote save is running.
    pub saving: bool,
    /// Close once the running remote save succeeds (vim `:wq`).
    pub quit_after_save: bool,
    /// First visible line and cell column of the text area.
    pub scroll_line: usize,
    pub scroll_col: usize,
    /// Text-area size in cells, from the last layout.
    pub rows: usize,
    pub cols: usize,
    /// Programmatic navigation can arrive before the first real layout.
    pending_horizontal_reveal: bool,
    /// A drag selection is in progress (the anchor stays put).
    pub dragging: bool,
    /// Closing with unsaved changes was asked once; the next close discards.
    pub close_armed: bool,
    /// The parse tree for highlighting, for a built-in language.
    pub syntax: Option<Syntax>,
    /// View mode for a large file (see [`MAX_PANE_BYTES`]).
    pub large: Option<LargeWindow>,
    /// The language server's diagnostics, sorted by position.
    pub diagnostics: Vec<PaneDiagnostic>,
    /// The file's stamp when it was last read or written; a mismatch on a
    /// later poll means something else changed it.
    pub disk: Option<DiskStamp>,
    /// A remote file's stamp as of the last background probe (ADR 0034, E3);
    /// `None` until the first probe seeds it.
    pub remote_disk: Option<DiskStamp>,
    /// Bytes last loaded or successfully saved, for content-based remote saves.
    pub remote_original: Option<Vec<u8>>,
    /// A background stamp probe for this remote pane is running.
    pub remote_polling: bool,
    /// The file vanished on disk and the user has been told once.
    pub missing_warned: bool,
    /// Foldable ranges, recomputed when the text or syntax changes.
    folds: Vec<(usize, usize)>,
    /// The document revision `folds` was computed at.
    folds_revision: u64,
    /// Start lines the user collapsed; folding one hides the lines after it.
    folded: std::collections::HashSet<usize>,
    /// Vim state when `editor-vim` is on (ADR 0034, E6); `None` is off.
    pub vim: Option<mtty_editor::vim::Vim>,
    /// A pending agent edit (ADR 0040, A1), tinted until accepted or rejected.
    pub proposal: Option<Proposal>,
    last_click: Option<(Instant, usize, u8)>,
    /// A line to flash after a jump to it, and when the flash started.
    flash: Option<(usize, Instant)>,
}

/// How long a jump-to-line highlight stays.
const FLASH: Duration = Duration::from_millis(1500);

impl EditorPane {
    /// Open `path` (UTF-8 text) in a pane: in view mode above
    /// [`MAX_PANE_BYTES`].
    pub fn open(id: String, path: &Path) -> Result<Self, String> {
        let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
        if meta.len() > MAX_PANE_BYTES {
            return Self::open_view(id, path);
        }
        Self::open_for_editing(id, path)
    }

    /// Load all of `path` for editing, whatever its size.
    pub fn open_for_editing(id: String, path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let doc = Document::from_bytes(&bytes).map_err(|e| e.to_string())?;
        Ok(Self::with_doc(id, path.to_path_buf(), doc))
    }

    /// Open `path` in view mode: read-only, read in place.
    pub fn open_view(id: String, path: &Path) -> Result<Self, String> {
        let file = LargeFile::open(path).map_err(|e| e.to_string())?;
        let (text, _) = file.read_lines(0, WINDOW_LINES, WINDOW_BYTES);
        let mut pane = Self::with_doc(id, path.to_path_buf(), Document::from_text(&text));
        pane.syntax = None;
        pane.markdown = None;
        pane.large = Some(LargeWindow {
            file,
            base: 0,
            pending_line: None,
        });
        Ok(pane)
    }

    /// Read-only view of a large file.
    pub fn is_view_only(&self) -> bool {
        self.large.is_some()
    }

    /// The file line `doc`'s first line is (0 outside view mode).
    pub fn base(&self) -> usize {
        self.large.as_ref().map_or(0, |l| l.base)
    }

    /// Lines in the file (in view mode, those indexed so far).
    pub fn total_lines(&self) -> usize {
        match &self.large {
            Some(l) => l.file.line_count(),
            None => self.doc.rope().len_lines(),
        }
    }

    /// Show the window starting at file line `base`, keeping the view and
    /// the caret where they are in the file.
    fn rewindow(&mut self, base: usize) {
        let Some(large) = &self.large else {
            return;
        };
        let file = large.file.clone();
        let old_base = large.base;
        let rope = self.doc.rope();
        let head = self.doc.selection().primary().head;
        let caret_line = old_base + rope.char_to_line(head);
        let caret_col = head - rope.line_to_char(rope.char_to_line(head));
        let top = old_base + self.scroll_line;
        let (text, _) = file.read_lines(base, WINDOW_LINES, WINDOW_BYTES);
        self.doc = Document::from_text(&text);
        self.large = Some(LargeWindow {
            file,
            base,
            pending_line: None,
        });
        let rope = self.doc.rope();
        let last = motion::last_line(rope);
        self.scroll_line = top.saturating_sub(base).min(last);
        let caret = if caret_line >= base && caret_line - base <= last {
            let line = caret_line - base;
            let len = layout::content_len(rope.line(line));
            rope.line_to_char(line) + caret_col.min(len)
        } else {
            rope.line_to_char(self.scroll_line)
        };
        self.doc.set_selection(Selection::cursor(caret));
    }

    /// In view mode, move the window when the screen nears its edge.
    fn keep_window(&mut self) {
        let Some(large) = &self.large else {
            return;
        };
        let lines = self.doc.rope().len_lines();
        let total = large.file.line_count();
        let margin = self.rows * 2;
        let near_top = self.scroll_line < margin && large.base > 0;
        let near_end = self.scroll_line + self.rows + margin > lines && large.base + lines < total;
        if near_top || near_end {
            let top = large.base + self.scroll_line;
            self.rewindow(top.saturating_sub(WINDOW_LINES / 3));
        }
    }

    /// Put the caret at the start of file line `line` and scroll it into
    /// view (a third from the top when the view has to move).
    pub fn go_to_line(&mut self, line: usize) {
        if let Some(large) = &mut self.large {
            // Past what indexing has reached: go there when it gets there.
            if line != usize::MAX && line >= large.file.line_count() && !large.file.indexed() {
                large.pending_line = Some(line);
                return;
            }
            large.pending_line = None;
        }
        let line = line.min(self.total_lines().saturating_sub(1));
        // Land on the line with a brief highlight, so a jump from elsewhere
        // (Go to Line, a symbol, an agent's `app.edit`) is visible.
        self.flash = Some((line, Instant::now()));
        if let Some(large) = &self.large {
            let lines = self.doc.rope().len_lines();
            if line < large.base || line >= large.base + lines {
                self.rewindow(line.saturating_sub(WINDOW_LINES / 3));
            }
        }
        let local = (line - self.base()).min(motion::last_line(self.doc.rope()));
        let at = self.doc.rope().line_to_char(local);
        self.doc.set_selection(Selection::cursor(at));
        if local < self.scroll_line || local >= self.scroll_line + self.rows {
            self.scroll_line = local.saturating_sub(self.rows / 3);
        }
        self.keep_window();
    }

    /// Go to file line `line` and cell column `col` (both 0-based; a column
    /// past the line's end stops at its end), as Go to Line does.
    pub fn go_to_line_col(&mut self, line: usize, col: usize) {
        self.go_to_line(line);
        if col == 0
            || self
                .large
                .as_ref()
                .is_some_and(|l| l.pending_line.is_some())
        {
            return;
        }
        let rope = self.doc.rope();
        let local = rope.char_to_line(self.doc.selection().primary().head);
        let offset = layout::offset_at_col(rope.line(local), col, self.doc.tab_width());
        let at = rope.line_to_char(local) + offset;
        self.doc.set_selection(Selection::cursor(at));
        self.reveal_cursor();
    }

    /// Select file bytes `start..end` (a search hit in view mode), on the
    /// hit's first line.
    pub fn select_bytes(&mut self, start: u64, end: u64) {
        let Some(large) = &self.large else {
            return;
        };
        let file = large.file.clone();
        let line = file.line_of(start);
        let line_start = file.line_start(line);
        self.go_to_line(line);
        let rope = self.doc.rope();
        let local = rope.char_to_line(self.doc.selection().primary().head);
        let slice = rope.line(local);
        let content = layout::content_len(slice);
        let to_char = |byte: u64| {
            let byte = (byte.saturating_sub(line_start) as usize).min(slice.len_bytes());
            slice.byte_to_char(byte).min(content)
        };
        let base = rope.line_to_char(local);
        let (a, b) = (base + to_char(start), base + to_char(end));
        self.doc.set_selection(Selection::single(Range::new(a, b)));
        self.reveal_cursor();
    }

    /// Open a remote file's bytes in an editable pane (ADR 0034, E3). No
    /// local stamp is kept: external-change polling is for local files.
    pub fn open_remote(
        id: String,
        dest: String,
        path: String,
        bytes: &[u8],
    ) -> Result<Self, String> {
        let doc = Document::from_bytes(bytes).map_err(|e| e.to_string())?;
        let mut pane = Self::with_doc(id, PathBuf::from(&path), doc);
        pane.remote = Some(RemoteRef { dest, path });
        pane.remote_original = Some(bytes.to_vec());
        pane.disk = None;
        Ok(pane)
    }

    /// A remote pane whose bytes have not arrived yet (session restore). The
    /// host fills it with [`EditorPane::remote_loaded`].
    pub fn open_remote_pending(id: String, dest: String, path: String) -> Self {
        let mut pane = Self::with_doc(id, PathBuf::from(&path), Document::from_text(""));
        pane.remote = Some(RemoteRef { dest, path });
        pane.disk = None;
        pane
    }

    /// Replace a pending remote pane's document with the bytes read from
    /// `dest`, keeping the parse tree in step.
    pub fn remote_loaded(&mut self, bytes: &[u8]) {
        if let Ok(doc) = Document::from_bytes(bytes) {
            self.doc = doc;
            self.syntax = Syntax::for_file(&self.path, self.doc.rope());
            self.disk = None;
            self.remote_original = Some(bytes.to_vec());
        }
    }

    /// After a background remote write of `sent`: mark the document saved
    /// when nothing changed while the write ran.
    pub fn remote_saved(&mut self, sent: &[u8]) {
        self.saving = false;
        self.remote_original = Some(sent.to_vec());
        if self.doc.to_bytes().as_slice() == sent {
            self.doc.mark_saved();
            self.close_armed = false;
        }
    }

    pub fn with_doc(id: String, path: PathBuf, mut doc: Document) -> Self {
        let markdown = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .then(crate::live_markdown::LiveMarkdown::default);
        let syntax = Syntax::for_file(&path, doc.rope());
        let disk = disk_stamp(&path);
        doc.take_edits();
        EditorPane {
            markdown,
            large: None,
            diagnostics: Vec::new(),
            disk,
            remote_disk: None,
            remote_original: None,
            remote_polling: false,
            missing_warned: false,
            folds: Vec::new(),
            folds_revision: u64::MAX,
            folded: std::collections::HashSet::new(),
            vim: None,
            syntax,
            id,
            doc,
            path,
            remote: None,
            saving: false,
            quit_after_save: false,
            scroll_line: 0,
            scroll_col: 0,
            rows: 1,
            cols: 1,
            pending_horizontal_reveal: false,
            dragging: false,
            close_armed: false,
            proposal: None,
            last_click: None,
            flash: None,
        }
    }

    /// Write the document back. Goes through a temporary file in the same
    /// directory and a rename, so a failed write never truncates the file;
    /// the file's permissions are kept.
    pub fn save(&mut self) -> Result<(), String> {
        // A remote file is written by the host over ssh on a background
        // thread; `save_local` must not touch the local filesystem.
        if self.remote.is_some() {
            return Err("remote file: the host writes it over ssh".into());
        }
        // View mode holds a window of the file: writing it would truncate
        // the file to that window. There is nothing to save.
        if self.large.is_some() {
            return Ok(());
        }
        let bytes = self.doc.to_bytes();
        let dir = self
            .path
            .parent()
            .filter(|d| !d.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        let tmp = dir.join(format!(".{name}.mtty-save"));
        let result = (|| -> std::io::Result<()> {
            std::fs::write(&tmp, &bytes)?;
            if let Ok(meta) = std::fs::metadata(&self.path) {
                std::fs::set_permissions(&tmp, meta.permissions())?;
            }
            std::fs::rename(&tmp, &self.path)
        })();
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        self.doc.mark_saved();
        self.close_armed = false;
        self.proposal = None;
        self.disk = disk_stamp(&self.path);
        self.missing_warned = false;
        Ok(())
    }

    /// Re-read the file from disk and replace the document's text with it, as
    /// one undoable step (the cursor rides through the change). Returns
    /// whether the text changed. View-mode panes are left alone: they hold a
    /// window of the file, not the file.
    pub fn reload_from_disk(&mut self) -> bool {
        // A remote pane has no local file to poll or reload from.
        if self.large.is_some() || self.remote.is_some() {
            return false;
        }
        let Ok(bytes) = std::fs::read(&self.path) else {
            self.disk = None;
            return false;
        };
        self.missing_warned = false;
        let changed = self.reload_with(&bytes);
        self.disk = disk_stamp(&self.path);
        changed
    }

    /// Replace the document with freshly read `bytes` as one undoable step
    /// (the cursor rides through the change), leaving the pane clean. The
    /// shared reload path for a local re-read and for a remote external
    /// change. Returns whether the text changed.
    fn reload_with(&mut self, bytes: &[u8]) -> bool {
        let Ok(changed) = self.doc.reload(bytes) else {
            return false;
        };
        if changed {
            self.doc.mark_saved();
            self.close_armed = false;
        }
        changed
    }

    /// Replace a remote pane's document with bytes read over ssh after an
    /// external change, reusing the local reload path. Called only when the
    /// pane has no unsaved edits; `stamp` is the probe's stamp. Returns
    /// whether the text changed.
    pub fn reload_remote(&mut self, bytes: &[u8], stamp: DiskStamp) -> bool {
        if self.remote.is_none() {
            return false;
        }
        let changed = self.reload_with(bytes);
        self.remote_original = Some(bytes.to_vec());
        self.remote_disk = Some(stamp);
        self.missing_warned = false;
        changed
    }

    /// Record the remote stamp seen by a background probe without touching
    /// the text: a first observation, or a change kept behind unsaved edits.
    pub fn note_remote_stamp(&mut self, stamp: DiskStamp) {
        self.remote_disk = Some(stamp);
    }

    /// Turn vim mode on (fresh Normal mode) or off.
    pub fn set_vim(&mut self, on: bool) {
        self.vim = on.then(mtty_editor::vim::Vim::default);
    }

    /// The vim mode in effect, if vim is on.
    pub fn vim_mode(&self) -> Option<mtty_editor::vim::Mode> {
        self.vim.as_ref().map(|v| v.mode())
    }

    /// Feed one key to vim, revealing the caret afterwards. `None` when vim
    /// is off (the caller uses the normal keymap then).
    pub fn vim_key(&mut self, key: mtty_editor::vim::Key) -> Option<mtty_editor::vim::Action> {
        let action = self.vim.as_mut()?.handle(key, &mut self.doc);
        self.reveal_cursor();
        Some(action)
    }

    /// The file name, prefixed with its host when remote and with `●` while
    /// there are unsaved changes.
    pub fn title(&self) -> String {
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        let name = match &self.remote {
            Some(r) => format!("{}:{name}", r.dest),
            None => name,
        };
        if self.doc.is_modified() {
            format!("{name} \u{25cf}")
        } else {
            name
        }
    }

    /// Apply an agent's edits as one undoable transaction and mark the changed
    /// lines (ADR 0040, A1). The edits are `(start, end, text)` in character
    /// offsets of the current document and may be given in any order.
    pub fn propose_text(&mut self, text: String, label: Option<String>) {
        // Whole-file ACP writes should retain unchanged context rather than
        // displaying every line as both deleted and added.
        let old = self.doc.rope();
        let new = mtty_editor::Rope::from_str(&text);
        let prefix = old
            .chars()
            .zip(new.chars())
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = old
            .chars_at(old.len_chars())
            .reversed()
            .zip(new.chars_at(new.len_chars()).reversed())
            .take(old.len_chars().min(new.len_chars()) - prefix)
            .take_while(|(a, b)| a == b)
            .count();
        if prefix == old.len_chars() && prefix == new.len_chars() {
            return;
        }
        let replacement = new.slice(prefix..new.len_chars() - suffix).to_string();
        self.propose(vec![(prefix, old.len_chars() - suffix, replacement)], label);
    }

    pub fn propose(&mut self, edits: Vec<(usize, usize, String)>, label: Option<String>) {
        if edits.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = edits
            .iter()
            .map(|(s, e, t)| {
                let rope = self.doc.rope();
                let removed = if s == e {
                    Vec::new()
                } else {
                    (rope.char_to_line(*s)..=rope.char_to_line(e - 1))
                        .map(|line| {
                            (
                                line,
                                rope.line(line)
                                    .to_string()
                                    .trim_end_matches(['\r', '\n'])
                                    .to_owned(),
                            )
                        })
                        .collect()
                };
                (*s, *e, t.chars().count(), removed)
            })
            .collect();
        sorted.sort_by_key(|c| (c.0, c.1));
        // Ranges in the document as it will be after the transaction.
        let mut ranges = Vec::with_capacity(sorted.len());
        let mut delta: isize = 0;
        for (start, end, added, removed) in sorted {
            let from = (start as isize + delta).max(0) as usize;
            ranges.push((from, from + added, removed));
            delta += added as isize - (end as isize - start as isize);
        }
        let changes = edits
            .into_iter()
            .map(|(s, e, t)| mtty_editor::Change::replace(s, e, t))
            .collect();
        self.doc
            .apply_external(mtty_editor::Transaction::new(changes));
        let rope = self.doc.rope();
        let mut lines = Vec::new();
        let mut deleted = Vec::new();
        let mut deleted_seen = std::collections::HashSet::new();
        for (from, to, removed) in ranges {
            let len = rope.len_chars();
            let a = rope.char_to_line(from.min(len));
            let b = rope.char_to_line(to.min(len));
            lines.extend(a..=b);
            deleted.extend(
                removed.into_iter().filter_map(|(old_line, text)| {
                    deleted_seen.insert(old_line).then_some((a, text))
                }),
            );
        }
        lines.sort_unstable();
        lines.dedup();
        self.proposal = Some(Proposal {
            label,
            lines,
            deleted,
            scroll_offset: 0,
            revision: self.doc.revision(),
        });
        // Start at the first change, including its deleted pre-image.
        if let Some(line) = self
            .proposal
            .as_ref()
            .and_then(|p| p.lines.first())
            .copied()
        {
            self.scroll_line = line.saturating_sub(2);
            self.folded.clear();
        }
    }

    pub fn proposal(&self) -> Option<&Proposal> {
        self.proposal.as_ref()
    }

    /// Keep the proposal (its text is already applied).
    pub fn accept_proposal(&mut self) {
        self.proposal = None;
    }

    /// Undo the proposal as one step. Returns whether there was one.
    pub fn reject_proposal(&mut self) -> bool {
        if self.proposal.is_none() {
            return false;
        }
        self.proposal = None;
        self.doc.undo()
    }

    /// Catch the parse tree up with the edits made since the last call (call
    /// before drawing; cheap when nothing changed).
    pub fn sync_syntax(&mut self) {
        // Any edit after a proposal is an accept (the text is already in).
        if let Some(p) = &self.proposal {
            if self.doc.revision() != p.revision {
                self.proposal = None;
            }
        }
        // View mode: a line waiting for indexing to reach it.
        if let Some(large) = &self.large {
            if let Some(line) = large.pending_line {
                if line < large.file.line_count() || large.file.indexed() {
                    self.go_to_line(line);
                }
            }
        }
        let edits = self.doc.take_edits();
        let mut tree_changed = false;
        if let Some(syntax) = &mut self.syntax {
            if !edits.is_empty() {
                syntax.update(self.doc.rope(), &edits);
            }
            // A large file's parse finishes on a background thread.
            tree_changed = syntax.poll();
        }
        if self.folds_revision != self.doc.revision() || tree_changed {
            self.refresh_folds();
        }
    }

    /// Recompute the foldable ranges for the current text and drop folds that
    /// no longer start a range (the text moved under them). Call after any
    /// edit or a finished parse.
    fn refresh_folds(&mut self) {
        self.folds = self
            .syntax
            .as_ref()
            .map(|s| s.folds(self.doc.rope()))
            .unwrap_or_default();
        self.folds_revision = self.doc.revision();
        self.folded
            .retain(|start| self.folds.iter().any(|(s, _)| s == start));
        self.normalize_scroll();
    }

    /// If `line` starts a collapsed fold, the hidden line after it and the
    /// last line it hides. `(hidden_end)`.
    fn collapsed_end(&self, line: usize) -> Option<usize> {
        if !self.folded.contains(&line) {
            return None;
        }
        self.folds
            .iter()
            .find(|(start, _)| *start == line)
            .map(|(_, end)| *end)
    }

    /// Whether `line` can start a fold (folded or not).
    pub fn is_fold_start(&self, line: usize) -> bool {
        self.folds.iter().any(|(start, _)| *start == line)
    }

    /// If `line` is hidden inside a collapsed fold, that fold's start line.
    fn hidden_by(&self, line: usize) -> Option<usize> {
        self.folds
            .iter()
            .find(|(start, end)| *start < line && line <= *end && self.folded.contains(start))
            .map(|(start, _)| *start)
    }

    fn deleted_range(&self, line: usize) -> std::ops::Range<usize> {
        let Some(p) = &self.proposal else {
            return 0..0;
        };
        let first = p.deleted.partition_point(|(at, _)| *at < line);
        let end = p.deleted.partition_point(|(at, _)| *at <= line);
        first..end
    }

    /// The document line and optional deleted pre-image in a visual row.
    fn view_row(&self, row: usize) -> Option<(usize, Option<&str>)> {
        let last = motion::last_line(self.doc.rope());
        let mut line = self.scroll_line;
        let mut offset = row + self.proposal.as_ref().map_or(0, |p| p.scroll_offset);
        while line <= last {
            let deleted = self.deleted_range(line);
            if offset < deleted.len() {
                return Some((
                    line,
                    Some(&self.proposal.as_ref()?.deleted[deleted.start + offset].1),
                ));
            }
            if offset == deleted.len() {
                return Some((line, None));
            }
            offset -= deleted.len() + 1;
            line = self.next_visible(line);
        }
        None
    }

    /// The logical line shown at text-area `row`, skipping collapsed folds.
    fn line_at_row(&self, row: usize) -> Option<usize> {
        self.view_row(row).map(|(line, _)| line)
    }

    /// The screen row of `line`, or `None` when it is off-screen or hidden.
    fn row_of_line(&self, line: usize) -> Option<usize> {
        let mut l = self.scroll_line;
        let mut r = 0usize;
        let offset = self.proposal.as_ref().map_or(0, |p| p.scroll_offset);
        while r < self.rows + offset {
            if l > line {
                return None;
            }
            if l == line {
                let row = (r + self.deleted_range(l).len()).checked_sub(offset)?;
                return (row < self.rows).then_some(row);
            }
            r += 1 + self.deleted_range(l).len();
            l = self.next_visible(l);
        }
        None
    }

    /// The next visible line after `line`.
    fn next_visible(&self, line: usize) -> usize {
        match self.collapsed_end(line) {
            Some(end) => end + 1,
            None => line + 1,
        }
    }

    /// The visible line before `line` (a line just inside a fold jumps to its
    /// header).
    fn prev_visible(&self, line: usize) -> usize {
        let cand = line.saturating_sub(1);
        if cand < line {
            if let Some(start) = self.hidden_by(cand) {
                return start;
            }
        }
        cand
    }

    /// Keep the top line visible (never inside a collapsed fold).
    fn normalize_scroll(&mut self) {
        let last = motion::last_line(self.doc.rope());
        if self.scroll_line > last {
            self.scroll_line = last;
        }
        if let Some(start) = self.hidden_by(self.scroll_line) {
            self.scroll_line = start;
        }
    }

    /// The line the primary caret is on.
    pub fn caret_line(&self) -> usize {
        let rope = self.doc.rope();
        rope.char_to_line(self.doc.selection().primary().head)
    }

    /// Collapse the innermost fold containing the caret line.
    fn fold_at_cursor(&mut self) {
        let line = self.caret_line();
        let fold = self
            .folds
            .iter()
            .filter(|(s, e)| *s <= line && line <= *e)
            .max_by_key(|(s, _)| *s)
            .copied();
        if let Some((start, _)) = fold {
            self.folded.insert(start);
            self.normalize_scroll();
        }
    }

    /// Expand the fold at the caret line (its header or a line it hides).
    fn unfold_at_cursor(&mut self) {
        let line = self.caret_line();
        if self.folded.remove(&line) {
            return;
        }
        if let Some(start) = self.hidden_by(line) {
            self.folded.remove(&start);
        }
    }

    /// Collapse the fold at the caret, or expand it when already collapsed.
    fn toggle_fold_at_cursor(&mut self) {
        let line = self.caret_line();
        if self.folded.contains(&line) {
            self.folded.remove(&line);
            return;
        }
        self.fold_at_cursor();
    }

    /// Move the caret a visible line up or down, keeping its cell column.
    fn move_vertical(&mut self, up: bool, extend: bool) {
        let rope = self.doc.rope();
        let tab = self.doc.tab_width();
        let head = self.doc.selection().primary().head;
        let line = rope.char_to_line(head);
        let col = layout::visual_col(rope.line(line), head - rope.line_to_char(line), tab);
        let target = if up {
            self.prev_visible(line)
        } else {
            self.next_visible(line).min(motion::last_line(rope))
        };
        if target == line {
            return;
        }
        let offset = layout::offset_at_col(rope.line(target), col, tab);
        let at = rope.line_to_char(target) + offset;
        let selection = if extend {
            let p = self.doc.selection().primary();
            Selection::single(Range::new(p.anchor, at))
        } else {
            Selection::cursor(at)
        };
        self.doc.set_selection(selection);
        self.reveal_cursor();
    }

    /// The highlighted language's name, if any.
    pub fn language(&self) -> Option<&'static str> {
        self.syntax.as_ref().map(|s| s.name())
    }

    /// Cells taken by line numbers: the widest number plus a space each side.
    pub fn gutter(&self) -> usize {
        let lines = self.total_lines().max(1);
        lines.to_string().len() + 2
    }

    /// Fit to a pane of `cols` × `rows` cells (gutter included).
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols.saturating_sub(self.gutter()).max(1);
        self.rows = rows.max(1);
        if self.pending_horizontal_reveal {
            self.pending_horizontal_reveal = false;
            self.reveal_cursor();
        }
    }

    /// Scroll so the primary caret is on screen.
    pub fn reveal_cursor(&mut self) {
        let line = self.caret_line();
        // A cursor inside a collapsed fold opens it.
        if let Some(start) = self.hidden_by(line) {
            self.folded.remove(&start);
        }
        if self.row_of_line(line).is_none() {
            if let Some(p) = self.proposal.as_mut() {
                p.scroll_offset = 0;
            }
            if line < self.scroll_line {
                self.scroll_line = line;
            } else {
                // Put `line` on the last row: walk back a screen of visible
                // lines from it.
                let mut top = line;
                for _ in 0..self.rows.saturating_sub(1) {
                    let prev = self.prev_visible(top);
                    if prev == top {
                        break;
                    }
                    top = prev;
                }
                self.scroll_line = top;
            }
            if self.row_of_line(line).is_none() {
                self.scroll_line = line;
                let skipped = self.deleted_range(line).len();
                if let Some(p) = self.proposal.as_mut() {
                    p.scroll_offset = skipped;
                }
            }
        }
        self.normalize_scroll();
        if self.cols == 1 && self.rows == 1 {
            self.pending_horizontal_reveal = true;
            return;
        }
        let col = {
            let rope = self.doc.rope();
            let head = self.doc.selection().primary().head;
            layout::visual_col(
                rope.line(line),
                head - rope.line_to_char(line),
                self.doc.tab_width(),
            )
        };
        if col < self.scroll_col {
            self.scroll_col = col;
        } else if col >= self.scroll_col + self.cols {
            self.scroll_col = col + 1 - self.cols;
        }
        self.keep_window();
    }

    /// Scroll by `lines` (wheel), clamped to the document.
    pub fn scroll_by(&mut self, lines: isize) {
        if self.large.is_some() {
            let base = self.base();
            let last = self.total_lines().saturating_sub(1);
            let top = ((base + self.scroll_line) as isize + lines).clamp(0, last as isize) as usize;
            let in_window = top >= base && top <= base + motion::last_line(self.doc.rope());
            if !in_window {
                self.rewindow(top.saturating_sub(WINDOW_LINES / 3));
            }
            self.scroll_line = top - self.base();
            self.keep_window();
            return;
        }
        let last = motion::last_line(self.doc.rope());
        if lines > 0 {
            for _ in 0..lines {
                let count = self.deleted_range(self.scroll_line).len();
                if let Some(p) = self.proposal.as_mut() {
                    if p.scroll_offset < count {
                        p.scroll_offset += 1;
                        continue;
                    }
                }
                let next = self.next_visible(self.scroll_line).min(last);
                if next == self.scroll_line {
                    break;
                }
                self.scroll_line = next;
                if let Some(p) = self.proposal.as_mut() {
                    p.scroll_offset = 0;
                }
            }
        } else {
            for _ in 0..(-lines) {
                if let Some(p) = self.proposal.as_mut() {
                    if p.scroll_offset > 0 {
                        p.scroll_offset -= 1;
                        continue;
                    }
                }
                let prev = self.prev_visible(self.scroll_line);
                if prev == self.scroll_line {
                    break;
                }
                self.scroll_line = prev;
                let count = self.deleted_range(prev).len();
                if let Some(p) = self.proposal.as_mut() {
                    p.scroll_offset = count;
                }
            }
        }
        self.normalize_scroll();
    }

    /// Whether `command` changes the text (refused in view mode).
    pub fn edits(command: Command) -> bool {
        !matches!(
            command,
            Command::Move(..)
                | Command::SelectAll
                | Command::SelectLine
                | Command::SelectNextOccurrence
                | Command::SelectAllOccurrences
                | Command::CursorsAtLineEnds
                | Command::AddCursor(_)
                | Command::GoToLine
                | Command::GoToSymbol
                | Command::Fold
                | Command::Unfold
                | Command::ToggleFold
                | Command::FoldAll
                | Command::UnfoldAll
                | Command::FindReplace
                | Command::Complete
                | Command::GoToDefinition
                | Command::NextProblem(_)
                | Command::Escape
        )
    }

    /// Run a command; returns true when the text or selection changed.
    pub fn run(&mut self, command: Command) -> bool {
        let before = (self.doc.revision(), self.doc.selection().clone());
        if self.large.is_some() {
            if Self::edits(command) {
                return false;
            }
            // The file's ends, not the window's.
            match command {
                Command::Move(Motion::DocStart, false) => {
                    self.go_to_line(0);
                    return true;
                }
                Command::Move(Motion::DocEnd, false) => {
                    self.go_to_line(usize::MAX);
                    return true;
                }
                _ => {}
            }
        }
        match command {
            Command::Move(m, extend) => {
                let m = match m {
                    Motion::PageUp(_) => Motion::PageUp(self.rows.max(2) - 1),
                    Motion::PageDown(_) => Motion::PageDown(self.rows.max(2) - 1),
                    other => other,
                };
                // Up/down follow visible lines, skipping collapsed folds.
                if !self.folded.is_empty() {
                    match m {
                        Motion::Up => {
                            self.move_vertical(true, extend);
                            return true;
                        }
                        Motion::Down => {
                            self.move_vertical(false, extend);
                            return true;
                        }
                        _ => {}
                    }
                }
                self.doc.move_cursor(m, extend);
            }
            Command::Backspace => self.doc.delete_backward(),
            Command::Delete => self.doc.delete_forward(),
            Command::DeleteWordBackward => self.doc.delete_word_backward(),
            Command::DeleteToLineStart => self.doc.delete_to_line_start(),
            Command::Newline => self.doc.newline(),
            Command::Indent => self.doc.indent(),
            Command::Outdent => self.doc.outdent(),
            Command::SelectAll => self.doc.select_all(),
            Command::SelectLine => self.doc.select_line(),
            Command::SelectNextOccurrence => {
                self.doc.select_next_occurrence();
            }
            Command::SelectAllOccurrences => {
                self.doc.select_all_occurrences();
            }
            Command::CursorsAtLineEnds => self.doc.cursors_at_line_ends(),
            Command::AddCursor(below) => self.doc.add_cursor(below),
            // The host's prompts; nothing to do in the document.
            Command::NextProblem(forward) => {
                self.next_problem(forward);
            }
            Command::Fold => {
                self.fold_at_cursor();
                return true;
            }
            Command::Unfold => {
                self.unfold_at_cursor();
                return true;
            }
            Command::ToggleFold => {
                self.toggle_fold_at_cursor();
                return true;
            }
            Command::FoldAll => {
                self.folded = self.folds.iter().map(|(start, _)| *start).collect();
                self.normalize_scroll();
                return true;
            }
            Command::UnfoldAll => {
                self.folded.clear();
                return true;
            }
            // The language server's requests, made by the host.
            Command::GoToLine
            | Command::GoToSymbol
            | Command::FindReplace
            | Command::Complete
            | Command::GoToDefinition => return false,
            Command::Undo => {
                self.doc.undo();
            }
            Command::Redo => {
                self.doc.redo();
            }
            Command::Escape => {
                let sel = self.doc.selection().clone();
                if sel.len() > 1 {
                    self.doc.collapse_to_primary();
                } else {
                    let head = sel.primary().head;
                    self.doc.set_selection(Selection::cursor(head));
                }
            }
        }
        self.reveal_cursor();
        (self.doc.revision(), self.doc.selection().clone()) != before
    }

    /// Typed text (a key's text or an IME commit); ignored in view mode.
    pub fn type_text(&mut self, text: &str) {
        if self.large.is_some() {
            return;
        }
        self.doc.type_text(text);
        self.reveal_cursor();
    }

    pub fn paste(&mut self, text: &str) {
        if self.large.is_some() {
            return;
        }
        self.doc.paste(text);
        self.reveal_cursor();
    }

    /// The selection's text for the clipboard (empty without a selection).
    pub fn copy(&self) -> String {
        self.doc.selected_text()
    }

    /// Cut: the selection's text, removed from the document.
    pub fn cut(&mut self) -> String {
        if self.large.is_some() {
            return self.copy();
        }
        let text = self.doc.selected_text();
        if !text.is_empty() {
            self.doc.delete_backward();
            self.reveal_cursor();
        }
        text
    }

    /// The char index under a cell of the text area (row, col from its
    /// top-left, gutter excluded); past a line's end lands on its end.
    pub fn hit(&self, row: usize, col: usize) -> usize {
        let rope = self.doc.rope();
        if let Some((line, Some(_))) = self.view_row(row) {
            return rope.line_to_char(line);
        }
        let line = self
            .line_at_row(row)
            .unwrap_or_else(|| motion::last_line(rope));
        let offset =
            layout::offset_at_col(rope.line(line), self.scroll_col + col, self.doc.tab_width());
        rope.line_to_char(line) + offset
    }

    /// The char drawn in a text-area cell, if the cell holds one (not past
    /// a line's end or the last line).
    pub fn char_under(&self, row: usize, col: usize) -> Option<usize> {
        if self.view_row(row)?.1.is_some() {
            return None;
        }
        let rope = self.doc.rope();
        let line = self.line_at_row(row)?;
        let slice = rope.line(line);
        let tab = self.doc.tab_width();
        let col = self.scroll_col + col;
        let content = layout::content_len(slice);
        if col >= layout::visual_col(slice, content, tab) {
            return None;
        }
        // The char whose cells cover `col`.
        let mut offset = layout::offset_at_col(slice, col, tab).min(content);
        while offset > 0 && layout::visual_col(slice, offset, tab) > col {
            offset -= 1;
        }
        Some(rope.line_to_char(line) + offset)
    }

    /// A press at a text-area cell: a click places the caret (⇧ extends),
    /// a double click selects the word, a triple click the line, ⌥ adds a
    /// caret (⌘ on Linux/Windows... kept to ⌥ everywhere for one rule).
    pub fn press(&mut self, row: usize, col: usize, shift: bool, add: bool, now: Instant) {
        let at = self.hit(row, col);
        let count = match self.last_click {
            Some((t, pos, n))
                if now.duration_since(t) < Duration::from_millis(400) && pos == at =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last_click = Some((now, at, count));
        let rope = self.doc.rope();
        let selection = match count {
            2 => {
                let (a, b) = motion::word_at(rope, at);
                Selection::single(Range::new(a, b))
            }
            3 => {
                let line = rope.char_to_line(at);
                let start = rope.line_to_char(line);
                let end = if line + 1 < rope.len_lines() {
                    rope.line_to_char(line + 1)
                } else {
                    rope.len_chars()
                };
                Selection::single(Range::new(start, end))
            }
            _ if add => self.doc.selection().push(Range::cursor(at)),
            _ if shift => {
                let p = self.doc.selection().primary();
                Selection::single(Range::new(p.anchor, at))
            }
            _ => Selection::cursor(at),
        };
        self.doc.set_selection(selection);
        self.dragging = count == 1 && !add;
    }

    /// Pointer moved with the button held: extend the primary selection.
    pub fn drag_to(&mut self, row: usize, col: usize) {
        if !self.dragging {
            return;
        }
        let at = self.hit(row, col);
        let p = self.doc.selection().primary();
        if p.head != at {
            self.doc
                .set_selection(Selection::single(Range::new(p.anchor, at)));
        }
    }

    /// The cells of chars `from..to` on the visible rows (columns include
    /// the gutter). A span running past a line's end shows one cell for its
    /// line break.
    fn span_cells(&self, from: usize, to: usize, out: &mut Vec<CellRect>) {
        let rope = self.doc.rope();
        let tab = self.doc.tab_width();
        let gutter = self.gutter();
        let first = rope.char_to_line(from).max(self.scroll_line);
        let end_line = rope.char_to_line(to).min(motion::last_line(rope));
        for line in first..=end_line {
            if self.hidden_by(line).is_some() {
                continue;
            }
            let Some(row) = self.row_of_line(line) else {
                continue;
            };
            let slice = rope.line(line);
            let line_start = rope.line_to_char(line);
            let content = layout::content_len(slice);
            let a = from.max(line_start);
            let b = to.min(line_start + slice.len_chars());
            if a >= b && !(from <= line_start && to > line_start + content) {
                continue;
            }
            let c0 = layout::visual_col(slice, a.saturating_sub(line_start), tab);
            let mut c1 = layout::visual_col(slice, (b - line_start).min(content), tab);
            if to > line_start + content {
                c1 += 1;
            }
            let c0 = c0.saturating_sub(self.scroll_col);
            let c1 = c1.saturating_sub(self.scroll_col).min(self.cols);
            if c1 > c0 {
                out.push(CellRect {
                    row,
                    col: gutter + c0,
                    width: c1 - c0,
                });
            }
        }
    }

    /// The chars from the first visible line's start to the last visible
    /// line's end.
    fn visible_chars(&self) -> (usize, usize) {
        let rope = self.doc.rope();
        let last = motion::last_line(rope);
        let top_line = self.scroll_line.min(last);
        let bottom_line = self
            .line_at_row(self.rows.saturating_sub(1))
            .unwrap_or(last);
        let top = rope.line_to_char(top_line);
        let bottom = motion::line_end(rope, bottom_line);
        (top, bottom)
    }

    /// Cells of the Find matches on screen, each with whether it is the
    /// current one. `hits` are sorted char ranges.
    pub fn match_cells(&self, hits: &[(usize, usize)], current: usize) -> Vec<(CellRect, bool)> {
        let rope = self.doc.rope();
        let (top, bottom) = self.visible_chars();
        let first = hits.partition_point(|&(_, end)| end <= top);
        let mut out = Vec::new();
        let mut cells = Vec::new();
        for (i, &(a, b)) in hits.iter().enumerate().skip(first) {
            if a > bottom || b > rope.len_chars() {
                break;
            }
            cells.clear();
            self.span_cells(a, b, &mut cells);
            out.extend(cells.iter().map(|c| (*c, i == current)));
        }
        out
    }

    /// The visible rows: glyph spans (gutter numbers, then text clipped to
    /// the text area), selection and caret cells.
    pub fn draw(&self, palette: Palette, focused: bool, carets_on: bool) -> EditorDraw {
        let rope = self.doc.rope();
        let tab = self.doc.tab_width();
        let gutter = self.gutter();
        let last = motion::last_line(rope);
        let selection = self.doc.selection();
        let head_line = rope.char_to_line(selection.primary().head);
        // Highlights for the visible lines only.
        let first_byte = rope.line_to_byte(self.scroll_line.min(last));
        let end_line = self
            .line_at_row(self.rows.saturating_sub(1))
            .map_or(rope.len_lines(), |l| l + 1)
            .min(rope.len_lines());
        let end_byte = if end_line >= rope.len_lines() {
            rope.len_bytes()
        } else {
            rope.line_to_byte(end_line)
        };
        let highlights = self
            .syntax
            .as_ref()
            .map(|s| s.highlights(rope, first_byte..end_byte))
            .unwrap_or_default();
        let mut hl = highlights.iter().peekable();
        let mut rows = Vec::with_capacity(self.rows);
        let mut sel_cells = Vec::new();
        let mut carets = Vec::new();
        let mut deletion_cells = Vec::new();
        for row in 0..self.rows {
            if let Some((_, Some(text))) = self.view_row(row) {
                let old = mtty_editor::Rope::from_str(text);
                let mut spans = vec![Span::new(0, "−", (0xbf, 0x61, 0x6a))];
                for g in layout::glyphs(old.slice(..), tab, self.scroll_col + self.cols) {
                    if g.col + g.width <= self.scroll_col || g.ch.is_control() {
                        continue;
                    }
                    let col = g.col.saturating_sub(self.scroll_col);
                    if col + g.width > self.cols {
                        break;
                    }
                    spans.push(Span::new(
                        (gutter + col) as u16,
                        g.ch.to_string(),
                        (0xbf, 0x61, 0x6a),
                    ));
                }
                deletion_cells.push(CellRect {
                    row,
                    col: gutter,
                    width: layout::visual_col(old.slice(..), old.len_chars(), tab)
                        .saturating_sub(self.scroll_col)
                        .min(self.cols)
                        .max(1),
                });
                rows.push(spans);
                continue;
            }
            let Some(line) = self.line_at_row(row) else {
                rows.push(Vec::new());
                continue;
            };
            let mut spans = Vec::new();
            let marker = if self.is_fold_start(line) {
                if self.folded.contains(&line) {
                    "\u{25b8}"
                } else {
                    "\u{25be}"
                }
            } else {
                ""
            };
            let number = format!("{marker}{}", self.base() + line + 1);
            let color = if line == head_line {
                palette.gutter_current
            } else {
                palette.gutter
            };
            // Right-align the number in the gutter. A windowed (large-file)
            // view can number lines past what `total_lines` reports while the
            // file is still being indexed, so the number can outgrow the
            // gutter; keep the least significant cells instead of underflowing
            // the column. Count cells by `char`, never bytes: the fold marker
            // is a multi-byte glyph.
            let room = gutter.saturating_sub(1);
            let cells = number.chars().count();
            let number = if cells > room {
                number.chars().skip(cells - room).collect::<String>()
            } else {
                number
            };
            let cells = number.chars().count();
            spans.push(Span::new(
                gutter.saturating_sub(1 + cells) as u16,
                number,
                color,
            ));
            let slice = rope.line(line);
            let mut byte = rope.line_to_byte(line);
            let mut run = String::new();
            let mut run_col = 0usize;
            let mut run_color = palette.fg;
            let flush = |run: &mut String, run_col: usize, color, spans: &mut Vec<Span>| {
                if !run.is_empty() {
                    spans.push(Span::new(
                        (gutter + run_col) as u16,
                        std::mem::take(run),
                        color,
                    ));
                }
            };
            for g in layout::glyphs(slice, tab, self.scroll_col + self.cols) {
                let at = byte;
                byte += g.ch.len_utf8();
                // The highlight covering this glyph's first byte, if any.
                while hl.peek().is_some_and(|(r, _)| r.end <= at) {
                    hl.next();
                }
                let color = match hl.peek() {
                    Some((r, h)) if r.start <= at => palette.highlight(*h),
                    _ => palette.fg,
                };
                if g.col + g.width <= self.scroll_col {
                    continue;
                }
                let col = g.col.saturating_sub(self.scroll_col);
                if col + g.width > self.cols {
                    break;
                }
                if g.ch == '\t' || g.ch.is_control() {
                    flush(&mut run, run_col, run_color, &mut spans);
                    continue;
                }
                if g.width != 1 {
                    // Wide characters get their own span at an exact cell.
                    flush(&mut run, run_col, run_color, &mut spans);
                    if g.width == 2 {
                        spans.push(Span::new((gutter + col) as u16, g.ch.to_string(), color));
                    }
                    continue;
                }
                if !run.is_empty() && (run_col + run.chars().count() != col || run_color != color) {
                    flush(&mut run, run_col, run_color, &mut spans);
                }
                if run.is_empty() {
                    run_col = col;
                    run_color = color;
                }
                run.push(g.ch);
            }
            flush(&mut run, run_col, run_color, &mut spans);
            // A collapsed fold shows an ellipsis where its hidden text would be.
            if self.collapsed_end(line).is_some() {
                let content = layout::content_len(slice);
                let endcol =
                    layout::visual_col(slice, content, tab).saturating_sub(self.scroll_col);
                let col = gutter + endcol.min(self.cols.saturating_sub(1));
                spans.push(Span::new(col as u16, "\u{22ef}", palette.gutter));
            }
            rows.push(spans);
        }
        // Only the ranges on screen: there may be thousands (every
        // occurrence selected). They are sorted and do not overlap.
        let (top, bottom) = self.visible_chars();
        let ranges = selection.ranges();
        let first = ranges.partition_point(|r| r.to() < top);
        for r in ranges[first..].iter().take_while(|r| r.from() <= bottom) {
            if !r.is_empty() {
                self.span_cells(r.from(), r.to(), &mut sel_cells);
            }
            if !(focused && carets_on) {
                continue;
            }
            let line = rope.char_to_line(r.head);
            let Some(row) = self.row_of_line(line) else {
                continue;
            };
            let slice = rope.line(line);
            let c = layout::visual_col(slice, r.head - rope.line_to_char(line), tab);
            if c >= self.scroll_col && c - self.scroll_col <= self.cols {
                carets.push((row, gutter + c - self.scroll_col));
            }
        }
        // Diagnostics on screen, at least a cell wide.
        let mut underlines = Vec::new();
        let first = self.diagnostics.partition_point(|d| d.to < top);
        for d in self.diagnostics[first..]
            .iter()
            .take_while(|d| d.from <= bottom)
        {
            let to =
                d.to.max(motion::next_grapheme(rope, d.from).max(d.from + 1));
            let mut cells = Vec::new();
            self.span_cells(d.from, to, &mut cells);
            underlines.extend(cells.into_iter().map(|c| (c, d.severity)));
        }
        let current_line = self.row_of_line(head_line);
        let flash_line = self
            .flash
            .as_ref()
            .filter(|(_, at)| at.elapsed() < FLASH)
            .and_then(|(line, _)| self.row_of_line(*line));
        // A pending agent proposal tints its changed lines.
        let mut proposal_cells = Vec::new();
        if let Some(p) = &self.proposal {
            for &line in &p.lines {
                if let Some(row) = self.row_of_line(line) {
                    proposal_cells.push(CellRect {
                        row,
                        col: gutter,
                        width: self.cols.saturating_sub(gutter).max(1),
                    });
                }
            }
        }
        EditorDraw {
            rows,
            selection: sel_cells,
            current_line,
            flash_line,
            carets,
            gutter,
            underlines,
            proposal: proposal_cells,
            deletions: deletion_cells,
        }
    }

    /// The diagnostics covering char `at` (an empty one at its position),
    /// most severe first.
    pub fn diagnostics_at(&self, at: usize) -> Vec<&PaneDiagnostic> {
        let mut found: Vec<&PaneDiagnostic> = self
            .diagnostics
            .iter()
            .filter(|d| d.from <= at && (at < d.to || at == d.from))
            .collect();
        found.sort_by_key(|d| d.severity);
        found
    }

    /// Errors and warnings, for the status bar.
    pub fn problem_counts(&self) -> (usize, usize) {
        let errors = self.diagnostics.iter().filter(|d| d.severity == 1).count();
        let warnings = self.diagnostics.iter().filter(|d| d.severity == 2).count();
        (errors, warnings)
    }

    /// F8: select the next diagnostic after the caret (or the previous one
    /// before it), wrapping around. Its index, if there is any.
    pub fn next_problem(&mut self, forward: bool) -> Option<usize> {
        if self.diagnostics.is_empty() {
            return None;
        }
        let caret = self.doc.selection().primary().from();
        let n = self.diagnostics.len();
        let index = if forward {
            self.diagnostics
                .iter()
                .position(|d| d.from > caret)
                .unwrap_or(0)
        } else {
            self.diagnostics
                .iter()
                .rposition(|d| d.from < caret)
                .unwrap_or(n - 1)
        };
        let d = &self.diagnostics[index];
        let len = self.doc.rope().len_chars();
        let (from, to) = (d.from.min(len), d.to.min(len));
        self.doc
            .set_selection(Selection::single(Range::new(from, to)));
        self.reveal_cursor();
        Some(index)
    }

    /// The primary caret as 1-based line and column (columns in cells).
    pub fn caret_line_col(&self) -> (usize, usize) {
        let rope = self.doc.rope();
        let head = self.doc.selection().primary().head;
        let line = rope.char_to_line(head);
        let col = layout::visual_col(
            rope.line(line),
            head - rope.line_to_char(line),
            self.doc.tab_width(),
        );
        (self.base() + line + 1, col + 1)
    }

    /// `LF` or `CRLF`.
    pub fn line_ending_name(&self) -> &'static str {
        match self.doc.line_ending() {
            mtty_editor::LineEnding::Lf => "LF",
            mtty_editor::LineEnding::CrLf => "CRLF",
        }
    }

    /// The primary caret's cell (row, col including the gutter), when on
    /// screen: where the IME candidate window goes.
    pub fn caret_cell(&self) -> Option<(usize, usize)> {
        let rope = self.doc.rope();
        let head = self.doc.selection().primary().head;
        let line = rope.char_to_line(head);
        if line < self.scroll_line || line >= self.scroll_line + self.rows {
            return None;
        }
        let c = layout::visual_col(
            rope.line(line),
            head - rope.line_to_char(line),
            self.doc.tab_width(),
        );
        (c >= self.scroll_col)
            .then(|| (line - self.scroll_line, self.gutter() + c - self.scroll_col))
    }
}

/// A Go to Line entry: `line`, `line:col` or `line,col` (1-based, as the
/// status bar shows them). Returns 0-based line and cell column.
pub fn parse_line_target(input: &str) -> Option<(usize, usize)> {
    let input = input.trim().trim_start_matches(':');
    let (line, col) = match input.split_once([':', ',']) {
        Some((l, c)) => (l.trim(), Some(c.trim())),
        None => (input, None),
    };
    let line: usize = line.parse().ok().filter(|l| *l > 0)?;
    let col = match col {
        Some(c) if !c.is_empty() => c.parse::<usize>().ok().filter(|c| *c > 0)?,
        _ => 1,
    };
    Some((line - 1, col - 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_navigation_waits_for_real_horizontal_geometry() {
        let mut pane = EditorPane::with_doc(
            "navigation".into(),
            "sample.rs".into(),
            Document::from_bytes(b"fn target() {}\n").unwrap(),
        );
        pane.doc.set_selection(mtty_editor::Selection::cursor(3));
        pane.reveal_cursor();
        assert_eq!(pane.scroll_col, 0);
        pane.resize(80, 24);
        assert_eq!(pane.scroll_col, 0);
        let text = "x".repeat(200);
        let mut pane = EditorPane::with_doc(
            "wide".into(),
            "wide.txt".into(),
            Document::from_bytes(text.as_bytes()).unwrap(),
        );
        pane.doc.set_selection(mtty_editor::Selection::cursor(150));
        pane.reveal_cursor();
        assert_eq!(pane.scroll_col, 0);
        pane.resize(80, 24);
        assert!(pane.scroll_col > 0);
    }

    fn view_of(lines: usize) -> (EditorPane, PathBuf, String) {
        let text: String = (0..lines).map(|i| format!("line {i}\n")).collect();
        let dir = std::env::temp_dir().join(format!("mtty-view-{}-{lines}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.log");
        std::fs::write(&path, &text).unwrap();
        let mut p = EditorPane::open_view("v".into(), &path).unwrap();
        let file = p.large.as_ref().unwrap().file.clone();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !file.indexed() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        p.resize(80, 30);
        (p, path, text)
    }

    fn first_number(p: &EditorPane) -> String {
        let palette = Palette {
            fg: (1, 1, 1),
            gutter: (2, 2, 2),
            gutter_current: (3, 3, 3),
        };
        p.draw(palette, true, true).rows[0][0]
            .text
            .trim()
            .to_string()
    }

    #[test]
    fn view_mode_scrolls_a_window_through_the_whole_file() {
        let (mut p, _, _) = view_of(20_000);
        assert!(p.is_view_only());
        assert_eq!(p.total_lines(), 20_001);
        assert!(
            p.doc.rope().len_lines() <= WINDOW_LINES + 1,
            "only a window is loaded"
        );
        p.scroll_by(12_345);
        assert_eq!(first_number(&p), "12346");
        assert_eq!(p.base() + p.scroll_line, 12_345);
        let top = p.scroll_line;
        assert_eq!(p.doc.rope().line(top).to_string(), "line 12345\n");
        // ⌘↓ / ⌘↑ go to the file's ends, not the window's.
        p.run(Command::Move(Motion::DocEnd, false));
        assert_eq!(p.caret_line_col().0, 20_001);
        p.run(Command::Move(Motion::DocStart, false));
        assert_eq!(p.caret_line_col().0, 1);
        assert_eq!(first_number(&p), "1");
        // Line by line past the window's end keeps the text continuous.
        p.go_to_line(WINDOW_LINES - 5);
        for _ in 0..20 {
            p.run(Command::Move(Motion::Down, false));
        }
        let (line, _) = p.caret_line_col();
        assert_eq!(line, WINDOW_LINES - 5 + 20 + 1);
        let head = p.doc.selection().primary().head;
        let local = p.doc.rope().char_to_line(head);
        assert_eq!(
            p.doc.rope().line(local).to_string(),
            format!("line {}\n", line - 1)
        );
    }

    #[test]
    fn a_line_past_the_indexed_part_is_reached_when_indexing_gets_there() {
        let (mut p, _, _) = view_of(3_000);
        // As a restored session does right after opening: pretend the
        // index is short of the line by asking before it is known.
        p.large.as_mut().unwrap().pending_line = Some(2_500);
        p.sync_syntax();
        assert_eq!(p.caret_line_col().0, 2_501);
        assert!(p.large.as_ref().unwrap().pending_line.is_none());
    }

    #[test]
    fn view_mode_refuses_edits_and_never_writes_the_file() {
        let (mut p, path, text) = view_of(5_000);
        p.go_to_line(4_000);
        assert!(!p.run(Command::Backspace));
        p.type_text("x");
        p.paste("pasted");
        assert!(!p.doc.is_modified());
        p.save().unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            text,
            "file untouched"
        );
    }

    #[test]
    fn view_mode_selects_a_search_hit_by_its_bytes() {
        let (mut p, _, text) = view_of(10_000);
        let at = text.find("line 7777\n").unwrap() as u64 + 5;
        p.select_bytes(at, at + 4);
        assert_eq!(p.copy(), "7777");
        assert_eq!(p.caret_line_col().0, 7_778);
    }

    fn pane(text: &str) -> EditorPane {
        let mut p =
            EditorPane::with_doc("e1".into(), "/tmp/x.rs".into(), Document::from_text(text));
        p.resize(40, 5);
        p
    }

    fn palette() -> Palette {
        Palette {
            fg: (200, 200, 200),
            gutter: (90, 90, 90),
            gutter_current: (150, 150, 150),
        }
    }

    fn row_text(spans: &[Span]) -> String {
        let mut cells = vec![' '; 60];
        for s in spans {
            for (i, c) in s.text.chars().enumerate() {
                if let Some(cell) = cells.get_mut(s.col as usize + i) {
                    *cell = c;
                }
            }
        }
        cells.into_iter().collect::<String>().trim_end().to_string()
    }

    #[test]
    fn keymap_follows_platform_text_conventions() {
        let mac = cfg!(target_os = "macos");
        let (word_alt, word_ctrl) = if mac { (true, false) } else { (false, true) };
        assert_eq!(
            keymap(KeyKind::Left, false, word_alt, false, word_ctrl),
            Some(Command::Move(Motion::WordLeft, false))
        );
        assert_eq!(
            keymap(KeyKind::Right, true, false, false, false),
            Some(Command::Move(Motion::Right, true))
        );
        assert_eq!(
            keymap(KeyKind::Enter, false, false, false, false),
            Some(Command::Newline)
        );
        assert_eq!(
            keymap(KeyKind::Tab, true, false, false, false),
            Some(Command::Outdent)
        );
        let (cmd, ctrl) = if mac { (true, false) } else { (false, true) };
        assert_eq!(
            keymap(KeyKind::Char('z'), false, false, cmd, ctrl),
            Some(Command::Undo)
        );
        assert_eq!(
            keymap(KeyKind::Char('Z'), true, false, cmd, ctrl),
            Some(Command::Redo)
        );
        assert_eq!(
            keymap(KeyKind::Char('d'), false, false, cmd, ctrl),
            Some(Command::SelectNextOccurrence)
        );
        assert_eq!(
            keymap(KeyKind::Char('s'), false, false, cmd, ctrl),
            None,
            "save is the app's"
        );
        assert_eq!(
            keymap(KeyKind::Char('x'), false, false, false, false),
            None,
            "text input"
        );
        if mac {
            assert_eq!(
                keymap(KeyKind::Left, false, false, true, false),
                Some(Command::Move(Motion::LineStart, false))
            );
            assert_eq!(
                keymap(KeyKind::Backspace, false, false, true, false),
                Some(Command::DeleteToLineStart)
            );
        }
    }

    #[test]
    fn a_line_jump_flashes_the_target_line() {
        let mut p = pane("a\nb\nc\nd\ne\n");
        p.go_to_line(2);
        let d = p.draw(palette(), true, true);
        assert_eq!(d.flash_line, Some(2), "the jumped-to line's row");
    }

    #[test]
    fn draws_numbers_text_and_clips_long_lines() {
        let mut p = pane("fn main() {\n\tlet 中 = 1;\n}\n");
        p.resize(14, 5);
        let d = p.draw(palette(), true, true);
        assert_eq!(d.gutter, 3);
        assert_eq!(row_text(&d.rows[0]), " 1 fn main() {");
        assert_eq!(row_text(&d.rows[1]), " 2     let 中");
        assert_eq!(row_text(&d.rows[3]), " 4");
        assert_eq!(d.rows[4].len(), 0, "past the end of the document");
        assert_eq!(d.carets, vec![(0, 3)]);
        assert_eq!(p.caret_line_col(), (1, 1));
        assert_eq!(p.line_ending_name(), "LF");
        assert_eq!(d.current_line, Some(0));
        let wide = d.rows[1].iter().find(|s| s.text == "中").unwrap();
        assert_eq!(wide.col, 11);
    }

    #[test]
    fn rust_files_draw_in_syntax_colours() {
        let mut p = EditorPane::with_doc(
            "e".into(),
            "/tmp/x.rs".into(),
            Document::from_text("fn main() { let s = \"hi\"; } // done\n"),
        );
        p.resize(80, 3);
        assert_eq!(p.language(), Some("Rust"));
        let d = p.draw(palette(), true, true);
        let color_of = |text: &str| {
            d.rows[0]
                .iter()
                .find(|s| s.text.contains(text))
                .map(|s| s.color)
        };
        assert_eq!(
            color_of("fn"),
            Some(palette().highlight(Highlight::Keyword))
        );
        assert_eq!(
            color_of("\"hi\""),
            Some(palette().highlight(Highlight::String))
        );
        assert_eq!(
            color_of("// done"),
            Some(palette().highlight(Highlight::Comment))
        );
        // An edit re-highlights through the incremental tree.
        p.doc.set_selection(Selection::cursor(0));
        p.type_text("// ");
        p.sync_syntax();
        let d = p.draw(palette(), true, true);
        let comment = palette().highlight(Highlight::Comment);
        assert!(d.rows[0]
            .iter()
            .filter(|s| s.col >= 3)
            .all(|s| s.color == comment));
        let txt =
            EditorPane::with_doc("e".into(), "/tmp/x.txt".into(), Document::from_text("fn x"));
        assert_eq!(txt.language(), None);
    }

    #[test]
    fn selection_cells_include_the_line_break() {
        let mut p = pane("abc\ndef\n");
        p.doc.set_selection(Selection::single(Range::new(1, 6)));
        let d = p.draw(palette(), true, false);
        assert_eq!(
            d.selection,
            vec![
                CellRect {
                    row: 0,
                    col: 4,
                    width: 3
                },
                CellRect {
                    row: 1,
                    col: 3,
                    width: 2
                },
            ]
        );
        assert!(d.carets.is_empty(), "blink off");
    }

    #[test]
    fn typing_scrolls_to_keep_the_caret_visible() {
        let mut p = pane(&"line\n".repeat(50));
        p.run(Command::Move(Motion::DocEnd, false));
        assert_eq!(p.scroll_line, 46, "last line at the bottom of 5 rows");
        p.run(Command::Move(Motion::DocStart, false));
        assert_eq!(p.scroll_line, 0);
        p.type_text(&"x".repeat(60));
        assert_eq!(p.scroll_col, 60 + 1 - p.cols);
    }

    #[test]
    fn clicks_place_select_words_and_lines() {
        let mut p = pane("let value = 1;\nnext\n");
        let t = Instant::now();
        p.press(0, 6, false, false, t);
        assert_eq!(p.doc.selection().primary(), Range::cursor(6));
        p.press(0, 6, false, false, t + Duration::from_millis(100));
        assert_eq!(p.copy(), "value");
        p.press(0, 6, false, false, t + Duration::from_millis(200));
        assert_eq!(p.copy(), "let value = 1;\n");
        p.press(1, 2, true, false, t + Duration::from_secs(2));
        assert_eq!(
            p.doc.selection().primary(),
            Range::new(0, 17),
            "shift extends from the line selection's anchor"
        );
        p.press(0, 99, false, false, t + Duration::from_secs(4));
        assert_eq!(
            p.doc.selection().primary(),
            Range::cursor(14),
            "past the end"
        );
        p.drag_to(1, 4);
        assert_eq!(p.copy(), "\nnext");
    }

    /// One step of a replay: a key with modifiers, or typed text.
    enum Step {
        Key(KeyKind, &'static str),
        Text(&'static str),
    }

    /// Feed keys through the keymap into the pane, as the host does; `mods`
    /// names the platform's primary modifier as `P` (⌘ / Ctrl), its word
    /// modifier as `W` (⌥ / Ctrl), plus `S` shift, `A` alt, `C` ctrl.
    fn replay(p: &mut EditorPane, steps: &[Step]) -> Vec<Command> {
        let mac = cfg!(target_os = "macos");
        let mut ran = Vec::new();
        for step in steps {
            match step {
                Step::Text(t) => p.type_text(t),
                Step::Key(key, mods) => {
                    let has = |c| mods.contains(c);
                    let shift = has('S');
                    let alt = has('A') || (mac && has('W'));
                    let cmd = mac && has('P');
                    let ctrl = has('C') || (!mac && (has('P') || has('W')));
                    let command = keymap(*key, shift, alt, cmd, ctrl)
                        .unwrap_or_else(|| panic!("a key with {mods} maps to nothing"));
                    p.run(command);
                    ran.push(command);
                }
            }
        }
        ran
    }

    fn texts(p: &EditorPane) -> String {
        p.doc.rope().to_string()
    }

    #[test]
    fn replay_multiple_cursors() {
        use KeyKind::*;
        let mac = cfg!(target_os = "macos");
        // Add carets below (⌥⌘↓ / Ctrl+Alt+↓), type on every line.
        let mut p = pane("a\nb\nc\n");
        let below = if mac { "PA" } else { "CA" };
        replay(
            &mut p,
            &[
                Step::Key(Down, below),
                Step::Key(Down, below),
                Step::Text("- "),
            ],
        );
        assert_eq!(texts(&p), "- a\n- b\n- c\n");
        assert_eq!(p.draw(palette(), true, true).carets.len(), 3);
        // Escape keeps the primary caret only.
        replay(&mut p, &[Step::Key(Escape, "")]);
        assert_eq!(p.doc.selection().len(), 1);

        // ⇧⌘L selects every occurrence of the word; typing renames them all.
        let mut p = pane("let n = n + 1; n");
        p.doc.set_selection(Selection::cursor(4));
        replay(&mut p, &[Step::Key(Char('l'), "PS"), Step::Text("count")]);
        assert_eq!(texts(&p), "let count = count + 1; count");
        // One undo step brings the name back everywhere.
        replay(&mut p, &[Step::Key(Char('z'), "P")]);
        assert_eq!(texts(&p), "let n = n + 1; n");

        // ⌘D twice, then ⇧⌥I on a block: carets at each line end.
        let mut p = pane("x = 1\nx = 2\nx = 3\n");
        replay(
            &mut p,
            &[
                Step::Key(Char('d'), "P"),
                Step::Key(Char('d'), "P"),
                Step::Text("y"),
            ],
        );
        assert_eq!(texts(&p), "y = 1\ny = 2\nx = 3\n");
        p.doc.set_selection(Selection::single(Range::new(0, 17)));
        replay(&mut p, &[Step::Key(Char('i'), "SA"), Step::Text(";")]);
        assert_eq!(texts(&p), "y = 1;\ny = 2;\nx = 3;\n");
    }

    #[test]
    fn replay_find_and_go_to_line_keys_reach_the_host() {
        use KeyKind::*;
        let mac = cfg!(target_os = "macos");
        let mut p = pane("one\ntwo\n");
        let go = if mac { "C" } else { "P" };
        let replace = if mac { "PA" } else { "P" };
        let ran = replay(
            &mut p,
            &[
                Step::Key(Char('g'), go),
                Step::Key(Char(if mac { 'f' } else { 'h' }), replace),
            ],
        );
        assert_eq!(ran, vec![Command::GoToLine, Command::FindReplace]);
        assert!(!p.run(Command::GoToLine), "nothing changes in the pane");
        assert!(!EditorPane::edits(Command::FindReplace));
    }

    #[test]
    fn go_to_line_targets_parse_and_land() {
        assert_eq!(parse_line_target("12"), Some((11, 0)));
        assert_eq!(parse_line_target(" 12:5 "), Some((11, 4)));
        assert_eq!(parse_line_target(":3,2"), Some((2, 1)));
        assert_eq!(parse_line_target("12:"), Some((11, 0)));
        assert_eq!(parse_line_target("0"), None);
        assert_eq!(parse_line_target("x"), None);
        assert_eq!(parse_line_target("3:y"), None);
        let mut p = pane(&"\tline\n".repeat(40));
        p.go_to_line_col(29, 6);
        assert_eq!(p.caret_line_col(), (30, 7), "a tab is four cells");
        assert!(p.scroll_line <= 29 && 29 < p.scroll_line + p.rows);
        p.go_to_line_col(5, 99);
        assert_eq!(p.caret_line_col(), (6, 9), "stops at the line end");
        p.go_to_line_col(9999, 0);
        assert_eq!(p.caret_line_col().0, 41, "past the end: the last line");
    }

    #[test]
    fn find_matches_draw_on_screen_only() {
        let mut p = pane(&"ab ab\n".repeat(20));
        let hits =
            mtty_editor::search::find_all(p.doc.rope(), &mtty_editor::SearchQuery::literal("ab"))
                .unwrap();
        assert_eq!(hits.len(), 40);
        p.scroll_by(3);
        let cells = p.match_cells(&hits, 7);
        assert_eq!(cells.len(), 10, "two a line on five rows");
        assert_eq!(
            cells[0],
            (
                CellRect {
                    row: 0,
                    col: 4,
                    width: 2
                },
                false
            ),
            "line 4, after a three-cell gutter"
        );
        assert_eq!(cells.iter().filter(|c| c.1).count(), 1);
        assert_eq!(
            cells[1],
            (
                CellRect {
                    row: 0,
                    col: 7,
                    width: 2
                },
                true
            )
        );
    }

    #[test]
    fn diagnostics_underline_and_f8_walks_them() {
        let mut p = pane("let a = 1;\nlet b = ;\nfoo(\n");
        p.diagnostics = vec![
            PaneDiagnostic {
                from: 4,
                to: 5,
                severity: 2,
                message: "unused variable: `a`".into(),
            },
            PaneDiagnostic {
                from: 19,
                to: 19,
                severity: 1,
                message: "expected expression".into(),
            },
        ];
        let d = p.draw(palette(), true, true);
        assert_eq!(
            d.underlines,
            vec![
                (
                    CellRect {
                        row: 0,
                        col: 7,
                        width: 1
                    },
                    2
                ),
                (
                    CellRect {
                        row: 1,
                        col: 11,
                        width: 1
                    },
                    1
                ),
            ],
            "an empty range still gets a cell"
        );
        assert_eq!(p.problem_counts(), (1, 1));
        assert_eq!(p.diagnostics_at(19)[0].message, "expected expression");
        assert!(p.diagnostics_at(0).is_empty());
        let mac = cfg!(target_os = "macos");
        use KeyKind::*;
        let ran = replay(&mut p, &[Step::Key(F(8), "")]);
        assert_eq!(ran, vec![Command::NextProblem(true)]);
        assert_eq!(p.doc.selection().primary(), Range::new(4, 5));
        replay(&mut p, &[Step::Key(F(8), "")]);
        assert_eq!(p.doc.selection().primary().head, 19);
        replay(&mut p, &[Step::Key(F(8), "")]);
        assert_eq!(p.doc.selection().primary().from(), 4, "wraps around");
        replay(&mut p, &[Step::Key(F(8), "S")]);
        assert_eq!(p.doc.selection().primary().head, 19, "⇧F8 goes back");
        assert_eq!(
            keymap(F(12), false, false, false, false),
            Some(Command::GoToDefinition)
        );
        assert_eq!(
            keymap(Char(' '), false, false, false, true),
            Some(Command::Complete)
        );
        let _ = mac;
    }

    #[test]
    fn the_char_under_a_cell() {
        let mut p = pane("\tab中c\n");
        p.resize(40, 5);
        assert_eq!(p.char_under(0, 0), Some(0), "inside the tab");
        assert_eq!(p.char_under(0, 3), Some(0));
        assert_eq!(p.char_under(0, 4), Some(1));
        assert_eq!(p.char_under(0, 7), Some(3), "the right half of 中");
        assert_eq!(p.char_under(0, 8), Some(4));
        assert_eq!(p.char_under(0, 9), None, "past the line");
        assert_eq!(p.char_under(3, 0), None, "past the document");
    }

    #[test]
    fn cut_and_paste_round_trip() {
        let mut p = pane("one two");
        p.doc.set_selection(Selection::single(Range::new(0, 4)));
        assert_eq!(p.cut(), "one ");
        assert_eq!(p.doc.rope().to_string(), "two");
        p.paste("one ");
        assert_eq!(p.doc.rope().to_string(), "one two");
    }

    /// Perf gate (ADR 0034, E2): building the visible rows of a 100 MB file,
    /// scrolled to its middle with a selection, stays far below the 4 ms
    /// frame budget. Run with `cargo test --release -- --ignored`.
    #[test]
    #[ignore = "perf gate; run `cargo test --release -- --ignored`"]
    fn drawing_a_screen_of_a_100_mb_file_fits_a_frame() {
        let line = "    let value = compute(index, &table[offset..end]); // 中文注释\n";
        let text = line.repeat((100 << 20) / line.len());
        let mut p =
            EditorPane::with_doc("e".into(), "/tmp/big.rs".into(), Document::from_text(&text));
        p.resize(200, 60);
        let middle = p.doc.rope().len_chars() / 2;
        p.doc
            .set_selection(Selection::single(Range::new(middle, middle + 5000)));
        p.reveal_cursor();
        let runs = 200;
        let start = Instant::now();
        let mut glyphs = 0;
        for _ in 0..runs {
            let d = p.draw(palette(), true, true);
            glyphs += d.rows.len();
        }
        let ms = start.elapsed().as_secs_f64() * 1e3 / runs as f64;
        let scale: f64 = std::env::var("MTTY_PERF_SCALE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        eprintln!("editor rows for one screen of a 100 MB file: {ms:.4} ms");
        assert!(glyphs > 0);
        assert!(ms <= 4.0 * scale, "{ms:.4} ms per frame, budget 4 ms");
    }

    /// Perf gate (ADR 0034, E4): every occurrence selected in a 100 MB file
    /// (over a million carets) still draws a screen within the frame budget,
    /// as only the ranges on screen are visited.
    #[test]
    #[ignore = "perf gate; run `cargo test --release -- --ignored`"]
    fn drawing_with_every_occurrence_selected_fits_a_frame() {
        let line = "    let value = compute(index, &table[offset..end]); // 中文注释\n";
        let text = line.repeat((100 << 20) / line.len());
        let mut p =
            EditorPane::with_doc("e".into(), "/tmp/big.rs".into(), Document::from_text(&text));
        p.resize(200, 60);
        let at = p.doc.rope().len_chars() / 2 + 8;
        p.doc.set_selection(Selection::cursor(at));
        let start = Instant::now();
        assert!(p.doc.select_all_occurrences());
        eprintln!(
            "select {} occurrences in 100 MB: {:.1} ms",
            p.doc.selection().len(),
            start.elapsed().as_secs_f64() * 1e3
        );
        p.reveal_cursor();
        let runs = 200;
        let start = Instant::now();
        for _ in 0..runs {
            let d = p.draw(palette(), true, true);
            assert!(!d.carets.is_empty());
        }
        let ms = start.elapsed().as_secs_f64() * 1e3 / runs as f64;
        let scale: f64 = std::env::var("MTTY_PERF_SCALE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1.0);
        eprintln!("editor rows with every occurrence selected: {ms:.4} ms");
        assert!(ms <= 4.0 * scale, "{ms:.4} ms per frame, budget 4 ms");
    }

    #[test]
    fn saves_atomically_and_keeps_permissions() {
        let dir = std::env::temp_dir().join(format!("mtty-editor-pane-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "hello\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let mut p = EditorPane::open("e".into(), &file).unwrap();
        p.doc.set_selection(Selection::cursor(5));
        p.type_text("!");
        assert!(p.title().ends_with('\u{25cf}'));
        p.save().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello!\r\n");
        assert_eq!(p.title(), "a.txt");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o640);
        }
        assert!(!dir.join(".a.txt.mtty-save").exists());
        assert!(EditorPane::open("e".into(), &dir.join("missing")).is_err());
        std::fs::write(dir.join("bin"), b"\0\x01\x02").unwrap();
        assert!(EditorPane::open("e".into(), &dir.join("bin")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_from_disk_picks_up_an_external_edit_and_clears_the_dirty_mark() {
        let dir = std::env::temp_dir().join(format!("mtty-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let mut p = EditorPane::open("e".into(), &file).unwrap();
        // An edit of our own leaves the pane dirty.
        p.doc.set_selection(Selection::cursor(7));
        p.type_text("X");
        assert!(p.doc.is_modified());
        assert!(p.title().ends_with('\u{25cf}'));
        // Something else rewrites the file.
        std::fs::write(&file, "one\nTWO\n").unwrap();
        assert!(p.reload_from_disk());
        assert_eq!(p.doc.rope().to_string(), "one\nTWO\n");
        assert!(!p.doc.is_modified(), "the text now matches disk");
        assert_eq!(p.title(), "a.txt");
        assert!(p.disk.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn view_mode_panes_ignore_external_edits() {
        let (mut p, _path, _text) = view_of(10);
        assert!(!p.reload_from_disk());
    }

    #[test]
    fn vim_mode_drives_normal_and_insert() {
        let mut p = pane("abc\n");
        p.set_vim(true);
        assert_eq!(p.vim_mode(), Some(mtty_editor::vim::Mode::Normal));
        // `x` deletes the char under the caret.
        p.vim_key(mtty_editor::vim::Key::Char('x'));
        assert_eq!(p.doc.rope().to_string(), "bc\n");
        // `i` enters insert; a char is typed.
        p.vim_key(mtty_editor::vim::Key::Char('i'));
        assert_eq!(p.vim_mode(), Some(mtty_editor::vim::Mode::Insert));
        p.vim_key(mtty_editor::vim::Key::Char('Z'));
        assert_eq!(p.doc.rope().to_string(), "Zbc\n");
    }

    #[test]
    fn folding_hides_lines_and_navigation_skips_them() {
        let mut p = pane("fn a() {\n    one();\n    two();\n}\nfn b() {}\n");
        p.resize(40, 10);
        p.sync_syntax();
        p.doc.set_selection(Selection::cursor(0));
        assert!(p.run(Command::Fold));
        // The hidden body (lines 1-3) is gone: row 1 is the next function.
        assert_eq!(p.line_at_row(0), Some(0));
        assert_eq!(p.line_at_row(1), Some(4));
        for row in 0..8 {
            assert!(!matches!(p.line_at_row(row), Some(1..=3)));
        }
        // Down from the folded header skips the hidden body.
        p.doc.set_selection(Selection::cursor(0));
        p.run(Command::Move(Motion::Down, false));
        assert_eq!(p.caret_line(), 4);
        // Unfolding brings the body back.
        p.run(Command::UnfoldAll);
        assert_eq!(p.line_at_row(1), Some(1));
    }

    #[test]
    fn remote_panes_load_bytes_and_report_their_host() {
        let mut p = EditorPane::open_remote(
            "e1".into(),
            "build-host".into(),
            "/etc/hosts".into(),
            b"127.0.0.1 localhost\n",
        )
        .unwrap();
        assert_eq!(
            p.remote.as_ref().map(|r| r.dest.as_str()),
            Some("build-host")
        );
        assert_eq!(p.title(), "build-host:hosts");
        assert!(!p.doc.is_modified());
        // A remote save goes through the host, not the local filesystem.
        assert!(p.save().is_err());
        // `reload_from_disk` never touches the local filesystem for a remote
        // pane; the host reloads it from bytes instead.
        assert!(!p.reload_from_disk());
    }

    fn stamp(len: u64, secs: u64) -> DiskStamp {
        DiskStamp {
            len,
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
        }
    }

    #[test]
    fn remote_poll_only_reloads_an_unmodified_pane_on_a_new_stamp() {
        use RemotePollOutcome::{Ignore, KeepLocal, Reload};
        let a = stamp(10, 100);
        let b = stamp(11, 101);
        // First observation seeds the baseline; a failed probe is silent.
        assert_eq!(remote_poll_outcome(None, Some(a), false), Ignore);
        assert_eq!(remote_poll_outcome(Some(a), None, false), Ignore);
        assert_eq!(remote_poll_outcome(None, None, false), Ignore);
        // Same stamp: nothing to do.
        assert_eq!(remote_poll_outcome(Some(a), Some(a), false), Ignore);
        // A new stamp reloads when clean, keeps local text when edited.
        assert_eq!(remote_poll_outcome(Some(a), Some(b), false), Reload);
        assert_eq!(remote_poll_outcome(Some(a), Some(b), true), KeepLocal);
    }

    #[test]
    fn a_remote_reload_replaces_the_text_and_moves_the_stamp() {
        let mut p = EditorPane::open_remote(
            "e1".into(),
            "host".into(),
            "/tmp/x.rs".into(),
            b"one\ntwo\n",
        )
        .unwrap();
        p.note_remote_stamp(stamp(8, 100));
        assert!(p.reload_remote(b"one\nTWO\n", stamp(8, 101)));
        assert_eq!(p.doc.rope().to_string(), "one\nTWO\n");
        assert!(!p.doc.is_modified(), "an external reload leaves it clean");
        assert_eq!(p.remote_disk, Some(stamp(8, 101)));
        // Identical bytes are not a change.
        assert!(!p.reload_remote(b"one\nTWO\n", stamp(8, 102)));
        assert_eq!(p.remote_disk, Some(stamp(8, 102)));
    }

    #[test]
    fn a_remote_write_marks_the_document_saved_unless_it_raced_an_edit() {
        let mut p =
            EditorPane::open_remote("e1".into(), "host".into(), "/tmp/x.rs".into(), b"one\n")
                .unwrap();
        p.type_text("two");
        assert!(p.doc.is_modified());
        let sent = p.doc.to_bytes();
        p.saving = true;
        p.remote_saved(&sent);
        assert!(!p.doc.is_modified());
        assert!(!p.saving);

        // An edit made while the write ran keeps the pane modified.
        let stale = p.doc.to_bytes();
        p.type_text("!");
        p.saving = true;
        p.remote_saved(&stale);
        assert!(p.doc.is_modified());
        assert!(!p.saving);
        assert_eq!(p.remote_original.as_deref(), Some(stale.as_slice()));
    }

    #[test]
    fn a_pending_remote_pane_fills_in_from_bytes() {
        let mut p = EditorPane::open_remote_pending("e1".into(), "host".into(), "/tmp/x.rs".into());
        assert_eq!(p.total_lines(), 1);
        p.remote_loaded(b"a\nb\nc\n");
        assert_eq!(p.total_lines(), 4);
        assert!(p.syntax.is_some(), "the parse tree follows the bytes");
    }

    #[test]
    fn a_proposal_applies_as_one_transaction_and_rejects_with_undo() {
        let mut p = pane("one\ntwo\nthree\n");
        let before = p.doc.rope().to_string();
        // Replace "two" (chars 4..7) with "TWO!".
        p.propose(vec![(4, 7, "TWO!".into())], Some("agent".into()));
        assert_eq!(p.doc.rope().to_string(), "one\nTWO!\nthree\n");
        assert!(p.doc.is_modified());
        assert!(p.proposal().is_some());
        // Reject is a single undo step back to the pre-image.
        assert!(p.reject_proposal());
        assert_eq!(p.doc.rope().to_string(), before);
        assert!(p.proposal().is_none());
        assert!(!p.doc.is_modified());
        assert!(!p.reject_proposal(), "nothing left to reject");
    }

    #[test]
    fn a_new_edit_accepts_a_proposal() {
        let mut p = pane("one\ntwo\n");
        p.propose(vec![(4, 7, "TWO".into())], None);
        assert!(p.proposal().is_some());
        // Typing moves the revision on, so the next sync accepts it.
        p.type_text("!");
        p.sync_syntax();
        assert!(p.proposal().is_none());
        assert!(p.doc.rope().to_string().contains("TWO"));
        // Accept keeps the text and drops the marking.
        p.propose(vec![(0, 1, "X".into())], None);
        assert!(p.proposal().is_some());
        p.accept_proposal();
        assert!(p.proposal().is_none());
        assert!(p.doc.rope().to_string().starts_with('X'));
    }

    #[test]
    fn deleted_rows_are_reviewable_scrollable_and_not_selectable_text() {
        let original = "old one\n旧的第二行\nold three\nkept\n";
        let mut p = pane(original);
        p.resize(50, 2);
        let end = original.find("kept").unwrap();
        let end = original[..end].chars().count();
        p.propose(vec![(0, end, "new\n".into())], None);
        assert_eq!(p.view_row(0), Some((0, Some("old one"))));
        assert_eq!(p.view_row(1), Some((0, Some("旧的第二行"))));
        assert_eq!(p.hit(0, 20), 0);
        assert_eq!(p.char_under(0, 0), None);
        assert_eq!(p.row_of_line(0), None);
        p.scroll_by(2);
        assert_eq!(p.view_row(0), Some((0, Some("old three"))));
        assert_eq!(p.row_of_line(0), Some(1));
        p.scroll_by(2);
        assert_eq!(p.view_row(0), Some((1, None)));
        p.scroll_by(-3);
        assert_eq!(p.view_row(0), Some((0, Some("旧的第二行"))));
        assert!(p.reject_proposal());
        assert_eq!(p.doc.rope().to_string(), original);
        assert_eq!(p.view_row(0), Some((0, None)));
    }

    #[test]
    fn whole_file_proposals_keep_unchanged_unicode_context_and_one_undo() {
        let mut p = pane("开头\nold\n结尾\n");
        p.propose_text("开头\nnew\n结尾\n".into(), None);
        assert_eq!(p.proposal().unwrap().deleted, vec![(1, "old".into())]);
        assert_eq!(p.proposal().unwrap().lines, vec![1]);
        assert!(p.reject_proposal());
        assert_eq!(p.doc.rope().to_string(), "开头\nold\n结尾\n");
        p.propose_text("开头\nold\n结尾\n".into(), None);
        assert!(p.proposal().is_none());
    }

    #[test]
    fn an_agent_write_only_reaches_disk_when_saved_including_new_files() {
        let root = std::env::temp_dir().join(format!("mtty-agent-save-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("new.txt");
        let mut p = EditorPane::with_doc("e1".into(), path.clone(), Document::from_text(""));
        p.propose_text("新增\n".into(), None);
        assert!(!path.exists());
        assert!(p.reject_proposal());
        assert!(!path.exists());
        p.propose_text("accepted\n".into(), None);
        p.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "accepted\n");
        assert!(!p.doc.is_modified());
        assert!(p.proposal().is_none());
        p.propose_text("rejected\n".into(), None);
        assert!(p.reject_proposal());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "accepted\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
