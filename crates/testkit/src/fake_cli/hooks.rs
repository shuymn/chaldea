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
//! Reports: `emitted` per emitter run (`step`, `code` or null when it did not exit by its
//! deadline, `elapsed_ms`, `stdout_len`, `stderr_len`, each null when that pipe did not close
//! by the deadline plus the reap); `nested` per nested run (`step`, `code`); `wait_file`
//! (`step`, `found`); `script_error` for a script that does not parse.
//!
//! Every child's deadline is measured from its own launch and enforced by
//! [`watch::settle`]: an emitter is killed [`EMITTER_WAIT`] after its launch.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::Opts;
use super::watch::{self, Watched};
use crate::report::Reporter;

/// The longest an emitter run is awaited from its launch (its own watchdog ends it at 3 s).
pub(crate) const EMITTER_WAIT: Duration = Duration::from_secs(10);
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
        for r in emit_all(emitter, &o.agent, std::slice::from_ref(p)) {
            report_emitted(rep, &json!(i), r.as_ref());
        }
    } else if let Some(Value::Array(ps)) = step.get("concurrent") {
        for r in emit_all(emitter, &o.agent, ps) {
            report_emitted(rep, &json!(i), r.as_ref());
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
        let code = cmd.spawn().ok().and_then(|c| {
            let w = Watched::new(c, launched, NESTED_WAIT);
            watch::settle(vec![w]).remove(0).code
        });
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

/// The result of one emitter run.
pub(crate) struct Emitted {
    /// `None`: it did not exit by its deadline (killed), or it ended by a signal.
    pub(crate) code: Option<i32>,
    pub(crate) elapsed: Duration,
    /// `None`: the pipe did not close by the deadline plus the reap (a descendant holds it).
    pub(crate) stdout_len: Option<usize>,
    pub(crate) stderr_len: Option<usize>,
}

/// Runs one emitter per payload, all launched before any is awaited and settled together,
/// and returns their results in payload order; `None` for an emitter that did not start.
pub(crate) fn emit_all(emitter: &str, agent: &str, payloads: &[Value]) -> Vec<Option<Emitted>> {
    let started: Vec<Option<Watched>> = payloads.iter().map(|p| start(emitter, agent, p)).collect();
    let ok: Vec<bool> = started.iter().map(Option::is_some).collect();
    let mut settled = watch::settle(started.into_iter().flatten().collect()).into_iter();
    ok.into_iter()
        .map(|ok| {
            let s = if ok { settled.next() } else { None }?;
            Some(Emitted {
                code: s.code,
                elapsed: s.elapsed,
                stdout_len: s.outputs[0],
                stderr_len: s.outputs[1],
            })
        })
        .collect()
}

/// Starts `<emitter> hook-emit <agent>` with both outputs counted, writes `payload` (a JSON
/// value; a string is sent as its raw text) to its stdin, and closes it.
fn start(emitter: &str, agent: &str, payload: &Value) -> Option<Watched> {
    let bytes = match payload {
        Value::String(s) => s.clone().into_bytes(),
        v => v.to_string().into_bytes(),
    };
    let launched = Instant::now();
    let mut child = Command::new(emitter)
        .args(["hook-emit", agent])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let (stdout, stderr, stdin) = (child.stdout.take(), child.stderr.take(), child.stdin.take());
    let mut w = Watched::new(child, launched, EMITTER_WAIT);
    w.count(stdout);
    w.count(stderr);
    if let Some(mut stdin) = stdin {
        // The emitter may exit without reading (a missing environment): a broken pipe here
        // is expected.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    Some(w)
}

/// The `emitted` event of one emitter run; `step` is the step index (`hooks`) or the hook's
/// name (`echo`).
pub(crate) fn emitted_event(step: &Value, r: Option<&Emitted>) -> Value {
    match r {
        Some(e) => json!({
            "event": "emitted",
            "step": step,
            "code": e.code,
            "elapsed_ms": u64::try_from(e.elapsed.as_millis()).unwrap_or(u64::MAX),
            "stdout_len": e.stdout_len,
            "stderr_len": e.stderr_len,
        }),
        None => json!({"event": "emitted", "step": step, "code": null, "spawn_failed": true}),
    }
}

/// Reports one emitter run (see [`emitted_event`]).
pub(crate) fn report_emitted(rep: &Reporter, step: &Value, r: Option<&Emitted>) {
    rep.send(&emitted_event(step, r));
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

    /// The report's wire form: a count given up is JSON `null`, a counted one a number.
    #[test]
    fn a_held_pipe_is_reported_as_null() {
        let e = Emitted {
            code: Some(0),
            elapsed: Duration::from_millis(5),
            stdout_len: None,
            stderr_len: Some(0),
        };
        let line = emitted_event(&json!(0), Some(&e)).to_string();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["stdout_len"], Value::Null, "{line}");
        assert_eq!(v["stderr_len"], 0, "{line}");
        assert_eq!(v["code"], 0, "{line}");
        let spawn_failed: Value =
            serde_json::from_str(&emitted_event(&json!(1), None).to_string()).unwrap();
        assert_eq!(
            (
                spawn_failed["code"].clone(),
                spawn_failed["spawn_failed"].clone()
            ),
            (Value::Null, json!(true))
        );
    }
}
