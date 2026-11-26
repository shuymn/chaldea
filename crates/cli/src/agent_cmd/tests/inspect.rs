//! `chaldea inspect` (C1 D2; C1-12) against the stub daemon.

use std::path::Path;

use std::time::{Duration, Instant};

use chaldea_core::ids::{
    Actor, Agent, ApprovalId, Epoch, InputId, LaunchId, Open, StreamId, WorkspaceId,
};
use chaldea_core::input::InputState;
use chaldea_core::payload::{
    ApprovalRequested, Content, InputEncoding, ThreadRole, VtCursor, VtModes,
};
use chaldea_core::state::{AttentionCause, ProcessState, TaskState, TurnState};
use chaldea_core::sync::{
    EventsResponse, GateEvidence, InputInfo, LaunchInfo, ScreenResponse, SessionInfo,
    SnapshotReason, StreamSnapshot, ThreadInfo, ThreadSnapshot,
};
use chaldea_daemon::lifecycle::ExitCode;
use clap::Parser;
use serde_json::{Value, json};

use super::{
    Client, Found, L, OTHER, RUN, Reply, Seen, Stub, T, error, ok, run, run_with, running,
};

const SNAPSHOT_PATH: &str =
    "/api/events?stream_id=thread:00000000-0000-0000-0000-000000000007&after_seq=9007199254740991";
const SUMMARY_PATH: &str = "/api/threads/00000000-0000-0000-0000-000000000007";
const SCREEN_PATH: &str = "/api/threads/00000000-0000-0000-0000-000000000007/screen";

fn snapshot() -> String {
    snapshot_with_cwd("/work/repo")
}

fn snapshot_with_cwd(cwd: &str) -> String {
    let s = ThreadSnapshot {
        thread: ThreadInfo {
            thread_id: T,
            agent: Open::Known(Agent::Claude),
            cwd: cwd.into(),
            workspace_id: WorkspaceId("ws".into()),
            parent_thread_id: None,
            role: ThreadRole::Standard,
            closed: false,
            process_state: ProcessState::Running,
            turn_state: TurnState::Idle,
            task_state: TaskState::None,
            attention: vec![
                AttentionCause::SessionStartTimeout,
                AttentionCause::InputGateTimeout {
                    input_id: InputId::from_u128(9),
                },
            ],
        },
        launch: Some(LaunchInfo {
            launch_id: L,
            pid: Some(4242),
            manual: false,
            exit_reason: None,
            death_confirmed: false,
        }),
        session: Some(SessionInfo {
            session_id: "s-1".into(),
            transcript_path: None,
        }),
        gate: GateEvidence {
            readiness_seq: None,
            readiness_evidence: None,
            last_write_seq: None,
            last_human_write_seq: None,
            last_turn_start_seq: None,
        },
        inputs: vec![InputInfo {
            input_id: InputId::from_u128(9),
            launch_id: Some(L),
            state: InputState::Queued,
            text: Content::text("hi"),
            encoding: InputEncoding::BracketedPaste,
            created_by: Actor::Human,
            sent_after_seq: None,
        }],
        open_approvals: vec![ApprovalRequested {
            approval_id: ApprovalId::from_u128(0xA),
            launch_id: L,
            turn_ref: None,
            tool_name: "Bash".into(),
            tool_input: Content::text("{}"),
            correlation: None,
        }],
        worktree: None,
        surfaces: Vec::new(),
        vt_snapshot: None,
        recent_events: vec![serde_json::from_value(json!({
            "stream_id": format!("thread:{T}"),
            "seq": 3,
            "event_id": "00000000-0000-0000-0000-00000000000e",
            "schema_version": 1,
            "idempotency_key": "x",
            "kind": "launch_started",
            "thread_id": T,
            "launch_id": L,
            "command_id": null,
            "source": { "kind": "daemon", "agent": null, "cli_version": null, "transcript": null },
            "occurred_at": "2026-11-22T09:45:00.123Z",
            "recorded_at": "2026-11-22T09:45:00.123Z",
            "payload": {},
        }))
        .unwrap()],
    };
    serde_json::to_string(&EventsResponse::Snapshot {
        stream_id: StreamId::Thread(T),
        epoch: Epoch("e".into()),
        snapshot: StreamSnapshot::Thread(Box::new(s)),
        as_of_seq: 3,
        reason: SnapshotReason::CursorAhead,
    })
    .unwrap()
}

fn screen(launch: LaunchId) -> String {
    serde_json::to_string(&ScreenResponse::new(
        T,
        launch,
        80,
        3,
        VtCursor {
            row: 0,
            col: 6,
            visible: true,
        },
        VtModes {
            bracketed_paste: true,
            alternate_screen: false,
        },
        vec!["日本語".into(), String::new(), "> \u{1b}x".into()],
    ))
    .unwrap()
}

/// A stub answering each path with `snapshot`, the summary, and `screen`.
fn stub(
    snapshot: impl Fn() -> Reply + Send + 'static,
    screen: impl Fn() -> Reply + Send + 'static,
) -> Stub {
    Stub::serve(20, move |s: &Seen| match s.path.as_str() {
        SNAPSHOT_PATH => snapshot(),
        SUMMARY_PATH => ok(running()),
        SCREEN_PATH => screen(),
        p => Reply::Json(500, format!("unexpected {p}")),
    })
}

fn paths(stub: &Stub) -> Vec<String> {
    stub.seen().into_iter().map(|s| s.path).collect()
}

fn inspect(json: bool) -> impl FnOnce(&super::Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode {
    move |c, o, e| c.inspect(T, json, o, e)
}

fn schema() -> jsonschema::Validator {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/contracts/schema/InspectOutput.schema.json");
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

/// The `--json` output, validated against the generated schema.
fn checked(out: &str) -> Value {
    let v: Value = serde_json::from_str(out.trim_end()).unwrap();
    let errors: Vec<String> = schema().iter_errors(&v).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{errors:?}\n{v}");
    v
}

/// C1-12: one snapshot read (no summary read), then the screen; the plain output has the
/// state, attention, launch, session, inputs, cards, events, and the screen rows, with
/// control characters escaped.
#[test]
fn normal() {
    let s = stub(|| ok(snapshot()), || ok(screen(L)));
    let (code, out, err) = run(&s, inspect(false));
    assert_eq!(code, ExitCode::Ok, "{err}");
    assert_eq!(paths(&s), [SNAPSHOT_PATH, SCREEN_PATH]);
    assert_eq!(
        out,
        "thread 00000000-0000-0000-0000-000000000007: process running, turn idle, task none
attention: session_start_timeout, input_gate_timeout input_id 00000000-0000-0000-0000-000000000009
launch: 00000000-0000-0000-0000-000000000001 pid 4242, not exited
session: s-1
inputs: 1
  00000000-0000-0000-0000-000000000009 queued by human
cards: 1
  00000000-0000-0000-0000-00000000000a Bash
recent events: 1
  3 launch_started
screen: 80x3, cursor 0,6
| 日本語
| 
| > \\u{1b}x
"
    );
    let (code, out, _) = run(&stub(|| ok(snapshot()), || ok(screen(L))), inspect(true));
    assert_eq!(code, ExitCode::Ok);
    let v = checked(&out);
    assert_eq!(v["snapshot"]["thread"]["thread_id"], T.to_string());
    assert_eq!(v["summary"], Value::Null);
    assert_eq!(v["screen"]["lines"][0], "日本語");
    assert_eq!(
        (&v["snapshot_error"], &v["screen_error"]),
        (&Value::Null, &Value::Null)
    );
}

/// A snapshot body above the client's default 1 MiB and within the 16 MiB cap is read.
#[test]
fn large_snapshot() {
    let big = format!("/work/{}", "x".repeat(2 * 1024 * 1024));
    let body = snapshot_with_cwd(&big);
    let s = stub(move || ok(body.clone()), || ok(screen(L)));
    let (code, out, err) = run(&s, inspect(true));
    assert_eq!(code, ExitCode::Ok, "{err}");
    assert_eq!(paths(&s), [SNAPSHOT_PATH, SCREEN_PATH]);
    assert_eq!(checked(&out)["snapshot"]["thread"]["cwd"], big);
}

/// A fallback case: its name, the snapshot reply, and the error it shows.
type Fallback = (&'static str, fn() -> Reply, &'static str);

/// C1-12: a snapshot body over the 16 MiB cap, a daemon 408, and a client-side timeout each
/// fall back to the summary, still read the screen, and exit 0; the output says what is
/// missing.
#[test]
fn fallbacks() {
    let cases: [Fallback; 3] = [
        (
            "over the cap",
            || Reply::Head(200, 16 * 1024 * 1024 + 1),
            "payload_too_large",
        ),
        (
            "daemon 408",
            || Reply::Json(408, error("request_timeout")),
            "request_timeout",
        ),
        (
            "client timeout",
            || Reply::Late(20_000, 200, snapshot()),
            "request_timeout",
        ),
    ];
    for (name, snap, code_name) in cases {
        let s = stub(snap, || ok(screen(L)));
        let (code, out, err) = run(&s, inspect(true));
        assert_eq!(code, ExitCode::Ok, "{name}: {err}");
        assert_eq!(
            paths(&s),
            [SNAPSHOT_PATH, SUMMARY_PATH, SCREEN_PATH],
            "{name}"
        );
        let v = checked(&out);
        assert_eq!(v["snapshot"], Value::Null, "{name}");
        assert_eq!(v["summary"]["thread_id"], T.to_string(), "{name}");
        assert_eq!(v["snapshot_error"]["code"], code_name, "{name}");
        assert_eq!(v["screen"]["launch_id"], L.to_string(), "{name}");

        let (code, out, _) = run(&stub(snap, || ok(screen(L))), inspect(false));
        assert_eq!(code, ExitCode::Ok, "{name}");
        assert!(out.contains("attention: 0 cause(s)\n"), "{name}: {out}");
        assert!(
            out.contains(&format!("The snapshot could not be read ({code_name})")),
            "{name}: {out}"
        );
        assert!(out.contains("| 日本語\n"), "{name}: {out}");
    }
}

/// The screen of another launch than the one read is `launch_changed`, with no screen; a
/// screen refusal is the daemon's code; both exit 0 with a line for the missing screen.
#[test]
fn screen_errors() {
    let s = stub(|| ok(snapshot()), || ok(screen(OTHER)));
    let (code, out, _) = run(&s, inspect(true));
    assert_eq!(code, ExitCode::Ok);
    let v = checked(&out);
    assert_eq!(v["screen"], Value::Null);
    assert_eq!(v["screen_error"]["code"], "launch_changed");

    let s = stub(
        || ok(snapshot()),
        || Reply::Json(409, error("no_live_launch")),
    );
    let (code, out, _) = run(&s, inspect(false));
    assert_eq!(code, ExitCode::Ok);
    assert!(out.contains("screen: not read (no_live_launch)\n"), "{out}");

    // The screen read keeps the client's 1 MiB limit: a body just over it is refused.
    let s = stub(|| ok(snapshot()), || Reply::Head(200, 1024 * 1024 + 1));
    let (code, out, _) = run(&s, inspect(true));
    assert_eq!(code, ExitCode::Ok);
    assert_eq!(checked(&out)["screen_error"]["code"], "payload_too_large");
}

/// An unreadable thread and an unreachable daemon exit 1; a summary read that runs out of
/// time exits 4.
#[test]
fn failures() {
    let s = stub(|| Reply::Json(404, error("not_found")), || ok(screen(L)));
    let (code, out, err) = run(&s, inspect(true));
    assert_eq!(code, ExitCode::Failed);
    assert_eq!(paths(&s), [SNAPSHOT_PATH]);
    assert!(err.contains("no such thread or not readable"), "{err}");
    assert_eq!(checked(&out)["snapshot_error"]["code"], "not_found");

    let s = stub(|| ok(snapshot()), || ok(screen(L)));
    let (code, _, err) = run_with(&s, &|| Ok(Found::NotRunning), inspect(false));
    assert_eq!(code, ExitCode::Failed);
    assert!(err.contains("daemon not running"), "{err}");

    let s = Stub::serve(20, |s: &Seen| match s.path.as_str() {
        SNAPSHOT_PATH => Reply::Json(408, error("request_timeout")),
        SUMMARY_PATH => Reply::Late(20_000, 200, running()),
        _ => ok(screen(L)),
    });
    let (code, _, _) = run(&s, inspect(false));
    assert_eq!(code, ExitCode::Timeout);
}

/// Bad arguments are a usage error (exit 2).
#[test]
fn bad_args() {
    for args in [
        &["chaldea", "inspect"][..],
        &["chaldea", "inspect", "nope"],
        &[
            "chaldea",
            "inspect",
            "00000000-0000-0000-0000-000000000007",
            "--bogus",
        ],
    ] {
        let e = crate::args::Cli::try_parse_from(args).unwrap_err();
        assert_eq!(e.exit_code(), 2, "{args:?}");
    }
    let ok = crate::args::Cli::try_parse_from([
        "chaldea",
        "inspect",
        "00000000-0000-0000-0000-000000000007",
        "--json",
    ]);
    assert!(ok.is_ok());
}

/// The generated schema requires every key `--json` prints, `null` ones included (as the
/// TypeScript type does): an output without one, or a summary without `launch`, is refused.
#[test]
fn schema_requires_every_key() {
    let s = stub(
        || Reply::Json(408, error("request_timeout")),
        || ok(screen(L)),
    );
    let (_, out, _) = run(&s, inspect(true));
    let v = checked(&out);
    for key in [
        "thread_id",
        "snapshot",
        "summary",
        "screen",
        "snapshot_error",
        "screen_error",
    ] {
        let mut w = v.clone();
        w.as_object_mut().unwrap().remove(key);
        assert!(!schema().is_valid(&w), "{key} may be left out");
    }
    let mut w = v.clone();
    w["summary"].as_object_mut().unwrap().remove("launch");
    assert!(!schema().is_valid(&w), "summary.launch may be left out");
}

/// A read whose budget runs out after concrete failures reports the last one: the daemon's
/// `busy` body (the snapshot then falls back to the summary), or `internal` for a peer that
/// closes before answering (the snapshot then fails, exit 1).
#[test]
fn last_failure_at_the_cutoff() {
    // The snapshot: busy until the budget is spent, then a summary.
    let busy = || Reply::Late(2_000, 503, r#"{"code":"busy","retry_after":1}"#.to_owned());
    let s = stub(busy, busy);
    let (code, out, err) = run(&s, |c, o, e| {
        c.inspect_within(T, true, Duration::from_secs(5), o, e)
    });
    assert_eq!(code, ExitCode::Ok, "{err}");
    let v = checked(&out);
    assert_eq!(
        v["snapshot_error"],
        json!({ "code": "busy", "retry_after": 1 })
    );
    assert_eq!(v["summary"]["thread_id"], T.to_string());
    assert_eq!(
        v["screen_error"],
        json!({ "code": "busy", "retry_after": 1 })
    );

    // Closed before answering, until the budget is spent.
    let s = Stub::serve(200, |_| Reply::Eof);
    let (code, out, err) = run(&s, |c, o, e| {
        c.inspect_within(T, true, Duration::from_millis(300), o, e)
    });
    assert_eq!(code, ExitCode::Failed);
    assert!(err.contains("the request failed"), "{err}");
    assert_eq!(checked(&out)["snapshot_error"]["code"], "internal");

    // The screen alone closed before answering: `internal`.
    let s = stub(|| ok(snapshot()), || Reply::Eof);
    let (code, out, _) = run(&s, |c, o, e| {
        c.inspect_within(T, true, Duration::from_millis(300), o, e)
    });
    assert_eq!(code, ExitCode::Ok);
    assert_eq!(checked(&out)["screen_error"]["code"], "internal");
}

/// Runs `f` on a client whose polling clock is 10 s behind the socket's: a socket timeout at
/// the cutoff arrives while polling still has time left.
fn run_lagging(
    stub: &Stub,
    f: impl FnOnce(&Client<'_>, &mut Vec<u8>, &mut Vec<u8>) -> ExitCode,
) -> (ExitCode, String, String) {
    const LAG: Duration = Duration::from_secs(10);
    let port = stub.port;
    let discover = move || Ok(Found::Port(port, RUN.to_owned()));
    let now = || Instant::now().checked_sub(LAG).unwrap_or_else(Instant::now);
    let c = Client {
        discover: &discover,
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

/// An I/O timeout of the snapshot request falls back to the summary (exit 0); one of the
/// summary request exits 4. The socket times out while the polling clock is before its
/// cutoff, so only the request's own timeout ends each read.
#[test]
fn socket_timeouts() {
    // Each read's cutoff on the lagging clock is 12 s ahead: 2 s of real time. The stub
    // answers one connection at a time, so the stalled snapshot (3 s) must end within the
    // summary's own 2 s.
    let within = Duration::from_secs(12);
    let s = Stub::serve(20, |s: &Seen| match s.path.as_str() {
        SNAPSHOT_PATH => Reply::Stall(3_000),
        SUMMARY_PATH => ok(running()),
        _ => ok(screen(L)),
    });
    let (code, out, err) = run_lagging(&s, |c, o, e| c.inspect_within(T, true, within, o, e));
    assert_eq!(code, ExitCode::Ok, "{err}");
    let v = checked(&out);
    assert_eq!(v["snapshot_error"]["code"], "request_timeout");
    assert_eq!(v["summary"]["thread_id"], T.to_string());

    let s = Stub::serve(20, |s: &Seen| match s.path.as_str() {
        SNAPSHOT_PATH => Reply::Json(408, error("request_timeout")),
        SUMMARY_PATH => Reply::Stall(3_000),
        _ => ok(screen(L)),
    });
    let (code, _, err) = run_lagging(&s, |c, o, e| c.inspect_within(T, false, within, o, e));
    assert_eq!(code, ExitCode::Timeout, "{err}");
}

/// A replay answer to the snapshot read is `internal` (exit 1), and the summary is not read.
#[test]
fn replay_is_refused() {
    let page = serde_json::to_string(&EventsResponse::Replay {
        stream_id: StreamId::Thread(T),
        epoch: Epoch("e".into()),
        events: Vec::new(),
        head_seq: 0,
        has_more: false,
        has_more_before: false,
    })
    .unwrap();
    let s = stub(move || ok(page.clone()), || ok(screen(L)));
    let (code, out, err) = run(&s, inspect(true));
    assert_eq!(code, ExitCode::Failed);
    assert!(err.contains("with a page"), "{err}");
    assert_eq!(paths(&s), [SNAPSHOT_PATH]);
    assert_eq!(checked(&out)["snapshot_error"]["code"], "internal");
}

/// A request that times out at the cutoff after an earlier attempt failed in transport keeps
/// that failure: `internal`, exit 1, no summary read.
#[test]
fn timeout_keeps_the_earlier_failure() {
    let s = Stub::start(vec![Reply::Eof, Reply::Stall(3_000)]);
    let (code, out, err) = run_lagging(&s, |c, o, e| {
        c.inspect_within(T, true, Duration::from_secs(12), o, e)
    });
    assert_eq!(code, ExitCode::Failed, "{err}");
    assert!(err.contains("the request failed"), "{err}");
    assert_eq!(checked(&out)["snapshot_error"]["code"], "internal");
    assert_eq!(paths(&s), [SNAPSHOT_PATH, SNAPSHOT_PATH]);
}
