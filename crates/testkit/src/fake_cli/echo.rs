//! `echo`: enables bracketed paste, then reports every stdin chunk as hex; with
//! `--validate-input <n>` it reads exactly `n` bytes instead and reports whether byte `i` is
//! `b'a' + (i % 26)`.

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
    if let Some(n) = o.validate_input {
        let (read, ok) = validate(&mut stdin, n);
        rep.send(&json!({"event": "validated", "bytes": read, "ok": ok}));
        return 0;
    }
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => rep.send(&json!({"event": "input", "hex": hex(&buf[..n])})),
        }
    }
}

/// Reads up to `n` bytes (until EOF or an error); how many, and whether all were present
/// and followed the pattern.
fn validate(r: &mut impl Read, n: usize) -> (usize, bool) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut read = 0usize;
    let mut ok = true;
    while read < n {
        let want = (n - read).min(buf.len());
        match r.read(&mut buf[..want]) {
            Ok(0) | Err(_) => return (read, false),
            Ok(k) => {
                for (j, b) in buf[..k].iter().enumerate() {
                    let i = read + j;
                    let expected = b'a' + u8::try_from(i % 26).unwrap_or(0);
                    ok &= *b == expected;
                }
                read += k;
            }
        }
    }
    (read, ok)
}

pub(crate) fn hex(b: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}
