//! Paste corpus, validator, newline mapping, bracketed-paste encoder (§4.4).

use std::time::{Duration, Instant};

use serde::Serialize;

use crate::pty::{Guard, WriterTx};

pub const PASTE_START: &[u8] = b"\x1b[200~";
pub const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Newline {
    Cr,
    Lf,
    Crlf,
}

impl Newline {
    pub const ALL: [Newline; 3] = [Newline::Cr, Newline::Lf, Newline::Crlf];

    pub fn parse(s: &str) -> Option<Newline> {
        match s {
            "cr" => Some(Newline::Cr),
            "lf" => Some(Newline::Lf),
            "crlf" => Some(Newline::Crlf),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Newline::Cr => "cr",
            Newline::Lf => "lf",
            Newline::Crlf => "crlf",
        }
    }
    fn bytes(self) -> &'static [u8] {
        match self {
            Newline::Cr => b"\r",
            Newline::Lf => b"\n",
            Newline::Crlf => b"\r\n",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: String,
    pub text: String,
    /// Part of the C-1 required subset.
    pub required: bool,
    /// Too large for real CLIs (> 8 KiB).
    pub fake_only: bool,
}

fn sized(target: usize) -> String {
    let unit = "日本語テキスト𠮷😀abc\n";
    let mut s = String::new();
    while s.len() + unit.len() <= target {
        s.push_str(unit);
    }
    while s.len() < target {
        s.push('x');
    }
    s
}

pub fn corpus() -> Vec<Entry> {
    let e = |id: &str, text: &str, required: bool| Entry {
        id: id.into(),
        text: text.into(),
        required,
        fake_only: false,
    };
    let mut v = vec![
        e(
            "ascii",
            "Hello, world! 0123456789 ~!@#$%^&*()_+-=[]{}|;':\",./<>?` tab\there",
            false,
        ),
        e("japanese", "日本語の入力テスト、句読点。「括弧」", true),
        e("halfwidth_kana", "ｶﾀｶﾅ", false),
        e("astral", "𠮷野家", true),
        e("emoji", "😀", false),
        e("emoji_zwj", "👨\u{200d}👩\u{200d}👧\u{200d}👦", false),
        e("emoji_flag", "🇯🇵", false),
        e("emoji_vs16", "❤\u{fe0f}", false),
        e("decomposed_kana", "か\u{3099}", true),
        e("decomposed_latin", "e\u{301}", true),
        e(
            "multiline_lf",
            "一行目の日本語\n二行目、句読点。\n三行目「括弧」",
            true,
        ),
        e(
            "multiline_crlf",
            "一行目の日本語\r\n二行目、句読点。\r\n三行目「括弧」",
            false,
        ),
        e("size_4k_plus_1", &sized(4097), false),
    ];
    v.push(Entry {
        id: "size_64k".into(),
        text: sized(64 * 1024),
        required: false,
        fake_only: true,
    });
    v
}

/// Rejected payload: offset and value of the first forbidden byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejected {
    pub offset: usize,
    pub byte: u8,
}

/// Rejects ESC and every C0 control other than TAB, CR, LF.
pub fn validate(payload: &[u8]) -> Result<(), Rejected> {
    match payload
        .iter()
        .position(|&b| b < 0x20 && !matches!(b, b'\t' | b'\r' | b'\n'))
    {
        Some(offset) => Err(Rejected {
            offset,
            byte: payload[offset],
        }),
        None => Ok(()),
    }
}

/// LF and CRLF both map to `mode`; other bytes (including a lone CR) are kept.
pub fn map_newlines(payload: &[u8], mode: Newline) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + payload.len() / 8);
    let mut i = 0;
    while i < payload.len() {
        match payload[i] {
            b'\r' if payload.get(i + 1) == Some(&b'\n') => {
                out.extend_from_slice(mode.bytes());
                i += 2;
            }
            b'\n' => {
                out.extend_from_slice(mode.bytes());
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    out
}

/// Logical form for real-CLI comparison: CRLF and CR become LF.
pub fn normalize_lf(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\r' {
            out.push(b'\n');
            i += if s.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

/// Bracketed paste wire bytes split into `chunk`-byte writes (0 = one write).
pub fn encode(payload: &[u8], mode: Newline, chunk: usize) -> Vec<Vec<u8>> {
    let mut wire = PASTE_START.to_vec();
    wire.extend_from_slice(&map_newlines(payload, mode));
    wire.extend_from_slice(PASTE_END);
    if chunk == 0 {
        vec![wire]
    } else {
        wire.chunks(chunk).map(<[u8]>::to_vec).collect()
    }
}

/// Bytes for ungated `write_key` (owner keypad / probe client). `w-` names use
/// win32-input-mode encoding (`CSI Vk;Sc;Uc;Kd;Cs;Rc _`, key-down then key-up).
pub fn key_bytes(name: &str) -> Option<Vec<u8>> {
    let w = |vk: u32, sc: u32, uc: u32, cs: u32| {
        format!("\x1b[{vk};{sc};{uc};1;{cs};1_\x1b[{vk};{sc};{uc};0;{cs};1_").into_bytes()
    };
    Some(match name {
        "down" => b"\x1b[B".to_vec(),
        "enter" => b"\r".to_vec(),
        "esc" => b"\x1b".to_vec(),
        "w-down" => w(0x28, 0x50, 0, 0x100),
        "w-enter" => w(0x0d, 0x1c, 0x0d, 0),
        "w-esc" => w(0x1b, 0x01, 0x1b, 0),
        _ => return None,
    })
}

#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    /// `sent`, `rejected`, `readiness_gate_failed` (unconfirmed), `write_timeout`, `write_error`.
    pub state: String,
    pub rejected: Option<Rejected>,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub detail: Option<String>,
}

pub struct PasteOpts {
    pub mode: Newline,
    pub chunk: usize,
    pub enter_delay: Duration,
    pub write_deadline: Instant,
}

/// Validate → gate → bracketed write → delay → CR as a separate write.
/// `ready` is the launch's readiness (bound, unambiguous SessionStart); the
/// DECSET 2004 half of the gate is admitted here and re-checked by the writer
/// immediately before the first byte of the paste and of the CR.
pub fn paste(tx: &WriterTx, payload: &[u8], opts: &PasteOpts, ready: impl Fn() -> bool) -> Outcome {
    let before = tx.written.load(std::sync::atomic::Ordering::SeqCst);
    let out = |state: &str, rejected, detail: Option<String>| Outcome {
        state: state.into(),
        rejected,
        bytes_before: before,
        bytes_after: tx.written.load(std::sync::atomic::Ordering::SeqCst),
        detail,
    };
    if let Err(r) = validate(payload) {
        return out("rejected", Some(r), None);
    }
    let generation = match (ready(), tx.gate.admit()) {
        (true, Some(g)) => g,
        _ => {
            return out(
                "readiness_gate_failed",
                None,
                Some("unconfirmed; not retried".into()),
            );
        }
    };
    let guard = || Guard {
        gate: Some((tx.gate.clone(), generation)),
        deadline: opts.write_deadline,
    };
    if let Err(e) = tx.write_guarded(encode(payload, opts.mode, opts.chunk), guard()) {
        let state = match e.as_str() {
            "write_timeout" | "expired_before_start" | "gate_closed_before_start" => e.clone(),
            _ => "write_error".into(),
        };
        return out(&state, None, Some(e));
    }
    std::thread::sleep(opts.enter_delay);
    if let Err(e) = tx.write_guarded(vec![b"\r".to_vec()], guard()) {
        return out("enter_not_sent", None, Some(e));
    }
    out("sent", None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validator_rejects_esc_and_c0() {
        assert!(validate("日本語\t\r\n".as_bytes()).is_ok());
        assert_eq!(
            validate(b"ab\x1bc"),
            Err(Rejected {
                offset: 2,
                byte: 0x1b
            })
        );
        assert_eq!(validate(b"\x03"), Err(Rejected { offset: 0, byte: 3 }));
        assert_eq!(validate(b"x\x00"), Err(Rejected { offset: 1, byte: 0 }));
        assert!(validate(b"\x7f").is_ok());
    }

    #[test]
    fn newline_mapping_defined_for_lf_and_crlf() {
        assert_eq!(map_newlines(b"a\nb\r\nc\rd", Newline::Cr), b"a\rb\rc\rd");
        assert_eq!(map_newlines(b"a\nb\r\nc", Newline::Lf), b"a\nb\nc");
        assert_eq!(map_newlines(b"a\nb\r\nc", Newline::Crlf), b"a\r\nb\r\nc");
        assert_eq!(normalize_lf(b"a\r\nb\rc\n"), b"a\nb\nc\n");
    }

    #[test]
    fn corpus_sizes() {
        let c = corpus();
        assert_eq!(
            c.iter()
                .find(|e| e.id == "size_4k_plus_1")
                .unwrap()
                .text
                .len(),
            4097
        );
        assert_eq!(
            c.iter().find(|e| e.id == "size_64k").unwrap().text.len(),
            65536
        );
        assert!(c.iter().all(|e| validate(e.text.as_bytes()).is_ok()));
    }

    #[test]
    fn chunked_encoding_concatenates_to_whole() {
        let whole = encode("日本\n".as_bytes(), Newline::Crlf, 0).concat();
        assert_eq!(
            encode("日本\n".as_bytes(), Newline::Crlf, 7).concat(),
            whole
        );
        assert!(whole.starts_with(PASTE_START) && whole.ends_with(PASTE_END));
    }
}
