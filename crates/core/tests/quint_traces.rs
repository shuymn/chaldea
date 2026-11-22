//! Replays the committed Quint traces (`specs/traces/*.itf.json`, exported from
//! `specs/thread_input_traces.qnt`) against the real `reduce`, `step`, `blocks_gate`, and
//! `gate` (`docs/design/formal-specs.md` "Trace bridge").
//!
//! Each model step records the reducer calls it made (`last.calls`) with the result the
//! model's mirror of the reducers gave: acceptance and, for `reduce`, the `StatusChanged`
//! output. After every step this test checks that the real functions agree on each call,
//! that the thread state and every input state equal the model's, and that `gate` on the
//! step's projection gives the model's decision. A reducer that drifts from the spec fails
//! here; `npm run traces:check` in `specs/` keeps the traces equal to the spec.

#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "this test reads the committed trace files; core itself stays Sans-IO"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chaldea_core::ids::{Actor, InputId, LaunchId};
use chaldea_core::input::{
    GateClosedReason, GateDecision, GateView, InputInput, InputState, ReadinessMark, RejectReason,
    blocks_gate, gate, step,
};
use chaldea_core::payload::{ExitReason, ReadinessEvidence};
use chaldea_core::state::{
    AttentionCause, ProcessState, StateInput, StatusCause, StatusChanged, TaskState, ThreadState,
    TurnState, reduce,
};
use serde_json::Value;

fn traces_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs/traces")
}

// ITF encoding (https://apalache-mc.org/docs/adr/015adr-trace.html): integers as
// `{"#bigint": "<n>"}`, sets as `{"#set": [..]}`, maps as `{"#map": [[k, v], ..]}`, sum
// type values as `{"tag": .., "value": ..}`, records as objects, lists as arrays.

fn int(v: &Value) -> u64 {
    let s = v["#bigint"]
        .as_str()
        .unwrap_or_else(|| panic!("not an ITF integer: {v}"));
    s.parse().unwrap_or_else(|e| panic!("bad integer {s}: {e}"))
}

fn boolean(v: &Value) -> bool {
    v.as_bool().unwrap_or_else(|| panic!("not a boolean: {v}"))
}

fn set(v: &Value) -> &[Value] {
    v["#set"]
        .as_array()
        .unwrap_or_else(|| panic!("not an ITF set: {v}"))
}

fn map(v: &Value) -> Vec<(&Value, &Value)> {
    v["#map"]
        .as_array()
        .unwrap_or_else(|| panic!("not an ITF map: {v}"))
        .iter()
        .map(|kv| (&kv[0], &kv[1]))
        .collect()
}

fn list(v: &Value) -> &[Value] {
    v.as_array().unwrap_or_else(|| panic!("not a list: {v}"))
}

fn tag(v: &Value) -> (&str, &Value) {
    let t = v["tag"]
        .as_str()
        .unwrap_or_else(|| panic!("not a variant: {v}"));
    (t, &v["value"])
}

/// A stream position; the model writes 0 for none.
fn position(v: &Value) -> Option<u64> {
    Some(int(v)).filter(|&p| p != 0)
}

fn input_id(v: &Value) -> InputId {
    InputId::from_u128(u128::from(int(v)))
}

fn launch_id(n: u64) -> LaunchId {
    LaunchId::from_u128(u128::from(n))
}

fn process(v: &Value) -> ProcessState {
    match tag(v).0 {
        "Starting" => ProcessState::Starting,
        "Running" => ProcessState::Running,
        "Stopping" => ProcessState::Stopping,
        "Exited" => ProcessState::Exited,
        t => panic!("unknown process state {t}"),
    }
}

fn turn(v: &Value) -> TurnState {
    match tag(v).0 {
        "TurnUnknown" => TurnState::Unknown,
        "Idle" => TurnState::Idle,
        "TurnRunning" => TurnState::Running,
        "AwaitingApproval" => TurnState::AwaitingApproval,
        t => panic!("unknown turn state {t}"),
    }
}

fn task(v: &Value) -> TaskState {
    match tag(v).0 {
        "TaskNone" => TaskState::None,
        "TaskActive" => TaskState::Active,
        "TaskCompleted" => TaskState::Completed,
        t => panic!("unknown task state {t}"),
    }
}

fn cause(v: &Value) -> AttentionCause {
    match tag(v) {
        ("SessionStartTimeout", _) => AttentionCause::SessionStartTimeout,
        ("StartupStall", _) => AttentionCause::StartupStall,
        ("InputUnconfirmed", id) => AttentionCause::InputUnconfirmed {
            input_id: input_id(id),
        },
        ("InputGateTimeout", id) => AttentionCause::InputGateTimeout {
            input_id: input_id(id),
        },
        ("ResumeBlocked", _) => AttentionCause::ResumeBlocked,
        (t, _) => panic!("unknown attention cause {t}"),
    }
}

/// An attention set as `StatusChanged` carries it: sorted.
fn causes(v: &Value) -> Vec<AttentionCause> {
    set(v)
        .iter()
        .map(cause)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn evidence(v: &Value) -> ReadinessEvidence {
    match tag(v).0 {
        "SessionStart" => ReadinessEvidence::SessionStart,
        "StopSettled" => ReadinessEvidence::StopSettled,
        "PostDecision" => ReadinessEvidence::PostDecision,
        "OwnerAsserted" => ReadinessEvidence::OwnerAsserted,
        t => panic!("unknown evidence {t}"),
    }
}

fn exit_reason(v: &Value) -> ExitReason {
    match tag(v).0 {
        "ExitNormal" => ExitReason::Exit,
        "ExitStopped" => ExitReason::Stopped,
        "ExitSpawnFailed" => ExitReason::SpawnFailed,
        "ExitDaemonRestart" => ExitReason::DaemonRestart,
        "ExitUnknownAfterCrash" => ExitReason::UnknownAfterCrash,
        t => panic!("unknown exit reason {t}"),
    }
}

fn state_input(v: &Value) -> StateInput {
    match tag(v) {
        ("Reserved", relaunch) => StateInput::Reserved {
            relaunch: boolean(relaunch),
        },
        ("Spawned", _) => StateInput::Spawned,
        ("SessionBound", _) => StateInput::SessionBound,
        ("StopRequested", _) => StateInput::StopRequested,
        ("ExitedBy", r) => StateInput::Exited {
            reason: exit_reason(r),
        },
        ("DeathConfirmed", _) => StateInput::DeathConfirmed,
        ("ReadinessObserved", _) => StateInput::ReadinessObserved,
        ("PromptSubmitted", _) => StateInput::PromptSubmitted,
        ("Interrupted", _) => StateInput::Interrupted,
        ("TurnStopped", _) => StateInput::TurnStopped,
        ("ApprovalOpened", _) => StateInput::ApprovalOpened,
        ("ApprovalsAllResolved", _) => StateInput::ApprovalsAllResolved,
        ("AttentionRaised", c) => StateInput::AttentionRaised(cause(c)),
        ("AttentionCleared", c) => StateInput::AttentionCleared(cause(c)),
        // The model does not track who set the task; the reducer ignores `by`.
        ("TaskSet", t) => StateInput::TaskSet {
            to: task(t),
            by: Actor::Human,
        },
        (t, _) => panic!("unknown state input {t}"),
    }
}

fn input_state(v: &Value) -> InputState {
    match tag(v).0 {
        "Queued" => InputState::Queued,
        "Sending" => InputState::Sending,
        "Sent" => InputState::Sent,
        "Acked" => InputState::Acked,
        "Unconfirmed" => InputState::Unconfirmed,
        "Rejected" => InputState::Rejected,
        "Cancelled" => InputState::Cancelled,
        "Dismissed" => InputState::Dismissed,
        t => panic!("unknown input state {t}"),
    }
}

fn input_input(v: &Value) -> InputInput {
    match tag(v) {
        ("GateOpened", seq) => InputInput::GateOpened {
            sent_after_seq: int(seq),
        },
        ("WriteCompleted", _) => InputInput::WriteCompleted,
        ("WriteOutcomeUnknown", _) => InputInput::WriteOutcomeUnknown,
        ("Reject", r) => InputInput::Rejected(match tag(r).0 {
            "ControlCharacter" => RejectReason::ControlCharacter,
            "SpawnFailed" => RejectReason::SpawnFailed,
            t => panic!("unknown reject reason {t}"),
        }),
        ("Cancel", _) => InputInput::Cancelled,
        ("RecheckFailed", _) => InputInput::RecheckFailed,
        ("LaunchExited", _) => InputInput::LaunchExited,
        ("DaemonRestarted", _) => InputInput::DaemonRestarted,
        ("AckAttributed", _) => InputInput::AckAttributed,
        ("AckNotAttributable", _) => InputInput::AckNotAttributable,
        ("AckWindowExpired", _) => InputInput::AckWindowExpired,
        ("Resolved", delivered) => InputInput::Resolved {
            delivered: boolean(delivered),
        },
        (t, _) => panic!("unknown input input {t}"),
    }
}

fn status_cause(v: &Value) -> StatusCause {
    serde_json::from_value(v.clone()).unwrap_or_else(|e| panic!("bad status cause {v}: {e}"))
}

fn change(v: &Value) -> StatusChanged {
    let (t, c) = tag(v);
    let cause = status_cause(&c["cause"]);
    match t {
        "ProcessChanged" => StatusChanged::ProcessState {
            from: process(&c["from"]),
            to: process(&c["to"]),
            cause,
        },
        "TurnChanged" => StatusChanged::TurnState {
            from: turn(&c["from"]),
            to: turn(&c["to"]),
            cause,
        },
        "TaskChanged" => StatusChanged::TaskState {
            from: task(&c["from"]),
            to: task(&c["to"]),
            cause,
        },
        "AttentionChanged" => StatusChanged::Attention {
            from: causes(&c["from"]),
            to: causes(&c["to"]),
            cause,
        },
        t => panic!("unknown change {t}"),
    }
}

fn gate_decision(v: &Value) -> GateDecision {
    match tag(v) {
        ("Open", _) => GateDecision::Open,
        ("Closed", r) => GateDecision::Closed(match tag(r).0 {
            "NotBound" => GateClosedReason::NotBound,
            "ProcessNotRunning" => GateClosedReason::ProcessNotRunning,
            "NoEvidence" => GateClosedReason::NoEvidence,
            "WriteAfterEvidence" => GateClosedReason::WriteAfterEvidence,
            "EvidenceOfSameTurn" => GateClosedReason::EvidenceOfSameTurn,
            "TurnNotIdle" => GateClosedReason::TurnNotIdle,
            "InputInFlight" => GateClosedReason::InputInFlight,
            "BracketedPasteOff" => GateClosedReason::BracketedPasteOff,
            t => panic!("unknown gate reason {t}"),
        }),
        (t, _) => panic!("unknown gate decision {t}"),
    }
}

fn thread_state(v: &Value) -> ThreadState {
    ThreadState {
        process: process(&v["process"]),
        turn: turn(&v["turn"]),
        task: task(&v["task"]),
        attention: set(&v["attention"]).iter().map(cause).collect(),
    }
}

/// Input id -> (launch number, state).
type Inputs = BTreeMap<u64, (u64, InputState)>;

fn inputs(v: &Value) -> Inputs {
    map(v)
        .into_iter()
        .map(|(k, i)| (int(k), (int(&i["launch"]), input_state(&i["state"]))))
        .collect()
}

/// The gate view of the model state, with the thread axes and `in_flight` taken from
/// the real state (`blocks_gate` over every input).
fn gate_view(st: &Value, thread: &ThreadState, real: &Inputs) -> GateView {
    let gp = &st["gp"];
    let current = launch_id(int(&st["launch"]));
    GateView {
        bound: boolean(&gp["bound"]),
        process: thread.process,
        turn: thread.turn,
        readiness: position(&gp["readinessSeq"]).map(|seq| ReadinessMark {
            seq,
            evidence: evidence(&gp["evidence"]),
        }),
        last_write: position(&gp["lastWrite"]),
        last_human_write: position(&gp["lastHumanWrite"]),
        last_turn_start: position(&gp["lastTurnStart"]),
        in_flight: real
            .values()
            .any(|&(l, s)| blocks_gate(Some(launch_id(l)), s, current)),
        needs_bracketed_paste: boolean(&gp["needsBracketedPaste"]),
        bracketed_paste_on: boolean(&gp["bracketedPasteOn"]),
    }
}

/// Replays one trace; returns the number of reducer calls checked.
fn replay(name: &str, itf: &Value) -> usize {
    let states = list(&itf["states"]);
    assert!(!states.is_empty(), "{name}: no states");

    let (init, task_given) = tag(&states[0]["last"]["act"]);
    assert_eq!(init, "Init", "{name}: the first state is not Init");
    let mut thread = ThreadState::registered(boolean(task_given));
    let mut real: Inputs = BTreeMap::new();
    let mut checked = 0;

    for (k, s) in states.iter().enumerate() {
        let st = &s["st"];
        let last = &s["last"];
        let at = format!("{name} state {k} ({})", last["act"]);

        for (j, call) in list(&last["calls"]).iter().enumerate() {
            let ok = boolean(&call["ok"]);
            match tag(&call["kind"]) {
                ("Reduce", i) => {
                    let input = state_input(i);
                    match reduce(&thread, &input) {
                        Ok((next, changes)) => {
                            assert!(
                                ok,
                                "{at} call {j}: reduce accepted {input:?}, the model rejects it"
                            );
                            let expected: Vec<_> =
                                list(&call["changes"]).iter().map(change).collect();
                            assert_eq!(
                                changes, expected,
                                "{at} call {j}: StatusChanged of {input:?}"
                            );
                            thread = next;
                        }
                        Err(e) => assert!(
                            !ok,
                            "{at} call {j}: reduce rejected {input:?} ({e}), the model accepts it"
                        ),
                    }
                }
                ("Step", c) => {
                    let id = int(&c["id"]);
                    let input = input_input(&c["input"]);
                    let (launch, cur) = *real
                        .get(&id)
                        .unwrap_or_else(|| panic!("{at} call {j}: step of unknown input {id}"));
                    match step(cur, &input) {
                        Ok(next) => {
                            assert!(
                                ok,
                                "{at} call {j}: step accepted {input:?} from {cur:?}, the model rejects it"
                            );
                            real.insert(id, (launch, next));
                        }
                        Err(e) => assert!(
                            !ok,
                            "{at} call {j}: step rejected {input:?} from {cur:?} ({e}), the model accepts it"
                        ),
                    }
                }
                ("Create", c) => {
                    let id = int(&c["id"]);
                    assert!(
                        real.insert(id, (int(&c["launch"]), input_state(&c["initial"])))
                            .is_none(),
                        "{at} call {j}: input {id} created twice"
                    );
                }
                (t, _) => panic!("{at}: unknown call {t}"),
            }
            checked += 1;
        }

        assert_eq!(thread, thread_state(&st["thread"]), "{at}: thread state");
        assert_eq!(real, inputs(&st["inputs"]), "{at}: input states");
        let view = gate_view(st, &thread, &real);
        assert_eq!(
            gate(&view),
            gate_decision(&last["gate"]),
            "{at}: gate of {view:?}"
        );
    }
    checked
}

#[test]
fn committed_traces_replay_against_the_reducers() {
    let mut files: Vec<PathBuf> = fs::read_dir(traces_dir())
        .expect("specs/traces exists")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.to_str().is_some_and(|s| s.ends_with(".itf.json")))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no trace under specs/traces");

    for path in &files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("UTF-8 file name");
        let text = fs::read_to_string(path).expect("read trace");
        let itf: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let calls = replay(name, &itf);
        assert!(calls > 0, "{name}: no reducer call to check");
    }
}
