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
//! `elapsed_ms`, `stdout_len`, `stderr_len`, each null when the pipe was still open at the
//! run's end); `nested` per nested run (`step`, `code`); `wait_file` (`step`, `found`);
//! `script_error` for a script that does not parse.
//!
//! Every child's bounds are measured from its own launch: an emitter is killed at
//! [`EMITTER_WAIT`], and its output pipes and reap are awaited until [`REAP`] after that.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Opts;
use crate::report::Reporter;

/// The longest an emitter run is awaited from its launch (its own watchdog ends it at 3 s).
pub(crate) const EMITTER_WAIT: Duration = Duration::from_secs(10);
/// How long past a child's exit bound its output pipes and its reap are awaited.
pub(crate) const REAP: Duration = Duration::from_secs(1);
/// The longest a nested FakeCli is awaited from its launch.
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
            .args(super::inherited_args(o))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let launched = Instant::now();
        let code = cmd
            .spawn()
            .ok()
            .and_then(|mut c| wait_until(&mut c, launched + NESTED_WAIT));
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

/// One emitter run: the process, its launch and exit bound, and the threads counting its
/// outputs.
pub(crate) struct Running {
    child: Child,
    started: Instant,
    /// The exit is awaited until it; past it the child is killed.
    deadline: Instant,
    stdout: Receiver<usize>,
    stderr: Receiver<usize>,
}

/// The result of one emitter run.
pub(crate) struct Emitted {
    /// `None`: killed at its deadline, or exited by a signal.
    pub(crate) code: Option<i32>,
    pub(crate) elapsed: Duration,
    /// `None`: the pipe was still open [`REAP`] after the deadline (a descendant holds it).
    pub(crate) stdout_len: Option<usize>,
    pub(crate) stderr_len: Option<usize>,
}

/// Starts `<emitter> hook-emit <agent>`, writes `payload` (a JSON value; a string is sent as
/// its raw text) to its stdin, and closes it.
pub(crate) fn start(emitter: &str, agent: &str, payload: &Value) -> Option<Running> {
    let bytes = match payload {
        Value::String(s) => s.clone().into_bytes(),
        v => v.to_string().into_bytes(),
    };
    let mut cmd = Command::new(emitter);
    cmd.args(["hook-emit", agent]);
    Running::launch(&mut cmd, bytes, EMITTER_WAIT)
}

/// Counts the bytes of `r` until its EOF, on a thread; the count arrives on the channel.
fn count(r: Option<impl Read + Send + 'static>) -> Receiver<usize> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        let _ = tx.send(buf.len());
    });
    rx
}

impl Running {
    /// Starts `cmd` with piped stdio, writes `input` to its stdin and closes it; the exit is
    /// awaited until `wait` after this launch.
    pub(crate) fn launch(cmd: &mut Command, input: Vec<u8>, wait: Duration) -> Option<Running> {
        let started = Instant::now();
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let stdout = count(child.stdout.take());
        let stderr = count(child.stderr.take());
        if let Some(mut stdin) = child.stdin.take() {
            // The emitter may exit without reading (a missing environment): a broken pipe
            // here is expected.
            std::thread::spawn(move || {
                let _ = stdin.write_all(&input);
            });
        }
        Some(Running {
            child,
            started,
            deadline: started + wait,
            stdout,
            stderr,
        })
    }

    /// Waits for the exit until the deadline (then kills it), and for its outputs and reap
    /// until [`REAP`] after the deadline.
    pub(crate) fn finish(mut self) -> Emitted {
        let code = wait_until(&mut self.child, self.deadline);
        let elapsed = self.started.elapsed();
        let end = self.deadline + REAP;
        let len = |rx: &Receiver<usize>| {
            rx.recv_timeout(end.saturating_duration_since(Instant::now()))
                .ok()
        };
        Emitted {
            code,
            elapsed,
            stdout_len: len(&self.stdout),
            stderr_len: len(&self.stderr),
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

/// The exit code by `deadline`, else kills the child and reaps it until [`REAP`] after the
/// deadline: `None`.
pub(crate) fn wait_until(c: &mut Child, deadline: Instant) -> Option<i32> {
    loop {
        match c.try_wait() {
            Ok(Some(s)) => return s.code(),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(POLL),
            _ => break,
        }
    }
    let _ = c.kill();
    let end = deadline + REAP;
    while matches!(c.try_wait(), Ok(None)) && Instant::now() < end {
        std::thread::sleep(POLL);
    }
    None
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

    #[cfg(unix)]
    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    /// Three runs that never exit, all launched before the first is finished: each is
    /// killed at its own launch's bound, so the three end together.
    #[cfg(unix)]
    #[test]
    fn each_run_is_bounded_from_its_launch() {
        let wait = Duration::from_millis(300);
        let t = Instant::now();
        let runs: Vec<Running> = (0..3)
            .map(|_| Running::launch(&mut sh("exec sleep 30"), Vec::new(), wait).unwrap())
            .collect();
        for r in runs {
            let e = r.finish();
            assert_eq!(e.code, None);
            assert!(e.elapsed >= wait, "{:?}", e.elapsed);
            assert_eq!((e.stdout_len, e.stderr_len), (Some(0), Some(0)));
        }
        // Restated: 300 ms each; serial bounds would end the last at 900 ms.
        assert!(
            t.elapsed() < Duration::from_millis(600),
            "{:?}",
            t.elapsed()
        );
    }

    /// A run that exits while a descendant holds its stdout: the count is given up at the
    /// run's end, and the descendant (its pid written to a file) is killed here.
    #[cfg(unix)]
    #[test]
    fn an_output_held_open_is_bounded() {
        let dir = std::env::temp_dir().join(format!("chaldea-hooks-{}", random_uuid()));
        std::fs::create_dir(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!(
            "sleep 30 2>/dev/null & echo $! > '{}'; exit 0",
            pidfile.display()
        );
        let wait = Duration::from_millis(300);
        let t = Instant::now();
        let e = Running::launch(&mut sh(&script), Vec::new(), wait)
            .unwrap()
            .finish();
        let took = t.elapsed();
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid).unwrap(),
            rustix::process::Signal::KILL,
        );
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(e.code, Some(0));
        assert_eq!(e.stdout_len, None, "stdout is held by the descendant");
        assert_eq!(e.stderr_len, Some(0));
        // Restated: the 300 ms bound plus the 1 s reap.
        assert!(took < Duration::from_millis(2000), "{took:?}");
        assert!(took >= Duration::from_millis(1300), "{took:?}");
    }

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
