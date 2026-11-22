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

fn cell_text(cell: &GridCell) -> String {
    let mut s = String::new();
    s.push(cell.c);
    if let Some(zw) = cell.zerowidth() {
        s.extend(zw.iter());
    }
    s
}

/// The walker: a wide character is a `WideLead` and its spacer a `WideTail`; orphan
/// halves and leading wide-char spacers are blank `Single` cells. `None` once `deadline`
/// passes, checked before each row.
pub(crate) fn walk<T: EventListener>(
    term: &Term<T>,
    output_offset: u64,
    deadline: Instant,
) -> Option<Snapshot> {
    let grid = term.grid();
    let rows = grid.screen_lines();
    let cols = grid.columns();
    let mut lines = Vec::with_capacity(rows);
    for l in 0..rows {
        if Instant::now() >= deadline {
            return None;
        }
        let row = &grid[GridLine(i32::try_from(l).unwrap_or(i32::MAX))];
        let mut cells = Vec::with_capacity(cols);
        let mut c = 0;
        while c < cols {
            let cell = &row[Column(c)];
            let next_is_spacer =
                c + 1 < cols && row[Column(c + 1)].flags.contains(Flags::WIDE_CHAR_SPACER);
            if cell.flags.contains(Flags::WIDE_CHAR) && next_is_spacer {
                cells.push(Cell {
                    text: cell_text(cell),
                    width: CellWidth::WideLead,
                });
                cells.push(Cell::tail());
                c += 2;
                continue;
            }
            let orphan = cell.flags.intersects(
                Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER,
            );
            let text = cell_text(cell);
            if orphan || text.trim_matches('\0').is_empty() {
                cells.push(Cell::blank());
            } else {
                cells.push(Cell {
                    text,
                    width: CellWidth::Single,
                });
            }
            c += 1;
        }
        lines.push(Line { cells });
    }
    let p = grid.cursor.point;
    let modes = crate::vt::term_modes(term);
    Some(Snapshot {
        cols: u16::try_from(cols).unwrap_or(u16::MAX),
        rows: u16::try_from(rows).unwrap_or(u16::MAX),
        lines,
        cursor: Cursor {
            row: u16::try_from(p.line.0.max(0)).unwrap_or(0),
            col: u16::try_from(p.column.0.min(cols.saturating_sub(1))).unwrap_or(0),
            visible: modes.show_cursor,
        },
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

/// JSON rows of cell strings; trailing blank `Single` cells omitted.
fn encode_grid(lines: &[Line]) -> String {
    let rows: Vec<Value> = lines
        .iter()
        .map(|line| {
            let keep = line
                .cells
                .iter()
                .rposition(|c| !c.is_blank())
                .map_or(0, |i| i + 1);
            Value::Array(
                line.cells[..keep]
                    .iter()
                    .map(|c| Value::String(c.text.clone()))
                    .collect(),
            )
        })
        .collect();
    Value::Array(rows).to_string()
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

/// The reader-side validation of the §2.6 grid encoding: the lines, filled back to `cols`.
/// A cell is a `WideLead` when an `""` follows it; a wide character without that tail, an
/// `""` without a lead, and every other shape outside §2.6 are rejected.
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
    for (r, row) in rs.iter().enumerate() {
        let Value::Array(cs) = row else {
            return Err(GridError::RowNotAnArray { row: r });
        };
        if cs.len() > usize::from(cols) {
            return Err(GridError::RowTooLong {
                row: r,
                cells: cs.len(),
            });
        }
        let mut texts = Vec::with_capacity(cs.len());
        for (c, cell) in cs.iter().enumerate() {
            let Value::String(s) = cell else {
                return Err(GridError::NotAString { row: r, col: c });
            };
            texts.push(s.as_str());
        }
        let mut cells: Vec<Cell> = Vec::with_capacity(usize::from(cols));
        for (c, s) in texts.iter().enumerate() {
            if s.is_empty() {
                let lead =
                    c > 0 && !texts[c - 1].is_empty() && cells[c - 1].width == CellWidth::Single;
                if !lead {
                    return Err(GridError::EmptyWithoutLead { row: r, col: c });
                }
                cells[c - 1].width = CellWidth::WideLead;
                cells.push(Cell::tail());
            } else {
                cells.push(Cell {
                    text: (*s).to_owned(),
                    width: CellWidth::Single,
                });
            }
        }
        // A wide character always has its tail, also at the end of a row (a trailing
        // tail is never omitted). Width is the emulator's (`unicode-width`, as alacritty).
        if let Some(c) = cells.iter().enumerate().position(|(i, cell)| {
            cell.width == CellWidth::Single
                && cell.text.chars().next().and_then(UnicodeWidthChar::width) == Some(2)
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
