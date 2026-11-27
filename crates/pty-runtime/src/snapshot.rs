//! Screen snapshots and the grid encoding (`docs/design/pty-runtime.md` §2.6, D9).

use std::fmt;
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line as GridLine};
use alacritty_terminal::term::cell::{Cell as GridCell, Flags};
use chaldea_core::payload::{Content, VtCursor, VtModes, VtSnapshotTaken};
use serde_json::Value;
use unicode_width::UnicodeWidthChar;

/// A copy of the visible screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    pub rows: u16,
    /// Exactly `rows` lines of the visible screen.
    pub lines: Vec<Line>,
    pub cursor: Cursor,
    pub modes: TermModes,
    /// Output bytes fed into the VT when it was taken.
    pub output_offset: u64,
}

/// Zero-based cell coordinates; `visible` is `SHOW_CURSOR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
}

/// One screen row: exactly `cols` cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub cells: Vec<Cell>,
}

/// One cell; `text` is non-empty except for a `WideTail`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub width: CellWidth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellWidth {
    Single,
    WideLead,
    WideTail,
}

impl Cell {
    fn blank() -> Cell {
        Cell {
            text: " ".to_owned(),
            width: CellWidth::Single,
        }
    }

    fn tail() -> Cell {
        Cell {
            text: String::new(),
            width: CellWidth::WideTail,
        }
    }

    fn is_blank(&self) -> bool {
        self.width == CellWidth::Single && self.text == " "
    }
}

impl Line {
    /// The cells without `WideTail`, right-trimmed.
    #[must_use]
    pub fn text(&self) -> String {
        let s: String = self
            .cells
            .iter()
            .filter(|c| c.width != CellWidth::WideTail)
            .map(|c| c.text.as_str())
            .collect();
        s.trim_end_matches(' ').to_owned()
    }
}

/// The terminal modes the daemon reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TermModes {
    /// `BRACKETED_PASTE` (DECSET 2004): the gate, `VtModes`.
    pub bracketed_paste: bool,
    /// `ALT_SCREEN` (1049): `VtModes`.
    pub alt_screen: bool,
    /// `SHOW_CURSOR` (DECTCEM): `VtCursor.visible`.
    pub show_cursor: bool,
    /// `APP_CURSOR` (DECCKM): arrow encoding for keypad and card keys.
    pub app_cursor: bool,
    /// `APP_KEYPAD` (DECKPAM).
    pub app_keypad: bool,
    /// `DISAMBIGUATE_ESC_CODES` … `REPORT_ASSOCIATED_TEXT` as 5 bits.
    pub kitty_flags: u8,
}

/// The screen's facts without its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenMeta {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Cursor,
    pub modes: TermModes,
    /// Output bytes fed into the VT when it was taken.
    pub output_offset: u64,
}

impl Snapshot {
    /// The snapshot's facts without its cells.
    #[must_use]
    pub fn meta(&self) -> ScreenMeta {
        ScreenMeta {
            cols: self.cols,
            rows: self.rows,
            cursor: self.cursor,
            modes: self.modes,
            output_offset: self.output_offset,
        }
    }
}

/// The length of `c` inside a JSON string as `serde_json` writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
        c if u32::from(c) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// A cell's text, its base character and every zero-width character, with its escaped
/// length; `None` once that length passes `room` (the copy stops there).
fn cell_text(cell: &GridCell, room: usize) -> Option<(String, usize)> {
    let mut s = String::new();
    let mut len = escaped_len(cell.c);
    if len > room {
        return None;
    }
    s.push(cell.c);
    if let Some(zw) = cell.zerowidth() {
        for &c in zw {
            len += escaped_len(c);
            if len > room {
                return None;
            }
            s.push(c);
        }
    }
    Some((s, len))
}

/// The exact length of the §2.6 encoding of the rows walked so far: brackets and row
/// separators for every row up front, then each kept cell as it is kept; a row's trailing
/// blank cells are counted only once a later cell keeps them.
struct Budget {
    limit: usize,
    used: usize,
    /// Kept cells of the current row.
    kept: usize,
    /// Blank cells after the current row's last kept cell.
    blanks: usize,
}

/// `" "` and its separator.
const BLANK_COST: usize = 4;

impl Budget {
    fn new(limit: usize, rows: usize) -> Budget {
        Budget {
            limit,
            used: 2 + rows.saturating_sub(1) + 2 * rows,
            kept: 0,
            blanks: 0,
        }
    }

    fn row(&mut self) {
        self.kept = 0;
        self.blanks = 0;
    }

    /// What keeping a cell now costs besides its escaped text: the pending blanks, its
    /// quotes, and a separator before every cell but the row's first.
    fn fixed(&self) -> usize {
        let separators = self.blanks + 1 - usize::from(self.kept == 0);
        self.blanks * (BLANK_COST - 1) + separators + 2
    }

    /// The room the copy of a cell's escaped text may take: what is not used yet, and at
    /// least one byte, a blank's (a blank is counted only once a later cell keeps it, so
    /// the exact count is `push`'s).
    fn room(&self) -> usize {
        self.limit.saturating_sub(self.used).max(1)
    }

    /// Counts a cell; false once the encoding passes the limit.
    fn push(&mut self, cell: &Cell, escaped: usize) -> bool {
        if cell.is_blank() {
            self.blanks += 1;
            return true;
        }
        self.used += self.fixed() + escaped;
        self.kept += self.blanks + 1;
        self.blanks = 0;
        self.used <= self.limit
    }
}

/// A walk's end.
pub(crate) enum Walk {
    /// Every cell, within the deadline and the budget.
    Copied(Snapshot),
    /// The encoding passed the budget: the facts only.
    OverBudget(ScreenMeta),
    /// The deadline passed.
    Late,
}

/// The walker: a wide character is a `WideLead` and its spacer a `WideTail`; orphan
/// halves and leading wide-char spacers are blank `Single` cells. `None` once `deadline`
/// passes, checked before each cell and before returning.
pub(crate) fn walk<T: EventListener>(
    term: &Term<T>,
    output_offset: u64,
    deadline: Instant,
) -> Option<Snapshot> {
    match walk_within(term, output_offset, deadline, |_| usize::MAX) {
        Walk::Copied(s) => Some(s),
        Walk::OverBudget(_) | Walk::Late => None,
    }
}

/// [`walk`] that also stops once the §2.6 encoding of the copied cells would pass
/// `budget(meta)` bytes, checked per cell while the cell's text is copied.
pub(crate) fn walk_within<T: EventListener>(
    term: &Term<T>,
    output_offset: u64,
    deadline: Instant,
    budget: impl FnOnce(&ScreenMeta) -> usize,
) -> Walk {
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let p = grid.cursor.point;
    let modes = crate::vt::term_modes(term);
    let meta = ScreenMeta {
        cols: u16::try_from(cols).unwrap_or(u16::MAX),
        rows: u16::try_from(rows).unwrap_or(u16::MAX),
        cursor: Cursor {
            row: u16::try_from(p.line.0.max(0)).unwrap_or(0),
            col: u16::try_from(p.column.0.min(cols.saturating_sub(1))).unwrap_or(0),
            visible: modes.show_cursor,
        },
        modes,
        output_offset,
    };
    let mut b = Budget::new(budget(&meta), rows);
    if b.used > b.limit {
        return Walk::OverBudget(meta);
    }
    let mut lines = Vec::with_capacity(rows);
    for l in 0..rows {
        let row = &grid[GridLine(i32::try_from(l).unwrap_or(i32::MAX))];
        let mut cells = Vec::with_capacity(cols);
        b.row();
        let mut c = 0;
        while c < cols {
            if Instant::now() >= deadline {
                return Walk::Late;
            }
            let cell = &row[Column(c)];
            let next_is_spacer =
                c + 1 < cols && row[Column(c + 1)].flags.contains(Flags::WIDE_CHAR_SPACER);
            let Some((text, escaped)) = cell_text(cell, b.room()) else {
                return Walk::OverBudget(meta);
            };
            if cell.flags.contains(Flags::WIDE_CHAR) && next_is_spacer {
                let lead = Cell {
                    text,
                    width: CellWidth::WideLead,
                };
                if !b.push(&lead, escaped) || !b.push(&Cell::tail(), 0) {
                    return Walk::OverBudget(meta);
                }
                cells.push(lead);
                cells.push(Cell::tail());
                c += 2;
                continue;
            }
            let orphan = cell.flags.intersects(
                Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            );
            let out = if orphan || text.trim_matches('\0').is_empty() {
                Cell::blank()
            } else {
                Cell {
                    text,
                    width: CellWidth::Single,
                }
            };
            let len = if out.is_blank() { 1 } else { escaped };
            if !b.push(&out, len) {
                return Walk::OverBudget(meta);
            }
            cells.push(out);
            c += 1;
        }
        lines.push(Line { cells });
    }
    // A row can be long (zero-width characters join their base cell): the copy must still
    // have ended by the deadline.
    if Instant::now() >= deadline {
        return Walk::Late;
    }
    Walk::Copied(Snapshot {
        cols: meta.cols,
        rows: meta.rows,
        lines,
        cursor: meta.cursor,
        modes,
        output_offset,
    })
}

impl Snapshot {
    /// The `vt_snapshot_taken` payload, with the grid in the §2.6 encoding.
    #[must_use]
    pub fn to_payload(&self) -> VtSnapshotTaken {
        VtSnapshotTaken {
            grid: Content::text(encode_grid(&self.lines)),
            cols: self.cols,
            rows: self.rows,
            cursor: VtCursor {
                row: self.cursor.row,
                col: self.cursor.col,
                visible: self.cursor.visible,
            },
            modes: VtModes {
                bracketed_paste: self.modes.bracketed_paste,
                alternate_screen: self.modes.alt_screen,
            },
        }
    }
}

/// A row's cells up to its last non-blank one: trailing blank `Single` cells omitted.
fn kept(line: &Line) -> &[Cell] {
    let keep = line
        .cells
        .iter()
        .rposition(|c| !c.is_blank())
        .map_or(0, |i| i + 1);
    &line.cells[..keep]
}

/// The §2.6 rows of `lines`: each row's cell strings, trailing blank `Single` cells
/// omitted.
#[must_use]
pub fn grid_rows(lines: &[Line]) -> Vec<Vec<String>> {
    lines
        .iter()
        .map(|line| kept(line).iter().map(|c| c.text.clone()).collect())
        .collect()
}

/// JSON rows of cell strings; trailing blank `Single` cells omitted.
fn encode_grid(lines: &[Line]) -> String {
    let rows: Vec<Vec<&str>> = lines
        .iter()
        .map(|line| kept(line).iter().map(|c| c.text.as_str()).collect())
        .collect();
    serde_json::to_string(&rows).expect("string rows always serialize")
}

/// Why a grid is unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GridError {
    NotJson(String),
    NotAnArray,
    RowCount { expected: u16, got: usize },
    RowNotAnArray { row: usize },
    RowTooLong { row: usize, cells: usize },
    NotAString { row: usize, col: usize },
    EmptyWithoutLead { row: usize, col: usize },
    LeadWithoutTail { row: usize, col: usize },
}

impl fmt::Display for GridError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unreadable snapshot grid: {self:?}")
    }
}

impl std::error::Error for GridError {}

/// Whether the emulator lays out a cell whose text is `text` on two cells: the width of
/// its base character (`unicode-width`, as alacritty).
fn is_wide(text: &str) -> bool {
    text.chars().next().and_then(UnicodeWidthChar::width) == Some(2)
}

/// The reader-side validation of the §2.6 grid encoding: the lines, filled back to `cols`.
/// A wide character followed by `""` is a `WideLead` and its `WideTail`; a wide character
/// without that tail, an `""` after anything but a wide character, and every other shape
/// outside §2.6 are rejected.
pub fn decode_grid(grid: &str, cols: u16, rows: u16) -> Result<Vec<Line>, GridError> {
    let v: Value = serde_json::from_str(grid).map_err(|e| GridError::NotJson(e.to_string()))?;
    let Value::Array(rs) = v else {
        return Err(GridError::NotAnArray);
    };
    if rs.len() != usize::from(rows) {
        return Err(GridError::RowCount {
            expected: rows,
            got: rs.len(),
        });
    }
    let mut lines = Vec::with_capacity(rs.len());
    for (r, row) in rs.into_iter().enumerate() {
        let Value::Array(cs) = row else {
            return Err(GridError::RowNotAnArray { row: r });
        };
        if cs.len() > usize::from(cols) {
            return Err(GridError::RowTooLong {
                row: r,
                cells: cs.len(),
            });
        }
        // Every cell is checked to be a string before the wide-pair checks.
        if let Some(c) = cs.iter().position(|cell| !cell.is_string()) {
            return Err(GridError::NotAString { row: r, col: c });
        }
        let mut cells: Vec<Cell> = Vec::with_capacity(usize::from(cols));
        for (c, cell) in cs.into_iter().enumerate() {
            let Value::String(text) = cell else {
                unreachable!("checked above");
            };
            if text.is_empty() {
                let lead =
                    c > 0 && cells[c - 1].width == CellWidth::Single && is_wide(&cells[c - 1].text);
                if !lead {
                    return Err(GridError::EmptyWithoutLead { row: r, col: c });
                }
                cells[c - 1].width = CellWidth::WideLead;
                cells.push(Cell::tail());
            } else {
                cells.push(Cell {
                    text,
                    width: CellWidth::Single,
                });
            }
        }
        // A wide character always has its tail, also at the end of a row (a trailing
        // tail is never omitted). Width is the emulator's (`unicode-width`, as alacritty).
        if let Some(c) = cells.iter().enumerate().position(|(i, cell)| {
            cell.width == CellWidth::Single
                && is_wide(&cell.text)
                && cells
                    .get(i + 1)
                    .is_none_or(|n| n.width != CellWidth::WideTail)
        }) {
            return Err(GridError::LeadWithoutTail { row: r, col: c });
        }
        cells.resize_with(usize::from(cols), Cell::blank);
        lines.push(Line { cells });
    }
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::{GridCell, cell_text};

    /// D12: the copy of a cell's text stops inside a long zero-width run once the run
    /// passes the room; it does not copy the run first.
    #[test]
    fn cell_text_stops_inside_zero_width_run() {
        let mut cell = GridCell {
            c: 'e',
            ..GridCell::default()
        };
        for _ in 0..20_000 {
            cell.push_zerowidth('\u{301}');
        }
        assert_eq!(cell_text(&cell, 4096), None);
        let (text, len) = cell_text(&cell, usize::MAX).unwrap();
        assert_eq!(text.chars().count(), 20_001);
        assert_eq!(len, 1 + 20_000 * '\u{301}'.len_utf8());
    }
}
