//! `echo`: enables bracketed paste, then reports every stdin chunk as hex; with
//! `--validate-input <n>` it reads exactly `n` bytes instead and reports whether byte `i` is
//! `b'a' + (i % 26)`.
//!
//! With `--emitter <path>` (`docs/design/hook-ingest.md` §2.14) it also runs the emitter as a
//! CLI's hooks do: a `SessionStart` at start (session `--session-id`, else a random UUID),
//! then a `UserPromptSubmit` per input line (split at CR or LF, bracketed-paste markers
//! removed, empty lines skipped) with the line as `prompt`. `--nested` first starts a nested
//! FakeCli (`echo --nested-child`, this binary, inheriting the environment), which emits a
//! `SessionStart` of another session before and after this process's own.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{Value, json};

use super::Opts;
use super::hooks::{self, Running};
use crate::report::Reporter;

/// How long the parent waits for the nested FakeCli's first hook, and for its exit.
const NESTED_STEP: Duration = Duration::from_secs(10);
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
    let mut stdin = std::io::stdin().lock();
    if let Some(n) = o.validate_input {
        let (read, ok) = validate(&mut stdin, n);
        rep.send(&json!({"event": "validated", "bytes": read, "ok": ok}));
        return 0;
    }
    let mut prompts = o.emitter.as_deref().map(|e| Prompts::start(o, e, &rep));
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
    /// Emits the `SessionStart` (inside the nested sequence with `--nested`).
    fn start(o: &'a Opts, emitter: &'a str, rep: &Reporter) -> Prompts<'a> {
        let session = o.session_id.clone().unwrap_or_else(hooks::random_uuid);
        let p = Prompts {
            emitter,
            agent: &o.agent,
            session,
            line: Vec::new(),
        };
        if o.nested {
            p.nested(o, rep);
        } else {
            p.emit(&session_start(&p.session), rep);
        }
        p
    }

    fn emit(&self, payload: &Value, rep: &Reporter) {
        let name = payload["hook_event_name"].clone();
        let r = hooks::start(self.emitter, self.agent, payload).map(Running::finish);
        hooks::report_emitted(rep, &name, r);
    }

    /// The nested sequence (B-11): the nested FakeCli emits its `SessionStart`, this process
    /// emits its own, then the nested one emits again and exits.
    fn nested(&self, o: &Opts, rep: &Reporter) {
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.args(["--mode", "echo", "--nested-child", "--role", "nested-child"])
            .args(["--emitter", self.emitter, "--agent", self.agent])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(r) = &o.report {
            cmd.args(["--report", r]);
        }
        let Ok(mut child) = cmd.spawn() else {
            rep.send(&json!({"event": "nested", "code": null, "spawn_failed": true}));
            self.emit(&session_start(&self.session), rep);
            return;
        };
        let (tx, rx) = mpsc::channel();
        if let Some(out) = child.stdout.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let _ = tx.send(line);
                }
            });
        }
        let _ = rx.recv_timeout(NESTED_STEP);
        self.emit(&session_start(&self.session), rep);
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(b"go\n");
        }
        let code = hooks::wait_bounded(&mut child, NESTED_STEP);
        rep.send(&json!({"event": "nested", "code": code}));
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
/// waits for a line on stdin, and emits it again.
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
    p.emit(&session_start(&p.session), &rep);
    let mut out = std::io::stdout();
    let _ = out.write_all(b"sent\n");
    let _ = out.flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    p.emit(&session_start(&p.session), &rep);
    0
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
