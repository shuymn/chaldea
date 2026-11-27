//! Attach input before it is submitted (`docs/design/t1-attach.md` I-4, I-6, D6): the
//! marker rule over raw frames and the socket's pending queue.

use std::collections::VecDeque;

/// The bracketed-paste markers raw input never completes.
const MARKERS: [&[u8]; 2] = [b"\x1b[200~", b"\x1b[201~"];
/// Bytes of written context the marker rule keeps.
pub(crate) const TAIL: usize = 5;

/// Whether a marker ends at or after `from` in `hay` (it overlaps the bytes from `from`).
fn marker_from(hay: &[u8], from: usize) -> bool {
    MARKERS.iter().any(|m| {
        hay.windows(m.len())
            .enumerate()
            .any(|(i, w)| w == *m && i + m.len() > from)
    })
}

/// The length of the longest proper marker prefix of 2 to 5 bytes `bytes` ends with; a lone
/// ESC (1 byte) is not held.
fn partial_len(bytes: &[u8]) -> usize {
    (2..TAIL + 1)
        .rev()
        .find(|&k| k <= bytes.len() && MARKERS.iter().any(|m| bytes.ends_with(&m[..k])))
        .unwrap_or(0)
}

/// What the marker rule made of one raw frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Refused whole: with the written tail and the held bytes it would complete a marker,
    /// or it contains one.
    Refused,
    /// Written: these bytes join the pending raw input now; `held` waits for the next frame
    /// or the hold's end.
    Accepted { write: Vec<u8>, held: Vec<u8> },
}

/// The marker rule (R-6): `tail` is the launch's last written raw bytes, `held` the
/// socket's held partial marker.
pub(crate) fn filter(tail: &[u8], held: &[u8], frame: &[u8]) -> Verdict {
    let mut window = Vec::with_capacity(tail.len() + held.len() + frame.len());
    window.extend_from_slice(tail);
    window.extend_from_slice(held);
    let from = window.len();
    window.extend_from_slice(frame);
    if marker_from(&window, from) {
        return Verdict::Refused;
    }
    let mut accepted = held.to_vec();
    accepted.extend_from_slice(frame);
    let k = partial_len(&accepted);
    let held = accepted.split_off(accepted.len() - k);
    Verdict::Accepted {
        write: accepted,
        held,
    }
}

/// Whether `unit`, written after `tail`, completes or contains a marker: the final
/// decision, made in write order against the bytes the PTY received.
pub(crate) fn completes_marker(tail: &[u8], unit: &[u8]) -> bool {
    let mut window = Vec::with_capacity(tail.len() + unit.len());
    window.extend_from_slice(tail);
    window.extend_from_slice(unit);
    marker_from(&window, tail.len())
}

/// `tail` after `written`: its last [`TAIL`] bytes.
pub(crate) fn advance_tail(tail: &mut Vec<u8>, written: &[u8]) {
    tail.extend_from_slice(written);
    let drop = tail.len().saturating_sub(TAIL);
    tail.drain(..drop);
}

/// One pending item, in arrival order: raw bytes are joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Item {
    Raw(Vec<u8>),
    Paste { id: String, text: String },
}

/// The socket's pending input: raw bytes joined for the next unit and paste texts.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    items: VecDeque<Item>,
    bytes: usize,
}

impl Pending {
    pub(crate) fn raw(&mut self, b: &[u8]) {
        if b.is_empty() {
            return;
        }
        self.bytes += b.len();
        if let Some(Item::Raw(r)) = self.items.back_mut() {
            r.extend_from_slice(b);
        } else {
            self.items.push_back(Item::Raw(b.to_vec()));
        }
    }

    pub(crate) fn paste(&mut self, id: String, text: String) {
        self.bytes += text.len();
        self.items.push_back(Item::Paste { id, text });
    }

    /// The next unit's item.
    pub(crate) fn next(&mut self) -> Option<Item> {
        let it = self.items.pop_front()?;
        self.bytes -= match &it {
            Item::Raw(r) => r.len(),
            Item::Paste { text, .. } => text.len(),
        };
        Some(it)
    }

    /// Whether raw bytes are pending.
    pub(crate) fn has_raw(&self) -> bool {
        self.items.iter().any(|i| matches!(i, Item::Raw(_)))
    }

    /// Pending bytes, raw and paste.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `frames` through the rule as one socket would, releasing the held bytes at
    /// the end (the hold's end); the bytes written.
    fn run(tail: &mut Vec<u8>, frames: &[&[u8]]) -> (Vec<u8>, usize) {
        let mut held = Vec::new();
        let mut out = Vec::new();
        let mut refused = 0;
        for f in frames {
            match filter(tail, &held, f) {
                Verdict::Refused => refused += 1,
                Verdict::Accepted { write, held: h } => {
                    advance_tail(tail, &write);
                    out.extend_from_slice(&write);
                    held = h;
                }
            }
        }
        advance_tail(tail, &held);
        out.extend_from_slice(&held);
        (out, refused)
    }

    fn has_marker(b: &[u8]) -> bool {
        marker_from(b, 0)
    }

    /// I-6: both markers split at every position, through joined units: no complete marker
    /// is written, and the frame that would complete it is refused whole.
    #[test]
    fn marker_split_at_every_position() {
        for m in MARKERS {
            let whole = [b"ab".as_slice(), m, b"cd"].concat();
            for i in 0..=whole.len() {
                for j in i..=whole.len() {
                    let frames = [&whole[..i], &whole[i..j], &whole[j..]];
                    let (out, refused) = run(&mut Vec::new(), &frames);
                    assert!(!has_marker(&out), "{i} {j}: {out:?}");
                    assert!(refused >= 1, "{i} {j}");
                }
            }
            // Across sockets: the tail is the launch's.
            let mut tail = Vec::new();
            let (_, r) = run(&mut tail, &[&m[..1]]);
            assert_eq!(r, 0);
            let (out, r) = run(&mut tail, &[&m[1..]]);
            assert_eq!((out, r), (Vec::new(), 1));
        }
    }

    /// I-6: a lone trailing ESC is written at once; `ESC[` is held.
    #[test]
    fn lone_escape_immediate() {
        assert_eq!(
            filter(b"", b"", b"a\x1b"),
            Verdict::Accepted {
                write: b"a\x1b".to_vec(),
                held: Vec::new()
            }
        );
        assert_eq!(
            filter(b"", b"", b"a\x1b[20"),
            Verdict::Accepted {
                write: b"a".to_vec(),
                held: b"\x1b[20".to_vec()
            }
        );
        // An arrow key is written whole.
        assert_eq!(
            filter(b"", b"", b"\x1b[A"),
            Verdict::Accepted {
                write: b"\x1b[A".to_vec(),
                held: Vec::new()
            }
        );
        assert_eq!(filter(b"\x1b", b"", b"[200~x"), Verdict::Refused);
    }
}
