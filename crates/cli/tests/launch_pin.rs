//! T2-16 `launch_pin_stale` (`docs/design/t2-terminal.md` §2.7, §6.3): `--launch <id>` on
//! `ready`, `stop`, `resume`, `close`, and `key` pins that launch in place of the current one
//! the CLI would read. Against a foreground daemon of this binary whose agent program is
//! FakeCli `echo` (a Claude launch bound through the real emitter) and whose thread moved to
//! a newer launch, a stale `--launch` exits 1 with the `launch_changed` line, adds no event to
//! the thread's stream, and writes no byte to the PTY; a matching one acts as the command does
//! without it. `stop_bodies` checks the three `stop` request bodies against a stub daemon.

mod support;

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::command::{StopOutcome, StopResult};
use chaldea_core::ids::LaunchId;
use chaldea_daemon::lifecycle::{DiscoveryRecord, Evidence, RecordState, names};
use chaldea_pty_runtime::host;
use serde_json::{Value, json};
use support::faults::{AGENT_PROGRAM, Labels, cmd};
use support::runner::{Case, main as run_cases};
use support::{Dir, Proc, WAIT, marked_dir, request_complete, run_to_exit, state, wait_for};

/// The `launch_changed` refusal's line (`agent_cmd::refused`).
const LAUNCH_CHANGED: &str = "the thread's current launch changed; nothing was sent again";

fn main() {
    run_cases(&[
        Case {
            name: "launch_pin_stale_ready",
            run: ready,
        },
        Case {
            name: "launch_pin_stale_key",
            run: key,
        },
        Case {
            name: "launch_pin_stale_stop",
            run: stop,
        },
        Case {
            name: "launch_pin_stale_resume",
            run: resume,
        },
        Case {
            name: "launch_pin_stale_close",
            run: close,
        },
        Case {
            name: "stop_bodies",
            run: stop_bodies,
        },
    ]);
}

/// FakeCli, built by `cargo test --workspace` (testkit's binary) next to the test binaries'
/// `deps/` directory.
fn fake_cli() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p = dir.join(format!("chaldea-fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "{} is missing: build with cargo test --workspace",
        p.display()
    );
    p
}

/// A thread whose first launch `old` was stopped and resumed as `new`, running and bound, on
/// a daemon whose FakeCli reports to `rx`.
struct Moved {
    labels: Labels,
    d: Dir,
    daemon: Proc,
    port: u16,
    token: String,
    rx: Receiver<Value>,
    t: String,
    old: String,
    new: String,
    /// FakeCli's `SessionStart` gate: a launch's hook waits until the file exists, so the
    /// launch is published before its hook's intake (`supervisor_e2e`'s `SessionGate`).
    _gate: tempfile::TempDir,
    /// The thread's working directory, which a resume starts in.
    _cwd: tempfile::TempDir,
}

impl Moved {
    fn start() -> Moved {
        let labels = Labels::new();
        let d = state();
        let cwd = tempfile::tempdir().unwrap();
        let gate = tempfile::tempdir().unwrap();
        let open = gate.path().join("open");
        let report = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("127.0.0.1:{}", report.local_addr().unwrap().port());
        let argv = json!([
            fake_cli(),
            "--mode",
            "echo",
            "--emitter",
            env!("CARGO_BIN_EXE_chaldea"),
            "--agent",
            "claude",
            "--session-start-gate",
            open,
            "--report",
            addr,
            "--max-life-secs",
            "120",
            "--",
        ])
        .to_string();
        let daemon = Proc::spawn(&mut cmd(
            &labels,
            &d.path,
            &["daemon", "run"],
            &[(AGENT_PROGRAM, OsStr::new(&argv))],
        ));
        let port = daemon.serving();
        let rx = reports(report);
        let token = chaldea_daemon::lifecycle::read_owner_token(&d.path).unwrap();
        let dir = cwd.path().to_string_lossy().into_owned();
        let (code, v, err) = agent(
            &labels,
            &d.path,
            &["spawn", "--agent", "claude", "--cwd", &dir],
        );
        assert_eq!(code, 0, "{err}");
        let t = v["thread_id"].as_str().unwrap().to_owned();
        let old = v["launch_id"].as_str().unwrap().to_owned();
        std::fs::write(&open, b"").unwrap();
        events_of(port, &token, &t, "readiness_observed", 1);
        let (code, _, err) = agent(&labels, &d.path, &["stop", &t]);
        assert_eq!(code, 0, "{err}");
        std::fs::remove_file(&open).unwrap();
        let (code, v, err) = agent(&labels, &d.path, &["resume", &t]);
        assert_eq!(code, 0, "{err}");
        let new = v["launch_id"].as_str().unwrap().to_owned();
        assert_ne!(new, old);
        std::fs::write(&open, b"").unwrap();
        let ready = events_of(port, &token, &t, "readiness_observed", 2);
        assert_eq!(ready[1]["payload"]["launch_id"], new.as_str());
        Moved {
            labels,
            d,
            daemon,
            port,
            token,
            rx,
            t,
            old,
            new,
            _gate: gate,
            _cwd: cwd,
        }
    }

    fn agent(&self, args: &[&str]) -> (i32, Value, String) {
        agent(&self.labels, &self.d.path, args)
    }

    /// The thread's stream and the host stream (where `thread_closed` goes).
    fn events(&self) -> (Vec<Value>, Vec<Value>) {
        (
            envelopes(self.port, &self.token, &format!("thread:{}", self.t)),
            envelopes(self.port, &self.token, "host"),
        )
    }

    /// `args`, which names the stale launch: exit 1 with the `launch_changed` line, and the
    /// thread's and host streams unchanged.
    fn refused(&self, args: &[&str]) {
        assert!(args.contains(&self.old.as_str()), "{args:?}");
        let before = self.events();
        let (code, out, err) = self.agent(args);
        assert_eq!(code, 1, "{args:?}: {out} {err}");
        assert!(err.contains(LAUNCH_CHANGED), "{args:?}: {err}");
        assert_eq!(out, Value::Null, "{args:?}: nothing printed on stdout");
        assert_eq!(self.events(), before, "{args:?}: no event");
    }

    /// No byte reached the running launch's PTY since the last drain: the next stdin chunk
    /// FakeCli reports is the one of a pinned `key` sent now.
    fn no_byte_since_drain(&self) {
        let (code, v, err) = self.agent(&["key", &self.t, "--launch", &self.new, "x"]);
        assert_eq!(code, 0, "{err}");
        assert_eq!(v["outcome"], "written", "{v}");
        let input = next_report(&self.rx, "the marker's input", |v| v["event"] == "input");
        assert_eq!(
            input["hex"], "78",
            "only the marker reached the PTY: {input}"
        );
    }

    fn drain(&self) {
        while self.rx.try_recv().is_ok() {}
    }

    fn end(mut self) {
        assert_eq!(
            self.daemon.stop_and_wait(&self.d.path),
            0,
            "{}",
            self.daemon.stderr()
        );
    }
}

fn ready() {
    let m = Moved::start();
    m.drain();
    m.refused(&["ready", &m.t, "--launch", &m.old]);
    m.no_byte_since_drain();
    let (code, v, err) = m.agent(&["ready", &m.t, "--launch", &m.new]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["launch_id"], m.new.as_str(), "{v}");
    m.end();
}

fn key() {
    let m = Moved::start();
    m.drain();
    m.refused(&["key", &m.t, "--launch", &m.old, "y", "enter"]);
    // The matching pin: the marker is written, alone.
    m.no_byte_since_drain();
    m.end();
}

fn stop() {
    let m = Moved::start();
    m.drain();
    m.refused(&["stop", &m.t, "--launch", &m.old]);
    m.no_byte_since_drain();
    let (code, v, err) = m.agent(&["stop", &m.t, "--launch", &m.new]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["launch_id"], m.new.as_str(), "{v}");
    assert_eq!(v["outcome"], "stopped", "{v}");
    assert_eq!(v["death_confirmed"], true, "{v}");
    m.end();
}

fn resume() {
    let m = Moved::start();
    let (code, _, err) = m.agent(&["stop", &m.t]);
    assert_eq!(code, 0, "{err}");
    m.drain();
    m.refused(&["resume", &m.t, "--launch", &m.old]);
    assert!(
        m.rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "no process was started"
    );
    let (code, v, err) = m.agent(&["resume", &m.t, "--launch", &m.new]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(v["outcome"], "started", "{v}");
    let third = v["launch_id"].as_str().unwrap();
    assert!(third != m.new && third != m.old, "{v}");
    m.end();
}

fn close() {
    let m = Moved::start();
    let (code, _, err) = m.agent(&["stop", &m.t]);
    assert_eq!(code, 0, "{err}");
    m.refused(&["close", &m.t, "--launch", &m.old]);
    let (code, _, err) = m.agent(&["close", &m.t, "--launch", &m.new]);
    assert_eq!(code, 0, "{err}");
    let (status, body) = request_complete(
        m.port,
        "GET",
        &format!("/api/threads/{}", m.t),
        &m.token,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let s: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(s["closed"], true, "{s}");
    m.end();
}

// ---- request bodies ----

/// One request a [`Stub`] read: its method, path, and body.
type Seen = (String, String, Value);

/// A daemon stand-in on `127.0.0.1`: this test holds `dir`'s lock and its record names this
/// process as the serving daemon on the stub's port. It answers `POST /api/commands/stop`
/// with `reply`, every other request 404, and records each one.
struct Stub {
    seen: Arc<Mutex<Vec<Seen>>>,
    _lock: std::fs::File,
}

impl Stub {
    fn start(dir: &Path, reply: &StopResult) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (s, reply) = (seen.clone(), serde_json::to_vec(reply).unwrap());
        std::thread::spawn(move || {
            for c in listener.incoming() {
                let Ok(mut c) = c else { return };
                let Some((method, path, body)) = read_request(&mut c) else {
                    continue;
                };
                let (status, out) = if method == "POST" && path == "/api/commands/stop" {
                    ("200 OK", reply.clone())
                } else {
                    ("404 Not Found", br#"{"code":"not_found"}"#.to_vec())
                };
                s.lock().unwrap().push((method, path, body));
                let _ = write!(
                    c,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    out.len()
                );
                let _ = c.write_all(&out);
            }
        });
        marked_dir(dir);
        support::write_private(
            &dir.join(names::OWNER_TOKEN),
            format!("{}\n", support::unknown_token()).as_bytes(),
        );
        let lock = std::fs::File::create(dir.join(names::LOCK)).unwrap();
        lock.try_lock().unwrap();
        let record = DiscoveryRecord {
            version: 1,
            instance: uuid::Uuid::new_v4().to_string(),
            pid: std::process::id(),
            start_time: host::current_identity().unwrap().start_time,
            state: RecordState::Serving,
            port: Some(port),
            url: None,
            started_at: String::new(),
            detached: None,
            evidence: Evidence::default(),
        };
        support::write_private(
            &dir.join(names::RECORD),
            &serde_json::to_vec(&record).unwrap(),
        );
        Stub { seen, _lock: lock }
    }

    /// The `args` of every `stop` command posted since the last call.
    fn stops(&self) -> Vec<Value> {
        let mut seen = self.seen.lock().unwrap();
        let out = seen
            .iter()
            .filter(|(m, _, _)| m == "POST")
            .map(|(_, p, b)| {
                assert_eq!(p, "/api/commands/stop", "{b}");
                b["args"].clone()
            })
            .collect();
        seen.clear();
        out
    }
}

/// The request line and the JSON body (`Null` when empty) of one request.
fn read_request(c: &mut std::net::TcpStream) -> Option<(String, String, Value)> {
    c.set_read_timeout(Some(WAIT)).ok()?;
    let mut r = BufReader::new(c.try_clone().ok()?);
    let mut line = String::new();
    r.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next()?.to_owned(), parts.next()?.to_owned());
    let mut len = 0;
    loop {
        let mut h = String::new();
        r.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body).ok()?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).ok()?
    };
    Some((method, path, body))
}

/// §6.6 item 5: `stop --launch X` alone sends a plain stop pinned to `X` (no `assume_dead`);
/// `--assume-dead --launch X` sends the owner's confirmation for `X`; `--assume-dead` alone
/// is the preview, which sends no command; a plain stop sends neither field.
fn stop_bodies() {
    let labels = Labels::new();
    let d = state();
    let t = "00000000-0000-4000-8000-000000000007";
    let x = LaunchId::from_u128(0x1);
    let stub = Stub::start(
        &d.path,
        &StopResult {
            launch_id: x,
            outcome: StopOutcome::Stopped,
            death_confirmed: true,
            confirmed_within_deadline: true,
            exit_code: Some(0),
            signal: None,
        },
    );
    let x_s = x.to_string();
    let run = |args: &[&str]| {
        let (code, _, err) = run_to_exit(&mut cmd(&labels, &d.path, args, &[]));
        (code, err)
    };

    let (code, err) = run(&["stop", t, "--launch", &x_s]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        stub.stops(),
        [json!({ "thread_id": t, "expected_launch_id": x })],
        "a pinned plain stop"
    );

    let (_, err) = run(&["stop", t, "--assume-dead", "--launch", &x_s]);
    assert_eq!(
        stub.stops(),
        [json!({ "thread_id": t, "expected_launch_id": x, "assume_dead": true })],
        "the confirmation: {err}"
    );

    let (code, err) = run(&["stop", t, "--assume-dead"]);
    assert_eq!(code, 1, "the preview's read is refused by the stub: {err}");
    assert!(
        stub.stops().is_empty(),
        "the preview sends no command: {err}"
    );

    let (code, err) = run(&["stop", t]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(stub.stops(), [json!({ "thread_id": t })], "a plain stop");
}

// ---- helpers ----

/// `chaldea --state-dir <dir> <args>`: its exit code, stdout as JSON (or `Null`), and stderr.
fn agent(labels: &Labels, dir: &Path, args: &[&str]) -> (i32, Value, String) {
    let (code, out, err) = run_to_exit(&mut cmd(labels, dir, args, &[]));
    (
        code,
        serde_json::from_str(out.trim()).unwrap_or(Value::Null),
        err,
    )
}

/// Every JSON line FakeCli processes send to `report`, from every connection, on a channel.
fn reports(report: TcpListener) -> Receiver<Value> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for conn in report.incoming() {
            let Ok(conn) = conn else { return };
            let tx = tx.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(conn).lines() {
                    let Ok(line) = line else { return };
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        let _ = tx.send(v);
                    }
                }
            });
        }
    });
    rx
}

/// The next report matching `pred` within [`WAIT`].
fn next_report(rx: &Receiver<Value>, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
    let end = Instant::now() + WAIT;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(v) if pred(&v) => return v,
            Ok(_) => {}
            Err(_) => panic!("no report {what} within {WAIT:?}"),
        }
    }
}

/// The events of `stream` (its text form).
fn envelopes(port: u16, token: &str, stream: &str) -> Vec<Value> {
    let (status, body) = request_complete(
        port,
        "GET",
        &format!("/api/events?stream_id={stream}&limit=1000"),
        token,
        None,
    );
    assert_eq!(status, 200, "{body}");
    let v: Value = serde_json::from_str(&body).unwrap();
    v["events"].as_array().unwrap().clone()
}

/// Waits until thread `t` has `n` events of `kind`; returns them.
fn events_of(port: u16, token: &str, t: &str, kind: &str, n: usize) -> Vec<Value> {
    let stream = format!("thread:{t}");
    let of = || -> Vec<Value> {
        envelopes(port, token, &stream)
            .into_iter()
            .filter(|e| e["kind"] == kind)
            .collect()
    };
    assert!(
        wait_for(|| of().len() >= n),
        "{n} {kind}: {:?}",
        envelopes(port, token, &stream)
    );
    of()
}
