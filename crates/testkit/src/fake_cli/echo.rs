//! `echo`: enables bracketed paste and reports `bracketed_paste_on`, then reports every stdin
//! chunk as hex; with `--validate-input <n>` it reads exactly `n` bytes instead and reports
//! whether byte `i` is `b'a' + (i % 26)`.
//!
//! With `--emitter <path>` (`docs/design/hook-ingest.md` §2.14) it also runs the emitter as a
//! CLI's hooks do: a `SessionStart` at start (session `--session-id`, else a random UUID),
//! then a `UserPromptSubmit` per input line (split at CR or LF, bracketed-paste markers
//! removed, empty lines skipped) with the line as `prompt`. `--nested` first starts a nested
//! FakeCli (`echo --nested-child`, this binary, inheriting the environment and the
//! arguments of `inherited_args`), which emits a `SessionStart` of another session before
//! and after this process's own.
//!
//! The order is a handshake of exact lines, each counted only with its LF or CRLF: the child
//! writes `sent` after its first hook, the parent writes `go` after its own. A side that gets
//! EOF (an unterminated line included), another line, or nothing within [`HANDSHAKE`]
//! reports `handshake_failed` (`side`, `reason`), emits no further hook, and exits 1; so does
//! a side whose emitter could not be started (reported as `emitted` with `spawn_failed`),
//! before it writes its line. The parent ends the child first.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Opts;
use super::hooks;
use super::watch::{self, Watched};
use crate::report::Reporter;

/// How long either side of the nested handshake waits for the other's line (one emitter run,
/// at most 11 s with its reap, comes before each).
const HANDSHAKE: Duration = Duration::from_secs(20);
/// The longest the parent awaits the nested FakeCli from its launch.
const NESTED_LIFE: Duration = Duration::from_secs(60);
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

pub(crate) fn run(o: &Opts) -> i32 {
    if o.nested_child {
        return nested_child(o);
    }
    let raw = super::enter_raw(o, true);
    let rep = super::hello(o, raw);
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[?2004h");
    let _ = out.flush();
    rep.send(&json!({"event": "bracketed_paste_on"}));
    let mut stdin = std::io::stdin().lock();
    if let Some(n) = o.validate_input {
        let (read, ok) = validate(&mut stdin, n);
        rep.send(&json!({"event": "validated", "bytes": read, "ok": ok}));
        return 0;
    }
    let mut prompts = match o.emitter.as_deref() {
        Some(e) => match Prompts::start(o, e, &rep) {
            Some(p) => Some(p),
            None => return 1,
        },
        None => None,
    };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => return 0,
            Ok(n) => {
                rep.send(&json!({"event": "input", "hex": hex(&buf[..n])}));
                if let Some(p) = &mut prompts {
                    p.feed(&buf[..n], &rep);
                }
            }
        }
    }
}

/// The hooks of one `echo --emitter` session.
struct Prompts<'a> {
    emitter: &'a str,
    agent: &'a str,
    session: String,
    line: Vec<u8>,
}

impl<'a> Prompts<'a> {
    /// Emits the `SessionStart` (inside the nested sequence with `--nested`); `None` when
    /// the nested sequence failed.
    fn start(o: &'a Opts, emitter: &'a str, rep: &Reporter) -> Option<Prompts<'a>> {
        let session = o.session_id.clone().unwrap_or_else(hooks::random_uuid);
        let p = Prompts {
            emitter,
            agent: &o.agent,
            session,
            line: Vec::new(),
        };
        if o.nested {
            if !p.nested(o, rep) {
                return None;
            }
        } else {
            p.emit(&session_start(&p.session), rep);
        }
        Some(p)
    }

    /// Runs the emitter with `payload` and reports it; false when it could not be started.
    fn emit(&self, payload: &Value, rep: &Reporter) -> bool {
        let name = payload["hook_event_name"].clone();
        let r = hooks::emit_all(self.emitter, self.agent, std::slice::from_ref(payload)).pop();
        let r = r.flatten();
        let started = r.is_some();
        hooks::report_emitted(rep, &name, r.as_ref());
        started
    }

    /// The nested sequence (B-11): the nested FakeCli emits its `SessionStart`, this process
    /// emits its own, then the nested one emits again and exits. False when it failed.
    fn nested(&self, o: &Opts, rep: &Reporter) -> bool {
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.args(["--mode", "echo", "--nested-child", "--role", "nested-child"])
            .args(["--emitter", self.emitter, "--agent", self.agent])
            .args(super::inherited_args(o))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let launched = Instant::now();
        let Ok(mut child) = cmd.spawn() else {
            rep.send(&json!({"event": "nested", "code": null, "spawn_failed": true}));
            return false;
        };
        let lines = lines_of(child.stdout.take());
        let mut stdin = child.stdin.take();
        let mut w = Watched::new(child, launched, NESTED_LIFE);
        let ok = match expect_line(&lines, "sent", launched + HANDSHAKE) {
            Err(b) => {
                rep.send(
                    &json!({"event": "handshake_failed", "side": "parent", "reason": b.name()}),
                );
                false
            }
            Ok(()) => self.emit(&session_start(&self.session), rep),
        };
        if ok {
            if let Some(mut stdin) = stdin.take() {
                let _ = stdin.write_all(b"go\n");
            }
        } else {
            w.end_now();
        }
        let code = watch::settle(vec![w]).remove(0).code;
        // Closed only once the child has ended: a failing parent kills it before it can see
        // EOF and start another report line.
        drop(stdin);
        rep.send(&json!({"event": "nested", "code": code}));
        ok && code == Some(0)
    }

    /// Emits a `UserPromptSubmit` for every complete line in `bytes`.
    fn feed(&mut self, bytes: &[u8], rep: &Reporter) {
        for b in bytes {
            if *b == b'\r' || *b == b'\n' {
                let line = strip_paste(&std::mem::take(&mut self.line));
                if !line.is_empty() {
                    let prompt = String::from_utf8_lossy(&line).into_owned();
                    self.emit(
                        &json!({
                            "hook_event_name": "UserPromptSubmit",
                            "session_id": self.session,
                            "prompt": prompt,
                        }),
                        rep,
                    );
                }
            } else {
                self.line.push(*b);
            }
        }
    }
}

fn session_start(session: &str) -> Value {
    json!({
        "hook_event_name": "SessionStart",
        "session_id": session,
        "transcript_path": null,
    })
}

/// `line` without the bracketed-paste markers.
fn strip_paste(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        let rest = &line[i..];
        if let Some(m) = [PASTE_START, PASTE_END]
            .iter()
            .find(|m| rest.starts_with(m))
        {
            i += m.len();
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    out
}

/// `echo --nested-child`: emits a `SessionStart` of its own session, writes `sent` to stdout,
/// waits for the `go` line on stdin, and emits it again.
fn nested_child(o: &Opts) -> i32 {
    let rep = super::hello(o, false);
    let Some(emitter) = o.emitter.as_deref() else {
        return 2;
    };
    let p = Prompts {
        emitter,
        agent: &o.agent,
        session: hooks::random_uuid(),
        line: Vec::new(),
    };
    if !p.emit(&session_start(&p.session), &rep) {
        return 1;
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(b"sent\n");
    let _ = out.flush();
    let lines = lines_of(Some(std::io::stdin()));
    if let Err(b) = expect_line(&lines, "go", Instant::now() + HANDSHAKE) {
        rep.send(&json!({"event": "handshake_failed", "side": "child", "reason": b.name()}));
        return 1;
    }
    if !p.emit(&session_start(&p.session), &rep) {
        return 1;
    }
    0
}

/// How a handshake line failed to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Broken {
    Eof,
    Timeout,
    Unexpected,
}

impl Broken {
    fn name(self) -> &'static str {
        match self {
            Broken::Eof => "eof",
            Broken::Timeout => "timeout",
            Broken::Unexpected => "unexpected",
        }
    }
}

/// A complete handshake line, without its terminator, and when the reader had decoded it.
type Frame = (String, Instant);

/// The LF- or CRLF-terminated lines of `r`, read on a thread, each stamped after it is
/// decoded and then published;
/// the channel closes at EOF or a read error, and an unterminated last line is not sent.
fn lines_of(r: Option<impl Read + Send + 'static>) -> Receiver<Frame> {
    let (tx, rx) = mpsc::channel();
    if let Some(r) = r {
        std::thread::spawn(move || {
            let mut r = BufReader::new(r);
            loop {
                let mut line = Vec::new();
                match r.read_until(b'\n', &mut line) {
                    Ok(_) if line.ends_with(b"\n") => {}
                    _ => return,
                }
                line.pop();
                if line.ends_with(b"\r") {
                    line.pop();
                }
                let frame = (String::from_utf8_lossy(&line).into_owned(), Instant::now());
                if tx.send(frame).is_err() {
                    return;
                }
            }
        });
    }
    rx
}

/// Takes the next line of `lines`, which must be exactly `want` and stamped before `until`
/// (a line stamped at or after it is late, even when it is already queued). The decision is
/// about the frames observed and published: a line stamped before `until` but published
/// after it, its reader paused in between, is late when the receive has already timed out,
/// and in time when this receive is itself late and still finds it queued.
fn expect_line(lines: &Receiver<Frame>, want: &str, until: Instant) -> Result<(), Broken> {
    match lines.recv_timeout(until.saturating_duration_since(Instant::now())) {
        Ok((_, at)) if at >= until => Err(Broken::Timeout),
        Ok((l, _)) if l == want => Ok(()),
        Ok(_) => Err(Broken::Unexpected),
        Err(RecvTimeoutError::Disconnected) => Err(Broken::Eof),
        Err(RecvTimeoutError::Timeout) => Err(Broken::Timeout),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handshake_token_queued_after_its_deadline_is_rejected() {
        let (tx, rx) = mpsc::channel();
        let deadline = Instant::now() - Duration::from_millis(1);
        tx.send(("go".to_owned(), Instant::now())).unwrap();

        assert_eq!(expect_line(&rx, "go", deadline), Err(Broken::Timeout));
        // Completed before the deadline and read after it: in time.
        let deadline = Instant::now();
        tx.send(("go".to_owned(), deadline - Duration::from_millis(1)))
            .unwrap();
        assert_eq!(expect_line(&rx, "go", deadline), Ok(()));
    }

    #[test]
    fn a_handshake_line_must_be_exact_and_in_time() {
        let until = || Instant::now() + Duration::from_secs(5);
        let (tx, rx) = mpsc::channel();
        for l in ["go", "gone", "go ", ""] {
            tx.send((l.to_owned(), Instant::now())).unwrap();
        }
        assert_eq!(expect_line(&rx, "go", until()), Ok(()));
        for _ in 0..3 {
            assert_eq!(expect_line(&rx, "go", until()), Err(Broken::Unexpected));
        }
        // Nothing by the bound.
        let t = Instant::now();
        let soon = t + Duration::from_millis(50);
        assert_eq!(expect_line(&rx, "go", soon), Err(Broken::Timeout));
        assert!(t.elapsed() >= Duration::from_millis(50));
        drop(tx);
        assert_eq!(expect_line(&rx, "go", until()), Err(Broken::Eof));
        // CRLF and LF lines are the same line; an unterminated line is EOF.
        let rx = lines_of(Some(std::io::Cursor::new(b"sent\r\ngo\ngo".to_vec())));
        assert_eq!(expect_line(&rx, "sent", until()), Ok(()));
        assert_eq!(expect_line(&rx, "go", until()), Ok(()));
        assert_eq!(expect_line(&rx, "go", until()), Err(Broken::Eof));
    }
}
