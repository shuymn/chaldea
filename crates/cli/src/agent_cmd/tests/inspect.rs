//! `chaldea inspect` (C1 D2; C1-12) against the stub daemon.

use std::path::Path;

use chaldea_core::ids::{Agent, Epoch, LaunchId, Open, StreamId, WorkspaceId};
use chaldea_core::payload::{ThreadRole, VtCursor, VtModes};
use chaldea_core::state::{AttentionCause, ProcessState, TaskState, TurnState};
use chaldea_core::sync::{
    EventsResponse, GateEvidence, LaunchInfo, ScreenResponse, SessionInfo, SnapshotReason,
    StreamSnapshot, ThreadInfo, ThreadSnapshot,
};
use chaldea_daemon::lifecycle::ExitCode;
use clap::Parser;
use serde_json::Value;

use super::{Found, L, OTHER, Reply, Seen, Stub, T, error, ok, run, run_with, running};

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
                    input_id: chaldea_core::ids::InputId::from_u128(9),
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
        inputs: Vec::new(),
        open_approvals: Vec::new(),
        worktree: None,
        surfaces: Vec::new(),
        vt_snapshot: None,
        recent_events: Vec::new(),
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
    for line in [
        "thread 00000000-0000-0000-0000-000000000007: process running, turn idle, task none\n",
        "attention: session_start_timeout, input_gate_timeout input_id 00000000-0000-0000-0000-000000000009\n",
        "launch: 00000000-0000-0000-0000-000000000001 pid 4242, not exited\n",
        "session: s-1\n",
        "inputs: 0\n",
        "cards: 0\n",
        "recent events: 0\n",
        "screen: 80x3, cursor 0,6\n| 日本語\n| \n| > \\u{1b}x\n",
    ] {
        assert!(out.contains(line), "{line:?} in\n{out}");
    }
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
