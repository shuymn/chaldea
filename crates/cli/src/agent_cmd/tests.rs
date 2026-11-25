//! The agent commands against a stub server on `127.0.0.1` that answers each connection from
//! a script and records every request (design §6: `agent_cmd::tests`).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::command::{LaunchOutcome, ResumeResult, StopOutcome, StopResult};
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_core::payload::ExitReason;
use chaldea_core::state::{ProcessState, TaskState, TurnState};
use chaldea_core::sync::{LaunchInfo, ThreadSummary};
use chaldea_daemon::lifecycle::ExitCode;

use super::{Client, Found, Until};

const T: ThreadId = ThreadId::from_u128(0x7);
const L: LaunchId = LaunchId::from_u128(0x1);
const OTHER: LaunchId = LaunchId::from_u128(0x2);

/// One request as the stub read it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: serde_json::Value,
}

/// How the stub answers one connection.
enum Reply {
    /// A status and a JSON body.
    Json(u16, String),
    /// Moves the test clock forward by `ms`, then answers.
    Late(u64, u16, String),
    /// Closes the connection without a response.
    Eof,
    /// Answers nothing until the test releases it (or 20 s).
    Hold(mpsc::Receiver<()>),
}

/// The stub server and the test clock (real time plus a skew the stub can add).
struct Stub {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
    skew: Arc<AtomicU64>,
}

impl Stub {
    fn start(script: Vec<Reply>) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let skew = Arc::new(AtomicU64::new(0));
        let (s, k) = (seen.clone(), skew.clone());
        std::thread::spawn(move || {
            for reply in script {
                let Ok((mut c, _)) = listener.accept() else {
                    return;
                };
                if let Some(req) = read_request(&mut c) {
                    s.lock().unwrap().push(req);
                }
                let (status, body) = match reply {
                    Reply::Json(st, b) => (st, b),
                    Reply::Late(ms, st, b) => {
                        k.fetch_add(ms, Ordering::SeqCst);
                        (st, b)
                    }
                    Reply::Eof => continue,
                    Reply::Hold(rx) => {
                        let _ = rx.recv_timeout(Duration::from_secs(20));
                        continue;
                    }
                };
                let _ = write!(
                    c,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Stub { port, seen, skew }
    }

    fn now(&self) -> impl Fn() -> Instant + '_ {
        || Instant::now() + Duration::from_millis(self.skew.load(Ordering::SeqCst))
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    fn posts(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "POST")
            .collect()
    }

    fn gets(&self) -> usize {
        self.seen().iter().filter(|s| s.method == "GET").count()
    }
}

fn read_request(c: &mut std::net::TcpStream) -> Option<Seen> {
    c.set_read_timeout(Some(Duration::from_secs(20))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let n = c.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let mut line = head.lines().next()?.split(' ');
    let (method, path) = (line.next()?.to_owned(), line.next()?.to_owned());
    let len: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < end + 4 + len {
        let n = c.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[end + 4..end + 4 + len]).unwrap_or_default();
    Some(Seen { method, path, body })
}

/// Runs `f` against `stub` with `discover`, returning the exit code, stdout, and stderr.
fn run_with(
    stub: &Stub,
    discover: &dyn Fn() -> Result<Found, String>,
    f: impl FnOnce(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode,
) -> (ExitCode, String, String) {
    let now = stub.now();
    let c = Client {
        discover,
        token: "chdo_test".to_owned(),
        now: &now,
        period: Duration::from_millis(1),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = f(&c, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

fn run(
    stub: &Stub,
    f: impl FnOnce(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode,
) -> (ExitCode, String, String) {
    let port = stub.port;
    run_with(stub, &move || Ok(Found::Port(port)), f)
}

fn summary(
    process: ProcessState,
    turn: TurnState,
    attention: u32,
    launch: Option<(LaunchId, Option<ExitReason>, bool)>,
) -> String {
    serde_json::to_string(&ThreadSummary {
        thread_id: T,
        closed: false,
        process_state: process,
        turn_state: turn,
        task_state: TaskState::None,
        attention_count: attention,
        launch: launch.map(|(launch_id, exit_reason, death_confirmed)| LaunchInfo {
            launch_id,
            pid: None,
            manual: false,
            exit_reason,
            death_confirmed,
        }),
    })
    .unwrap()
}

fn running() -> String {
    summary(
        ProcessState::Running,
        TurnState::Running,
        0,
        Some((L, None, false)),
    )
}

fn error(code: &str) -> String {
    format!(r#"{{"code":"{code}"}}"#)
}

fn ok(body: String) -> Reply {
    Reply::Json(200, body)
}

fn wait(
    until: Until,
    timeout: Duration,
) -> impl FnOnce(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode {
    move |c, o, e| c.wait(T, until, timeout, o, e)
}

const LONG: Duration = Duration::from_secs(20);

/// PR10a1-CL-26: each condition true (at once, level-triggered), false then true, and false
/// with the process exited.
#[test]
fn wait_conditions() {
    let exited = || {
        summary(
            ProcessState::Exited,
            TurnState::Idle,
            0,
            Some((L, Some(ExitReason::Exit), true)),
        )
    };
    let idle = || {
        summary(
            ProcessState::Running,
            TurnState::Idle,
            0,
            Some((L, None, false)),
        )
    };
    let attention = || {
        summary(
            ProcessState::Running,
            TurnState::Running,
            2,
            Some((L, None, false)),
        )
    };
    for (until, holds) in [
        (Until::Exited, exited()),
        (Until::Idle, idle()),
        (Until::Attention, attention()),
    ] {
        // At once.
        let stub = Stub::start(vec![ok(holds.clone())]);
        let (code, out, _) = run(&stub, wait(until, LONG));
        assert_eq!(code, ExitCode::Ok, "{until:?}");
        assert_eq!(out.trim(), holds, "{until:?}");
        assert_eq!(stub.gets(), 1);
        // False, then true.
        let stub = Stub::start(vec![ok(running()), ok(running()), ok(holds.clone())]);
        let (code, out, _) = run(&stub, wait(until, LONG));
        assert_eq!(code, ExitCode::Ok, "{until:?}");
        assert_eq!(out.trim(), holds);
        assert_eq!(stub.gets(), 3);
    }
    // `idle` and `attention` that find the process exited without their condition: exit 1,
    // even with the turn idle.
    for until in [Until::Idle, Until::Attention] {
        let stub = Stub::start(vec![ok(exited())]);
        let (code, out, err) = run(&stub, wait(until, LONG));
        assert_eq!(code, ExitCode::Failed, "{until:?}");
        assert!(out.is_empty());
        assert!(err.contains("the launch exited"), "{err}");
    }
}

/// PR10a1-CL-26: the cutoff. A response read after it is not adopted; a request that ends
/// `TimedOut` at it is exit 4; discovery not serving, 408, 503, and EOF keep polling; 401,
/// 404, 500, and a broken body are exit 1 after one request.
#[test]
fn wait_cutoff() {
    let exited = summary(
        ProcessState::Exited,
        TurnState::Idle,
        0,
        Some((L, Some(ExitReason::Exit), true)),
    );
    // A true summary read after the cutoff (the stub moves the clock past it first).
    let stub = Stub::start(vec![Reply::Late(60_000, 200, exited.clone())]);
    let (code, out, _) = run(&stub, wait(Until::Exited, Duration::from_secs(30)));
    assert_eq!(code, ExitCode::Timeout);
    assert!(out.is_empty(), "the late body is not adopted: {out}");

    // A request that ends `TimedOut` at the cutoff.
    let (release, held) = mpsc::channel();
    let stub = Stub::start(vec![Reply::Hold(held)]);
    let started = Instant::now();
    let (code, out, _) = run(&stub, wait(Until::Exited, Duration::from_millis(300)));
    let _ = release.send(());
    assert_eq!(code, ExitCode::Timeout);
    assert!(out.is_empty());
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the request was cut at the cutoff"
    );

    // Discovery `Starting`/`Stopping`/`Unknown` and `NotRunning`, then 408, 503, and an
    // EOF: each polled again.
    let stub = Stub::start(vec![
        Reply::Json(408, error("request_timeout")),
        Reply::Json(503, error("busy")),
        Reply::Eof,
        ok(exited.clone()),
    ]);
    let calls = AtomicUsize::new(0);
    let port = stub.port;
    let discover = || match calls.fetch_add(1, Ordering::SeqCst) {
        0 => Ok(Found::NotYet),
        1 => Ok(Found::NotRunning),
        _ => Ok(Found::Port(port)),
    };
    let (code, out, _) = run_with(&stub, &discover, wait(Until::Exited, LONG));
    assert_eq!(code, ExitCode::Ok);
    assert_eq!(out.trim(), exited);
    assert_eq!(
        stub.gets(),
        4,
        "the EOF's request is read too, without a reply"
    );
    assert!(calls.load(Ordering::SeqCst) >= 6);

    // Exit 1 after one request.
    for reply in [
        Reply::Json(401, error("unauthorized")),
        Reply::Json(403, error("forbidden")),
        Reply::Json(404, error("not_found")),
        Reply::Json(500, error("internal")),
        ok("{\"thread_id\":".to_owned()),
    ] {
        let stub = Stub::start(vec![reply, ok(exited.clone())]);
        let (code, out, err) = run(&stub, wait(Until::Exited, LONG));
        assert_eq!(code, ExitCode::Failed, "{err}");
        assert!(out.is_empty());
        assert_eq!(stub.gets(), 1, "{err}");
    }
}

fn resume_ok() -> String {
    serde_json::to_string(&ResumeResult {
        launch_id: OTHER,
        input_id: None,
        outcome: LaunchOutcome::Started,
    })
    .unwrap()
}

fn close_ok(stop: Option<StopResult>) -> String {
    serde_json::to_string(&chaldea_core::command::CloseResult { thread_id: T, stop }).unwrap()
}

fn exited_l() -> String {
    summary(
        ProcessState::Exited,
        TurnState::Idle,
        0,
        Some((L, Some(ExitReason::Exit), true)),
    )
}

/// PR10a1-CL-37: one read pins the launch; the POST keeps its `command_id` and
/// `expected_launch_id` across a transport retry; `launch_changed` is exit 1 with no
/// further POST.
#[test]
fn expected_launch_pinned() {
    type Verb = fn(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode;
    let resume: Verb = |c, o, e| c.resume(T, Some("go".to_owned()), o, e);
    let close: Verb = |c, o, e| c.close(T, false, o, e);
    for (name, verb, done) in [
        ("resume", resume, resume_ok()),
        ("close", close, close_ok(None)),
    ] {
        let stub = Stub::start(vec![
            ok(exited_l()),
            Reply::Eof,
            Reply::Json(503, error("outcome_unknown")),
            Reply::Json(409, error("in_progress")),
            ok(done),
        ]);
        let (code, _, err) = run(&stub, verb);
        assert_eq!(code, ExitCode::Ok, "{name}: {err}");
        assert_eq!(stub.gets(), 1, "{name}");
        let posts = stub.posts();
        assert_eq!(posts.len(), 4, "{name}");
        assert!(
            posts
                .iter()
                .all(|p| p.path == format!("/api/commands/{name}"))
        );
        let id = &posts[0].body["command_id"];
        assert!(id.is_string());
        for p in &posts {
            assert_eq!(&p.body["command_id"], id, "{name}: one command_id");
            assert_eq!(
                p.body["args"]["expected_launch_id"],
                serde_json::to_value(L).unwrap(),
                "{name}: the pinned launch"
            );
        }

        // `launch_changed`: exit 1, nothing sent again.
        let stub = Stub::start(vec![
            ok(exited_l()),
            Reply::Json(409, error("launch_changed")),
            ok(resume_ok()),
        ]);
        let (code, _, err) = run(&stub, verb);
        assert_eq!(code, ExitCode::Failed, "{name}");
        assert!(err.contains("launch changed"), "{err}");
        assert_eq!(stub.posts().len(), 1, "{name}");
    }
}

const GENERAL: &str = "The thread's last process could not be confirmed to have ended.";
const RESTART: &str = "The previous process of this thread could not be confirmed to have ended after the daemon restarted.";
const STOPPED: &str =
    "The previous process of this thread could not be confirmed to have ended when it stopped.";
const TAIL: &str = "This version cannot resume or close this thread. Its history is kept, and new threads can be started.";

/// PR10a1-CL-37: `death_unconfirmed` prints the reason's line only when the read after the
/// refusal shows the pinned launch exited unconfirmed; otherwise, and for `stop`, the general
/// line; each exit 1.
#[test]
fn death_unconfirmed_message() {
    type Verb = fn(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode;
    let resume: Verb = |c, o, e| c.resume(T, None, o, e);
    let close: Verb = |c, o, e| c.close(T, true, o, e);
    let unconfirmed = |l, r| {
        summary(
            ProcessState::Exited,
            TurnState::Idle,
            0,
            Some((l, Some(r), false)),
        )
    };
    let refusal = || Reply::Json(409, error("death_unconfirmed"));
    for verb in [resume, close] {
        let cases: Vec<(Reply, &str)> = vec![
            (ok(unconfirmed(L, ExitReason::DaemonRestart)), RESTART),
            (ok(unconfirmed(L, ExitReason::UnknownAfterCrash)), RESTART),
            (ok(unconfirmed(L, ExitReason::Stopped)), STOPPED),
            // Another current launch.
            (ok(unconfirmed(OTHER, ExitReason::DaemonRestart)), GENERAL),
            // The read fails.
            (Reply::Json(500, error("internal")), GENERAL),
            // The read ends at the cutoff.
            (
                Reply::Late(120_000, 200, unconfirmed(L, ExitReason::DaemonRestart)),
                GENERAL,
            ),
        ];
        for (read, line) in cases {
            let stub = Stub::start(vec![ok(exited_l()), refusal(), read]);
            let (code, out, err) = run(&stub, verb);
            assert_eq!(code, ExitCode::Failed);
            assert!(out.is_empty());
            let lines: Vec<&str> = err.lines().collect();
            assert_eq!(
                lines,
                [format!("chaldea: {line}"), format!("chaldea: {TAIL}")],
                "{err}"
            );
            assert_eq!(stub.gets(), 2);
        }
    }
    // `stop` pins no launch: the general line, with no read.
    let stub = Stub::start(vec![refusal()]);
    let (code, _, err) = run(&stub, |c, o, e| c.stop(T, o, e));
    assert_eq!(code, ExitCode::Failed);
    assert_eq!(
        err.lines().collect::<Vec<_>>(),
        [format!("chaldea: {GENERAL}"), format!("chaldea: {TAIL}")]
    );
    assert_eq!(stub.gets(), 0);
}

fn stop_result(confirmed: bool, within: bool) -> StopResult {
    StopResult {
        launch_id: L,
        outcome: StopOutcome::Stopped,
        death_confirmed: confirmed,
        confirmed_within_deadline: within,
        exit_code: None,
        signal: Some(9),
    }
}

/// PR10a1-CL-39: `stop` and `close --stop` exit 0 within the deadline, 4 with one warning line
/// when confirmed late, and 1 with the general line when unconfirmed; the result is printed.
#[test]
fn stop_outcomes() {
    let late = "chaldea: The process ended, but its end was not confirmed within 5 s of the stop request the daemon accepted.";
    for (r, code, lines) in [
        (stop_result(true, true), ExitCode::Ok, vec![]),
        (
            stop_result(true, false),
            ExitCode::Timeout,
            vec![late.to_owned()],
        ),
        (
            stop_result(false, false),
            ExitCode::Failed,
            vec![format!("chaldea: {GENERAL}"), format!("chaldea: {TAIL}")],
        ),
    ] {
        let body = serde_json::to_string(&r).unwrap();
        let stub = Stub::start(vec![ok(body.clone())]);
        let (got, out, err) = run(&stub, |c, o, e| c.stop(T, o, e));
        assert_eq!(got, code, "{r:?}");
        assert_eq!(out.trim(), body);
        assert_eq!(err.lines().collect::<Vec<_>>(), lines, "{r:?}");
        let posts = stub.posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].path, "/api/commands/stop");
        assert_eq!(posts[0].body["args"], serde_json::json!({ "thread_id": T }));
    }
    // `close --stop` whose inner stop was confirmed late.
    let stub = Stub::start(vec![
        ok(running()),
        ok(close_ok(Some(stop_result(true, false)))),
    ]);
    let (code, _, err) = run(&stub, |c, o, e| c.close(T, true, o, e));
    assert_eq!(code, ExitCode::Timeout);
    assert_eq!(err.lines().collect::<Vec<_>>(), [late]);
    assert_eq!(stub.posts()[0].body["args"]["stop"], true);
    // Without a stop: exit 0.
    let stub = Stub::start(vec![ok(exited_l()), ok(close_ok(None))]);
    let (code, _, err) = run(&stub, |c, o, e| c.close(T, false, o, e));
    assert_eq!(code, ExitCode::Ok, "{err}");
}

/// The daemon's prompt checks run first, with their reason, and nothing is sent.
#[test]
fn prompt_checked_first() {
    let stub = Stub::start(vec![]);
    for (agent, prompt, want) in [
        (chaldea_core::ids::Agent::Claude, "hi", "Claude prompt"),
        (chaldea_core::ids::Agent::Codex, "  ", "empty_prompt"),
        (
            chaldea_core::ids::Agent::Codex,
            "a\u{1b}b",
            "control_character at byte 1",
        ),
    ] {
        let args = chaldea_core::command::SpawnArgs {
            agent,
            cwd: "/work".to_owned(),
            prompt: Some(prompt.to_owned()),
            task: None,
        };
        let looked = AtomicUsize::new(0);
        let discover = || {
            looked.fetch_add(1, Ordering::SeqCst);
            Err("no daemon is looked up".to_owned())
        };
        let (code, _, err) = run_with(&stub, &discover, |c, o, e| c.spawn(&args, o, e));
        assert_eq!(code, ExitCode::Usage, "{err}");
        assert!(err.contains(want), "{err}");
        assert_eq!(looked.load(Ordering::SeqCst), 0);
    }
}

/// A timeout the parse accepted that no `Instant` reaches from the wait's own now: a usage
/// error, the daemon never asked.
#[test]
fn wait_timeout_past_the_last_instant() {
    let base = Instant::now();
    // The longest whole-second timeout an `Instant` reaches from `base`.
    let (mut lo, mut hi) = (0u64, u64::MAX);
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if base.checked_add(Duration::from_secs(mid)).is_some() {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    let timeout = Duration::from_secs(lo);
    let later = base + Duration::from_secs(60);
    let now = move || later;
    let asked = AtomicUsize::new(0);
    let discover = || {
        asked.fetch_add(1, Ordering::SeqCst);
        Err("not asked".to_owned())
    };
    let c = Client {
        discover: &discover,
        token: "chdo_test".to_owned(),
        now: &now,
        period: Duration::from_millis(1),
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = c.wait(T, Until::Exited, timeout, &mut out, &mut err);
    let err = String::from_utf8(err).unwrap();
    assert_eq!(code, ExitCode::Usage, "{err}");
    assert!(err.contains("too long"), "{err}");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
}

#[test]
fn durations() {
    assert_eq!(super::duration("10m"), Ok(Duration::from_secs(600)));
    assert_eq!(super::duration("30"), Ok(Duration::from_secs(30)));
    assert_eq!(super::duration("500ms"), Ok(Duration::from_millis(500)));
    assert!(super::duration("0s").is_err());
    assert!(super::duration("5d").is_err());
    assert!(super::duration("m").is_err());
    assert!(super::duration("18446744073709551615s").is_err());
    assert!(super::duration("18446744073709551615h").is_err());
}
