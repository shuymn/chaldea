//! Hook emission (`docs/design/hook-ingest.md` §2.14): FakeCli runs the emitter as a CLI
//! does, `<emitter> hook-emit <agent>` with the payload on stdin, and reports each run.
//!
//! `hooks --emitter <path> [--agent <claude|codex>] --script <json>` runs a list of steps in
//! order:
//!
//! | Step | Effect |
//! |---|---|
//! | `{ "emit": <payload> }` | one emitter process with the payload on stdin; waits for its exit |
//! | `{ "concurrent": [<payload>, …] }` | one emitter each, all started, then all awaited |
//! | `{ "nested": [<step>, …] }` | a nested FakeCli `hooks` with those steps, inheriting the environment; waits for it |
//! | `{ "wait_file": "<path>" }` | waits until the file exists, at most [`WAIT_FILE`] |
//!
//! Reports: `emitted` per emitter run (`step`, `code` or null when it was killed,
//! `elapsed_ms`, `stdout_len`, `stderr_len`); `nested` per nested run (`step`, `code`);
//! `wait_file` (`step`, `found`); `script_error` for a script that does not parse.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Opts;
use crate::report::Reporter;

/// The longest an emitter run is awaited (its own watchdog ends it at 3 s).
pub(crate) const EMITTER_WAIT: Duration = Duration::from_secs(10);
/// The longest a nested FakeCli is awaited.
const NESTED_WAIT: Duration = Duration::from_secs(60);
/// The longest a `wait_file` step waits.
pub(crate) const WAIT_FILE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(10);

pub(crate) fn run(o: &Opts) -> i32 {
    let rep = super::hello(o, false);
    let Some(emitter) = o.emitter.as_deref() else {
        return 2;
    };
    let Some(Ok(Value::Array(steps))) = o.script.as_deref().map(serde_json::from_str::<Value>)
    else {
        rep.send(&json!({"event": "script_error"}));
        return 2;
    };
    let mut code = 0;
    for (i, step) in steps.iter().enumerate() {
        if !run_step(o, emitter, &rep, i, step) {
            code = 1;
        }
    }
    code
}

/// Runs one step; false when the step is not one of the table's.
fn run_step(o: &Opts, emitter: &str, rep: &Reporter, i: usize, step: &Value) -> bool {
    if let Some(p) = step.get("emit") {
        let r = start(emitter, &o.agent, p).map(Running::finish);
        report_emitted(rep, &json!(i), r);
    } else if let Some(Value::Array(ps)) = step.get("concurrent") {
        let started: Vec<_> = ps.iter().map(|p| start(emitter, &o.agent, p)).collect();
        for r in started {
            report_emitted(rep, &json!(i), r.map(Running::finish));
        }
    } else if let Some(steps @ Value::Array(_)) = step.get("nested") {
        let mut cmd = Command::new(std::env::current_exe().unwrap_or_default());
        cmd.args(["--mode", "hooks", "--role", "nested", "--emitter", emitter])
            .args(["--agent", &o.agent, "--script", &steps.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(r) = &o.report {
            cmd.args(["--report", r]);
        }
        let code = cmd
            .spawn()
            .ok()
            .and_then(|mut c| wait_bounded(&mut c, NESTED_WAIT));
        rep.send(&json!({"event": "nested", "step": i, "code": code}));
    } else if let Some(Value::String(path)) = step.get("wait_file") {
        let end = Instant::now() + WAIT_FILE;
        let mut found = std::path::Path::new(path).exists();
        while !found && Instant::now() < end {
            std::thread::sleep(POLL);
            found = std::path::Path::new(path).exists();
        }
        rep.send(&json!({"event": "wait_file", "step": i, "found": found}));
    } else {
        return false;
    }
    true
}

/// One emitter run: the process, its start, and the threads collecting its outputs.
pub(crate) struct Running {
    child: Child,
    started: Instant,
    stdout: JoinHandle<usize>,
    stderr: JoinHandle<usize>,
}

/// The result of one emitter run.
pub(crate) struct Emitted {
    /// `None`: killed after [`EMITTER_WAIT`], or exited by a signal.
    pub(crate) code: Option<i32>,
    pub(crate) elapsed: Duration,
    pub(crate) stdout_len: usize,
    pub(crate) stderr_len: usize,
}

/// Starts `<emitter> hook-emit <agent>`, writes `payload` (a JSON value; a string is sent as
/// its raw text) to its stdin, and closes it.
pub(crate) fn start(emitter: &str, agent: &str, payload: &Value) -> Option<Running> {
    let bytes = match payload {
        Value::String(s) => s.clone().into_bytes(),
        v => v.to_string().into_bytes(),
    };
    let started = Instant::now();
    let mut child = Command::new(emitter)
        .args(["hook-emit", agent])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = count(child.stdout.take());
    let stderr = count(child.stderr.take());
    if let Some(mut stdin) = child.stdin.take() {
        // The emitter may exit without reading (a missing environment): a broken pipe here
        // is expected.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    Some(Running {
        child,
        started,
        stdout,
        stderr,
    })
}

/// Counts the bytes of `r` until its EOF, on a thread.
fn count(r: Option<impl Read + Send + 'static>) -> JoinHandle<usize> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        buf.len()
    })
}

impl Running {
    /// Waits for the exit (at most [`EMITTER_WAIT`], then kills it) and its outputs.
    pub(crate) fn finish(mut self) -> Emitted {
        let code = wait_bounded(&mut self.child, EMITTER_WAIT);
        let elapsed = self.started.elapsed();
        Emitted {
            code,
            elapsed,
            stdout_len: self.stdout.join().unwrap_or(0),
            stderr_len: self.stderr.join().unwrap_or(0),
        }
    }
}

/// Reports one emitter run; `step` is the step index (`hooks`) or the hook's name (`echo`).
pub(crate) fn report_emitted(rep: &Reporter, step: &Value, r: Option<Emitted>) {
    let v = match r {
        Some(e) => json!({
            "event": "emitted",
            "step": step,
            "code": e.code,
            "elapsed_ms": u64::try_from(e.elapsed.as_millis()).unwrap_or(u64::MAX),
            "stdout_len": e.stdout_len,
            "stderr_len": e.stderr_len,
        }),
        None => json!({"event": "emitted", "step": step, "code": null, "spawn_failed": true}),
    };
    rep.send(&v);
}

/// The exit code within `within`, else kills and reaps the child: `None`.
pub(crate) fn wait_bounded(c: &mut Child, within: Duration) -> Option<i32> {
    let end = Instant::now() + within;
    loop {
        match c.try_wait() {
            Ok(Some(s)) => return s.code(),
            Ok(None) if Instant::now() < end => std::thread::sleep(POLL),
            _ => {
                let _ = c.kill();
                let _ = c.wait();
                return None;
            }
        }
    }
}

/// A random version-4 UUID in hyphenated form, from the standard library's per-process hash
/// keys (FakeCli has no random source of its own).
pub(crate) fn random_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let half = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u32(std::process::id());
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.finish()
    };
    let n = (u128::from(half(1)) << 64) | u128::from(half(2));
    let n = (n & !(0xf << 76) | (0x4 << 76)) & !(0x3 << 62) | (0x2 << 62);
    let hex = format!("{n:032x}");
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_uuid_is_v4() {
        let a = random_uuid();
        let b = random_uuid();
        assert_ne!(a, b);
        for u in [a, b] {
            assert_eq!(u.len(), 36, "{u}");
            assert_eq!(&u[14..15], "4", "{u}");
            assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"), "{u}");
            assert_eq!(u.matches('-').count(), 4, "{u}");
        }
    }
}
