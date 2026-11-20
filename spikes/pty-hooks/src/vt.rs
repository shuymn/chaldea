//! Headless VT (§4.4): `alacritty_terminal` Term + an EventListener that
//! answers queries through the single writer channel, plus the snapshot walker.

use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{NamedColor, Processor, Rgb};
use serde::Serialize;

use crate::pty::{WriteMsg, WriterTx};

pub const FG: Rgb = Rgb {
    r: 0xd0,
    g: 0xd0,
    b: 0xd0,
};
pub const BG: Rgb = Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x1e,
};
pub const CELL_W: u16 = 8;
pub const CELL_H: u16 = 16;

#[derive(Debug, Clone, Serialize)]
pub struct QueryEntry {
    pub kind: String,
    pub query_ms: f64,
    pub reply: Option<String>,
    pub harness_wrote: bool,
    pub latency_ms: Option<f64>,
}

pub type QueryLog = Arc<Mutex<Vec<QueryEntry>>>;

pub struct Responder {
    tx: WriterTx,
    enabled: bool,
    log: QueryLog,
    t0: Instant,
    chunk_at: Arc<Mutex<Instant>>,
    cols: AtomicU16,
    rows: AtomicU16,
}

fn kind_of(reply: &str) -> String {
    let k = if reply == "\x1b[0n" {
        "dsr5"
    } else if reply.ends_with('R') {
        "dsr6"
    } else if reply.starts_with("\x1b[?") && reply.ends_with('c') {
        "da1"
    } else if reply.starts_with("\x1b[>") && reply.ends_with('c') {
        "da2"
    } else if reply.starts_with("\x1b[?") && reply.ends_with('u') {
        "kitty_keyboard"
    } else {
        "other"
    };
    k.to_string()
}

impl Responder {
    fn reply(&self, kind: String, text: String) {
        let at = *self.chunk_at.lock().unwrap_or_else(|e| e.into_inner());
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let id = log.len();
        log.push(QueryEntry {
            kind,
            query_ms: at.duration_since(self.t0).as_secs_f64() * 1000.0,
            reply: self.enabled.then(|| text.clone()),
            harness_wrote: false,
            latency_ms: None,
        });
        drop(log);
        if self.enabled {
            self.tx.send(WriteMsg::Reply {
                bytes: text.into_bytes(),
                log_id: id,
                query_at: at,
            });
        }
    }
}

impl EventListener for Responder {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.reply(kind_of(&text), text),
            Event::ColorRequest(index, fmt) => {
                // Indexes are alacritty's: 0..=255 palette (OSC 4), then NamedColor
                // Foreground (OSC 10), Background (OSC 11), Cursor (OSC 12).
                let fg = NamedColor::Foreground as usize;
                let bg = NamedColor::Background as usize;
                let cursor = NamedColor::Cursor as usize;
                let (kind, rgb) = match index {
                    i if i == fg => ("osc10".to_string(), FG),
                    i if i == bg => ("osc11".to_string(), BG),
                    i if i == cursor => ("osc12".to_string(), FG),
                    i => (format!("osc4;{i}"), FG),
                };
                self.reply(kind, fmt(rgb));
            }
            Event::TextAreaSizeRequest(fmt) => {
                let ws = WindowSize {
                    num_lines: self.rows.load(Ordering::Relaxed),
                    num_cols: self.cols.load(Ordering::Relaxed),
                    cell_width: CELL_W,
                    cell_height: CELL_H,
                };
                self.reply("text_area_size".into(), fmt(ws));
            }
            _ => {}
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Snapshot {
    pub cols: usize,
    pub rows: usize,
    pub lines: Vec<String>,
    pub cursor: (usize, usize),
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
}

pub struct Vt {
    term: Term<Responder>,
    parser: Processor,
    chunk_at: Arc<Mutex<Instant>>,
}

impl Vt {
    pub fn new(
        cols: u16,
        rows: u16,
        tx: WriterTx,
        enabled: bool,
        log: QueryLog,
        t0: Instant,
    ) -> Vt {
        let chunk_at = Arc::new(Mutex::new(Instant::now()));
        let responder = Responder {
            tx,
            enabled,
            log,
            t0,
            chunk_at: chunk_at.clone(),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
        };
        let config = Config {
            kitty_keyboard: true,
            ..Config::default()
        };
        let term = Term::new(
            config,
            &Size {
                cols: cols as usize,
                rows: rows as usize,
            },
            responder,
        );
        Vt {
            term,
            parser: Processor::new(),
            chunk_at,
        }
    }

    /// Feeds one chunk read from the PTY at `at` (replies are timed from `at`).
    pub fn feed(&mut self, bytes: &[u8], at: Instant) {
        *self.chunk_at.lock().unwrap_or_else(|e| e.into_inner()) = at;
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn bracketed_paste(&self) -> bool {
        self.term.mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Visible grid: per row `cell.c` + zero-width chars, skipping spacer cells, right-trimmed.
    pub fn snapshot(&self) -> Snapshot {
        let grid = self.term.grid();
        let rows = grid.screen_lines();
        let cols = grid.columns();
        let mut lines = Vec::with_capacity(rows);
        for l in 0..rows {
            let row = &grid[Line(l as i32)];
            let mut s = String::new();
            for c in 0..cols {
                let cell = &row[Column(c)];
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                s.push(cell.c);
                if let Some(zw) = cell.zerowidth() {
                    s.extend(zw.iter());
                }
            }
            lines.push(s.trim_end_matches(' ').to_string());
        }
        let p = grid.cursor.point;
        Snapshot {
            cols,
            rows,
            lines,
            cursor: (p.line.0.max(0) as usize, p.column.0),
            alt_screen: self.term.mode().contains(TermMode::ALT_SCREEN),
            bracketed_paste: self.bracketed_paste(),
            decided_by: None,
        }
    }
}
