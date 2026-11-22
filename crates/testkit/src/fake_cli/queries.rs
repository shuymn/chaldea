//! `queries`: sends seven terminal queries, each with a reply bound, and reports which
//! were answered.

use std::io::{Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Instant;

use serde_json::json;

use super::Opts;

/// The query and its expected reply, as the PR 2 responder answers.
const QUERIES: &[(&str, &[u8])] = &[
    ("dsr6", b"\x1b[6n"),
    ("dsr5", b"\x1b[5n"),
    ("da1", b"\x1b[c"),
    ("da2", b"\x1b[>c"),
    ("osc10", b"\x1b]10;?\x1b\\"),
    ("osc11", b"\x1b]11;?\x1b\\"),
    ("kitty_keyboard", b"\x1b[?u"),
];

/// `ESC [ <prefix> <digits and ;> <end>` somewhere in `s`.
fn csi(s: &str, prefix: &str, end: char) -> bool {
    let lead = format!("\x1b[{prefix}");
    s.match_indices(&lead).any(|(i, _)| {
        let t = &s[i + lead.len()..];
        let n = t
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == ';')
            .count();
        n > 0 && t[n..].starts_with(end)
    })
}

/// True when `b` holds the complete expected reply for `kind`.
pub(crate) fn reply_ok(kind: &str, b: &[u8]) -> bool {
    let s = String::from_utf8_lossy(b);
    match kind {
        "dsr6" => csi(&s, "", 'R'),
        "dsr5" => s.contains("\x1b[0n"),
        "da1" => csi(&s, "?", 'c'),
        "da2" => csi(&s, ">", 'c'),
        "osc10" => s.contains("\x1b]10;rgb:d0d0/d0d0/d0d0"),
        "osc11" => s.contains("\x1b]11;rgb:1e1e/1e1e/1e1e"),
        "kitty_keyboard" => csi(&s, "?", 'u'),
        _ => false,
    }
}

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, true);
    let rep = super::hello(o, raw);
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = stdin.read(&mut buf) {
            if tx.send(buf[..n].to_vec()).is_err() {
                return;
            }
        }
    });
    let mut out = std::io::stdout();
    let mut all = true;
    let mut eof = false;
    for (kind, send) in QUERIES {
        while rx.try_recv().is_ok() {}
        let _ = out.write_all(send);
        let _ = out.flush();
        let end = Instant::now() + o.reply_timeout;
        let mut got = Vec::new();
        let mut ok = false;
        while !eof {
            match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                Ok(c) => {
                    got.extend_from_slice(&c);
                    if reply_ok(kind, &got) {
                        ok = true;
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                // stdin EOF ends the remaining waits at once.
                Err(RecvTimeoutError::Disconnected) => eof = true,
            }
        }
        all &= ok;
        rep.send(&json!({
            "event": "query",
            "kind": kind,
            "ok": ok,
            "received": String::from_utf8_lossy(&got).escape_debug().to_string(),
        }));
    }
    rep.send(&json!({"event": "queries", "all_ok": all}));
    i32::from(!all)
}
