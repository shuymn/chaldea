//! The headless VT and its responder (`docs/design/pty-runtime.md` §2.5, §2.6), lifted
//! from `spikes/pty-hooks/src/vt.rs`: an `alacritty_terminal` `Term` whose event listener
//! turns each reply event into one reply write, fed through a parser that never buffers a
//! synchronized update, plus the packed mode word.

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, TermMode};
use alacritty_terminal::vte::ansi::{NamedColor, Processor, Rgb, Timeout};

use crate::backend::PtySize;
use crate::origin::ReplyKind;
use crate::snapshot::{Snapshot, TermModes, walk};

/// Nominal foreground reported for OSC 10, OSC 12 (cursor), and OSC 4 (palette).
pub(crate) const FG: Rgb = Rgb {
    r: 0xd0,
    g: 0xd0,
    b: 0xd0,
};
/// Nominal background reported for OSC 11.
pub(crate) const BG: Rgb = Rgb {
    r: 0x1e,
    g: 0x1e,
    b: 0x1e,
};
/// Nominal cell size for `CSI 14 t`.
pub(crate) const CELL_W: u16 = 8;
pub(crate) const CELL_H: u16 = 16;

/// vte's synchronized-update timeout that never pends, so the parser applies every action
/// as it parses it instead of buffering until `CSI ?2026l` or 150 ms.
#[derive(Debug, Default)]
pub(crate) struct NoSync;

impl Timeout for NoSync {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

/// Where the responder sends each reply: the session's reply lane.
pub(crate) type ReplyTarget = Arc<dyn Fn(ReplyKind, Vec<u8>) + Send + Sync>;

/// The event listener: one reply per reply event.
pub(crate) struct Responder {
    target: ReplyTarget,
    cols: Arc<AtomicU16>,
    rows: Arc<AtomicU16>,
}

/// Classifies a `PtyWrite` reply by its shape.
fn kind_of(reply: &str) -> ReplyKind {
    if reply == "\x1b[0n" {
        ReplyKind::Dsr5
    } else if reply.ends_with("$y") {
        ReplyKind::ModeReport
    } else if reply.starts_with("\x1b[8;") && reply.ends_with('t') {
        ReplyKind::TextAreaChars
    } else if reply.ends_with('R') {
        ReplyKind::Dsr6
    } else if reply.starts_with("\x1b[?") && reply.ends_with('c') {
        ReplyKind::Da1
    } else if reply.starts_with("\x1b[>") && reply.ends_with('c') {
        ReplyKind::Da2
    } else if reply.starts_with("\x1b[?") && reply.ends_with('u') {
        ReplyKind::KittyKeyboard
    } else {
        ReplyKind::Other
    }
}

impl EventListener for Responder {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => (self.target)(kind_of(&text), text.into_bytes()),
            Event::ColorRequest(index, fmt) => {
                // alacritty's indexes: 0..=255 palette (OSC 4), then Foreground (OSC 10),
                // Background (OSC 11), Cursor (OSC 12).
                let rgb = if index == NamedColor::Background as usize {
                    BG
                } else {
                    FG
                };
                (self.target)(ReplyKind::Color { index }, fmt(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                let ws = WindowSize {
                    num_lines: self.rows.load(Ordering::Relaxed),
                    num_cols: self.cols.load(Ordering::Relaxed),
                    cell_width: CELL_W,
                    cell_height: CELL_H,
                };
                (self.target)(ReplyKind::TextAreaPixels, fmt(ws).into_bytes());
            }
            // OSC 52 loads and stores, titles, bells, and the rest: no reply, no effect.
            _ => {}
        }
    }
}

struct Size {
    cols: usize,
    rows: usize,
}

impl From<PtySize> for Size {
    fn from(s: PtySize) -> Size {
        Size {
            cols: usize::from(s.cols()),
            rows: usize::from(s.rows()),
        }
    }
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

/// The headless VT.
pub(crate) struct Vt {
    term: Term<Responder>,
    parser: Processor<NoSync>,
    cols: Arc<AtomicU16>,
    rows: Arc<AtomicU16>,
}

impl Vt {
    pub(crate) fn new(size: PtySize, target: ReplyTarget) -> Vt {
        let cols = Arc::new(AtomicU16::new(size.cols()));
        let rows = Arc::new(AtomicU16::new(size.rows()));
        let responder = Responder {
            target,
            cols: cols.clone(),
            rows: rows.clone(),
        };
        // No scrollback: only the visible screen is ever read, and history would cost
        // tens of MiB per session at full length.
        let config = Config {
            kitty_keyboard: true,
            scrolling_history: 0,
            ..Config::default()
        };
        let term = Term::new(config, &Size::from(size), responder);
        Vt {
            term,
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    /// Parses one piece of output; reply events fire while it runs.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Resizes the grid and the size the responder reports.
    pub(crate) fn resize(&mut self, size: PtySize) {
        self.term.resize(Size::from(size));
        self.cols.store(size.cols(), Ordering::Relaxed);
        self.rows.store(size.rows(), Ordering::Relaxed);
    }

    pub(crate) fn modes(&self) -> TermModes {
        term_modes(&self.term)
    }

    /// The screen, or `None` once `deadline` passes during the copy.
    pub(crate) fn snapshot(&self, output_offset: u64, deadline: Instant) -> Option<Snapshot> {
        walk(&self.term, output_offset, deadline)
    }
}

/// The modes of `term`.
pub(crate) fn term_modes<T: EventListener>(term: &Term<T>) -> TermModes {
    TermModes::from_term(*term.mode())
}

impl TermModes {
    fn from_term(m: TermMode) -> TermModes {
        let kitty = (m & TermMode::KITTY_KEYBOARD_PROTOCOL).bits() >> 18;
        TermModes {
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            show_cursor: m.contains(TermMode::SHOW_CURSOR),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
            kitty_flags: u8::try_from(kitty & 0x1f).unwrap_or(0),
        }
    }

    fn pack(self) -> u64 {
        u64::from(self.bracketed_paste)
            | u64::from(self.alt_screen) << 1
            | u64::from(self.show_cursor) << 2
            | u64::from(self.app_cursor) << 3
            | u64::from(self.app_keypad) << 4
            | u64::from(self.kitty_flags & 0x1f) << 5
    }

    fn unpack(w: u64) -> TermModes {
        TermModes {
            bracketed_paste: w & 1 != 0,
            alt_screen: w & (1 << 1) != 0,
            show_cursor: w & (1 << 2) != 0,
            app_cursor: w & (1 << 3) != 0,
            app_keypad: w & (1 << 4) != 0,
            kitty_flags: u8::try_from((w >> 5) & 0x1f).unwrap_or(0),
        }
    }
}

/// The modes and the 2004 epoch, as one load gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeView {
    pub modes: TermModes,
    /// Increments on every change of `bracketed_paste`.
    pub bracketed_paste_epoch: u64,
}

const MODE_BITS: u32 = 10;
const MODE_MASK: u64 = (1 << MODE_BITS) - 1;

/// `TermModes` (10 bits) and the 2004 epoch (54 bits) in one atomic word, so `modes()` is
/// one tear-free load. Only the drain thread publishes, under the VT lock.
pub(crate) struct ModeWord(AtomicU64);

impl ModeWord {
    pub(crate) fn new(modes: TermModes) -> ModeWord {
        ModeWord(AtomicU64::new(modes.pack()))
    }

    pub(crate) fn raw(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }

    pub(crate) fn load(&self) -> ModeView {
        Self::view(self.raw())
    }

    pub(crate) fn view(w: u64) -> ModeView {
        ModeView {
            modes: TermModes::unpack(w & MODE_MASK),
            bracketed_paste_epoch: w >> MODE_BITS,
        }
    }

    /// Publishes `modes`, advancing the epoch when `bracketed_paste` changed.
    pub(crate) fn publish(&self, modes: TermModes) {
        let old = self.raw();
        let mut epoch = old >> MODE_BITS;
        if (old & 1 != 0) != modes.bracketed_paste {
            epoch += 1;
        }
        self.0
            .store((epoch << MODE_BITS) | modes.pack(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_word_round_trips() {
        let m = TermModes {
            bracketed_paste: true,
            alt_screen: false,
            show_cursor: true,
            app_cursor: true,
            app_keypad: false,
            kitty_flags: 0b10101,
        };
        let w = ModeWord::new(TermModes::unpack(0));
        w.publish(m);
        assert_eq!(
            w.load(),
            ModeView {
                modes: m,
                bracketed_paste_epoch: 1
            }
        );
        w.publish(m);
        assert_eq!(w.load().bracketed_paste_epoch, 1);
        w.publish(TermModes {
            bracketed_paste: false,
            ..m
        });
        assert_eq!(w.load().bracketed_paste_epoch, 2);
    }

    #[test]
    fn replies_are_classified() {
        for (s, k) in [
            ("\x1b[?6c", ReplyKind::Da1),
            ("\x1b[>0;2600;1c", ReplyKind::Da2),
            ("\x1b[0n", ReplyKind::Dsr5),
            ("\x1b[3;4R", ReplyKind::Dsr6),
            ("\x1b[?1u", ReplyKind::KittyKeyboard),
            ("\x1b[?2004;2$y", ReplyKind::ModeReport),
            ("\x1b[4;2$y", ReplyKind::ModeReport),
            ("\x1b[8;24;80t", ReplyKind::TextAreaChars),
            ("x", ReplyKind::Other),
        ] {
            assert_eq!(kind_of(s), k, "{s:?}");
        }
    }
}
