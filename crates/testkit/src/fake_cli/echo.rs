//! `echo`: enables bracketed paste, then reports every stdin chunk as hex.

use std::io::{Read, Write};

use serde_json::json;

use super::Opts;

pub(crate) fn run(o: &Opts) -> i32 {
    let raw = super::enter_raw(o, true);
    let rep = super::hello(o, raw);
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?2004h");
    let _ = out.flush();
    let mut stdin = std::io::stdin().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => rep.send(&json!({"event": "input", "hex": hex(&buf[..n])})),
        }
    }
}

pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}
