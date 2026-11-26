//! `stop --assume-dead` without `--launch` (`docs/design/supervisor-recovery.md` §2.5
//! "Preview"; PR10a2-CL-9) against a stub that serves the thread summary and backward pages
//! of a scripted thread stream, as the daemon's `GET /api/events` does.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use chaldea_core::ids::LaunchId;
use chaldea_core::payload::ExitReason;
use chaldea_core::state::{ProcessState, TaskState, TurnState};
use chaldea_core::sync::{LaunchInfo, ThreadSummary};
use chaldea_daemon::lifecycle::ExitCode;
use serde_json::{Value, json};

use super::{Found, L, RUN, Reply, Seen, Stub, T, error, ok, run, run_with};

const MAX: u64 = 9_007_199_254_740_991;
/// A `tool_activity` payload's `input` and `output` sizes: 8 such events are over the
/// client's 1 MiB body limit, 4 are within it.
const BIG: usize = 64 * 1024;
const PID: u32 = 4242;

fn at(seq: u64) -> String {
    format!("2026-11-26T00:00:{:02}.{:03}Z", seq / 1000, seq % 1000)
}

fn env(seq: u64, kind: &str, launch: Option<LaunchId>, payload: Value) -> Value {
    let mut e = json!({
        "stream_id": format!("thread:{T}"),
        "seq": seq,
        "event_id": uuid::Uuid::from_u128(u128::from(seq)),
        "schema_version": 1,
        "idempotency_key": format!("k:{seq}"),
        "kind": kind,
        "thread_id": T,
        "launch_id": launch,
        "command_id": null,
        "source": { "kind": "daemon", "agent": null, "cli_version": null, "transcript": null },
        "occurred_at": at(seq),
        "recorded_at": at(seq),
    });
    e["payload"] = payload;
    e
}

/// A scripted thread stream, appended in seq order from 1.
#[derive(Clone, Default)]
struct Stream(Vec<Value>);

impl Stream {
    fn push(&mut self, kind: &str, launch: Option<LaunchId>, payload: Value) -> u64 {
        let seq = self.0.len() as u64 + 1;
        self.0.push(env(seq, kind, launch, payload));
        seq
    }

    fn pad(&mut self, n: usize) {
        for _ in 0..n {
            self.push("user_message", None, json!({ "text": "x" }));
        }
    }

    fn big(&mut self, n: usize) {
        for _ in 0..n {
            self.push(
                "tool_activity",
                Some(L),
                json!({ "input": "i".repeat(BIG), "output": "o".repeat(BIG) }),
            );
        }
    }

    fn reserved(&mut self) -> u64 {
        self.push(
            "launch_reserved",
            Some(L),
            json!({ "launch_id": L, "expected_previous": null, "agent": "codex",
                    "argv_digest": "d", "manual": false, "worktree_id": null }),
        )
    }

    fn started(&mut self) {
        self.push(
            "launch_started",
            Some(L),
            json!({ "launch_id": L, "pid": PID, "start_time": "st-4242",
                    "pgid_or_job": "4242", "cli_version": null }),
        );
    }

    fn exited(&mut self, reason: &str, failed_start: Option<Value>) {
        let mut p = json!({ "launch_id": L, "code": null, "signal": null,
                            "reason": reason, "death_confirmed": false });
        if let Some(f) = failed_start {
            p["failed_start"] = f;
        }
        self.push("launch_exited", Some(L), p);
    }

    fn evidence(&mut self, source: &str, run: Option<&str>, ids: &Value, unchecked: &Value) {
        self.push(
            "launch_evidence",
            Some(L),
            json!({ "launch_id": L, "source": source, "identities": ids,
                    "unchecked": unchecked, "probe_run": run }),
        );
    }

    fn confirmed(&mut self) -> u64 {
        self.push(
            "launch_death_confirmed",
            Some(L),
            json!({ "launch_id": L, "method": "owner_asserted" }),
        )
    }

    /// The backward page `GET /api/events` answers: up to `limit` events below `before`.
    fn page(&self, before: u64, limit: usize, epoch: &str) -> String {
        let below: Vec<&Value> = self
            .0
            .iter()
            .filter(|e| e["seq"].as_u64().unwrap() < before)
            .collect();
        let from = below.len().saturating_sub(limit);
        let events: Vec<&Value> = below[from..].to_vec();
        let lowest = events.first().map_or(1, |e| e["seq"].as_u64().unwrap());
        json!({
            "mode": "replay",
            "stream_id": format!("thread:{T}"),
            "epoch": epoch,
            "events": events,
            "head_seq": self.0.len(),
            "has_more": false,
            "has_more_before": lowest > 1,
        })
        .to_string()
    }
}

fn summary(reason: Option<ExitReason>, confirmed: bool, pid: Option<u32>) -> String {
    serde_json::to_string(&ThreadSummary {
        thread_id: T,
        closed: false,
        process_state: if reason.is_some() {
            ProcessState::Exited
        } else {
            ProcessState::Running
        },
        turn_state: TurnState::Idle,
        task_state: TaskState::None,
        attention_count: 0,
        launch: Some(LaunchInfo {
            launch_id: L,
            pid,
            manual: false,
            exit_reason: reason,
            death_confirmed: confirmed,
        }),
    })
    .unwrap()
}

/// `before_seq` and `limit` of an events request.
fn query(path: &str) -> (u64, usize) {
    let q = path.split_once('?').unwrap().1;
    let get = |k: &str| {
        q.split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{k}=")))
            .unwrap()
            .to_owned()
    };
    assert_eq!(get("stream_id"), format!("thread:{T}"));
    (
        get("before_seq").parse().unwrap(),
        get("limit").parse().unwrap(),
    )
}

/// Page `n`'s answer for `(before, limit)`, when it is not the stream's page.
type Hook = Box<dyn Fn(usize, u64, usize, &Stream) -> Option<Reply> + Send>;

/// The stub daemon: the first summary read answers `first`, later ones `later`; page `n`
/// (from 0) answers `hook(n, before, limit)` when it returns one, else the stream's page.
struct Daemon {
    first: String,
    later: String,
    stream: Stream,
    hook: Hook,
}

impl Daemon {
    fn new(first: String, stream: Stream) -> Daemon {
        Daemon {
            later: first.clone(),
            first,
            stream,
            hook: Box::new(|_, _, _, _| None),
        }
    }

    fn start(self) -> Stub {
        let summaries = AtomicUsize::new(0);
        let pages = AtomicUsize::new(0);
        Stub::serve(200, move |req: &Seen| {
            if req.path.starts_with("/api/threads/") {
                let n = summaries.fetch_add(1, Ordering::SeqCst);
                return ok(if n == 0 {
                    self.first.clone()
                } else {
                    self.later.clone()
                });
            }
            assert!(req.path.starts_with("/api/events?"), "{}", req.path);
            let (before, limit) = query(&req.path);
            let n = pages.fetch_add(1, Ordering::SeqCst);
            (self.hook)(n, before, limit, &self.stream)
                .unwrap_or_else(|| ok(self.stream.page(before, limit, "ep-1")))
        })
    }
}

fn preview(stub: &Stub) -> (ExitCode, String, String) {
    run(stub, |c, o, e| c.preview(T, o, e))
}

/// `(before_seq, limit)` of each events request.
fn pages(stub: &Stub) -> Vec<(u64, usize)> {
    stub.seen()
        .iter()
        .filter(|s| s.path.starts_with("/api/events?"))
        .map(|s| query(&s.path))
        .collect()
}

fn rerun_line() -> String {
    format!(
        "Run `chaldea stop {T} --assume-dead` again to read the evidence before confirming that this launch has ended."
    )
}

fn confirm_line() -> String {
    format!("To confirm that this launch has ended: chaldea stop {T} --assume-dead --launch {L}")
}

const NO_PROBE: &str = "The running daemon has not recorded a probe of this launch: it may still be running, or it was skipped (see the daemon log).";
const NO_REPORT: &str = "No check report was recorded for this launch (it ended under a daemon version that did not record one).";
const TREE: &str = "  not covered: the kill did not prove the process tree dead: processes it did not list may exist";

/// A stopped launch whose close evidence lies below 16 large events: the pages halve at the
/// same `before_seq` on the body-limit error only, the scan stops at the exit, nothing is
/// sent, and the report lists the evidence.
#[test]
fn assume_dead_preview() {
    let mut s = Stream::default();
    s.pad(4);
    s.reserved();
    s.started();
    s.pad(1);
    s.exited("stopped", None);
    let ev = s.0.len() as u64 + 1;
    s.evidence(
        "close",
        None,
        &json!([{ "pid": PID, "start_time": "st-4242", "observed": "unresolved" }]),
        &json!(["tree_unproven"]),
    );
    s.big(16);
    let stub = Daemon::new(summary(Some(ExitReason::Stopped), false, Some(PID)), s).start();
    let (code, out, err) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}{err}");
    assert!(stub.posts().is_empty());
    assert_eq!(
        pages(&stub),
        [
            (MAX, 16),
            (MAX, 8),
            (MAX, 4),
            (22, 4),
            (18, 4),
            (14, 4),
            (10, 4)
        ]
    );
    let want = [
        format!("launch: {L}"),
        "exit reason: stopped".to_owned(),
        "death: unconfirmed".to_owned(),
        format!("recorded identity: pid {PID}, start time st-4242"),
        "evidence:".to_owned(),
        format!("  {} close", at(ev)),
        format!("    pid {PID}, start time st-4242: unresolved (not observed)"),
        format!("  {TREE}"),
        confirm_line(),
    ];
    assert_eq!(out.lines().collect::<Vec<_>>(), want, "{out}");
    assert!(err.is_empty(), "{err}");
}

/// A response that is not the body-limit error is not retried smaller: a malformed page and a
/// 500 end the scan interrupted (exit 1, no "no report" line, the command still printed).
#[test]
fn preview_interrupted_by_a_bad_page() {
    for bad in [ok("{".to_owned()), Reply::Json(500, error("internal"))] {
        let mut s = Stream::default();
        s.reserved();
        s.started();
        s.exited("stopped", None);
        let mut d = Daemon::new(summary(Some(ExitReason::Stopped), false, Some(PID)), s);
        let bad = Mutex::new(Some(bad));
        d.hook = Box::new(move |n, _, _, _| {
            if n == 0 {
                bad.lock().unwrap().take()
            } else {
                None
            }
        });
        let stub = d.start();
        let (code, out, _) = preview(&stub);
        assert_eq!(code, ExitCode::Failed, "{out}");
        assert_eq!(pages(&stub), [(MAX, 16)]);
        assert!(!out.contains(NO_REPORT), "{out}");
        assert!(
            out.contains("The history could not be read completely ("),
            "{out}"
        );
        // An interrupted read offers no confirmation, only a new read.
        assert!(!out.contains("--launch"), "{out}");
        assert!(out.ends_with(&format!("{}\n", rerun_line())), "{out}");
    }
}

/// 20 probe events after a `daemon_restart` exit put the exit on page two; an epoch change,
/// a page of another stream, or a 500 between the pages interrupts the scan.
#[test]
fn preview_interrupted_between_pages() {
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.exited("daemon_restart", None);
    for _ in 0..20 {
        s.evidence(
            "probe",
            Some("run-0"),
            &json!([{ "pid": PID, "start_time": "st-4242", "observed": "alive" }]),
            &json!(["unrecorded_session_members"]),
        );
    }
    let epoch: Hook = Box::new(|n, b, l, s: &Stream| (n == 1).then(|| ok(s.page(b, l, "ep-2"))));
    let failed: Hook = Box::new(|n, _, _, _| (n == 1).then(|| Reply::Json(500, error("internal"))));
    let other: Hook = Box::new(|n, b, l, s: &Stream| {
        (n == 1).then(|| {
            ok(s.page(b, l, "ep-1").replace(
                &format!("\"thread:{T}\""),
                &format!("\"thread:{}\"", chaldea_core::ids::ThreadId::from_u128(0x8)),
            ))
        })
    });
    // The control: both pages read, complete.
    let control: Hook = Box::new(|_, _, _, _| None);
    for (hook, complete) in [
        (control, true),
        (epoch, false),
        (other, false),
        (failed, false),
    ] {
        let mut d = Daemon::new(
            summary(Some(ExitReason::DaemonRestart), false, Some(PID)),
            s.clone(),
        );
        d.hook = hook;
        let stub = d.start();
        let (code, out, _) = preview(&stub);
        assert_eq!(pages(&stub).len(), 2, "{out}");
        if complete {
            assert_eq!(code, ExitCode::Ok, "{out}");
            assert_eq!(out.matches(" probe (an earlier daemon run)").count(), 20);
            assert!(!out.contains("could not be read completely"), "{out}");
        } else {
            assert_eq!(code, ExitCode::Failed, "{out}");
            assert!(
                out.contains("The history could not be read completely ("),
                "{out}"
            );
        }
        assert!(out.contains(NO_PROBE), "{out}");
    }
}

/// The serving run replaced between the first read and the re-check interrupts the scan.
#[test]
fn preview_bound_to_one_run() {
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.exited("daemon_restart", None);
    s.evidence(
        "probe",
        Some(RUN),
        &json!([{ "pid": PID, "start_time": "st-4242", "observed": "gone" }]),
        &json!(["unrecorded_session_members"]),
    );
    let stub = Daemon::new(
        summary(Some(ExitReason::DaemonRestart), false, Some(PID)),
        s,
    )
    .start();
    let port = stub.port;
    // Discovery runs once for step 1, and before and after each adopted request: the summary,
    // the page, and the re-check's summary (7 in all). A second run serving from any of
    // them on interrupts the preview, the last one (between the re-check's request and its
    // adoption) included.
    for switch in 1..7 {
        let calls = AtomicUsize::new(0);
        let discover = || {
            let instance = if calls.fetch_add(1, Ordering::SeqCst) < switch {
                RUN
            } else {
                "run-2"
            };
            Ok(Found::Port(port, instance.to_owned()))
        };
        let sent = stub.gets();
        let (code, out, err) = run_with(&stub, &discover, |c, o, e| c.preview(T, o, e));
        assert_eq!(code, ExitCode::Failed, "{switch}: {out}{err}");
        if switch == 1 {
            // Another run serves before the first request: nothing is sent to it.
            assert_eq!(stub.gets(), sent, "{out}{err}");
        }
        assert!(
            out.contains("(the daemon restarted during the read)")
                || err.contains("the daemon restarted during the read"),
            "{switch}: {out}{err}"
        );
        assert!(calls.load(Ordering::SeqCst) >= switch);
    }
    // No daemon serving after a response was read (after the summary, the page, or the
    // re-check's summary): the read ends, naming a stop rather than a restart.
    for switch in [2, 4, 6] {
        let calls = AtomicUsize::new(0);
        let discover = || {
            if calls.fetch_add(1, Ordering::SeqCst) < switch {
                Ok(Found::Port(port, RUN.to_owned()))
            } else {
                Ok(Found::NotRunning)
            }
        };
        let (code, out, err) = run_with(&stub, &discover, |c, o, e| c.preview(T, o, e));
        assert_eq!(code, ExitCode::Failed, "{switch}: {out}{err}");
        assert!(
            out.contains("(the daemon stopped during the read)")
                || err.contains("the daemon stopped during the read"),
            "{switch}: {out}{err}"
        );
        assert!(
            !out.contains("restarted") && !err.contains("restarted"),
            "{switch}: {out}{err}"
        );
    }
    // Discovery not able to tell after a response (starting, stopping, a record not
    // readable yet): the read ends, naming neither a stop nor a restart.
    for switch in [2, 4, 6] {
        let calls = AtomicUsize::new(0);
        let discover = || {
            if calls.fetch_add(1, Ordering::SeqCst) < switch {
                Ok(Found::Port(port, RUN.to_owned()))
            } else {
                Ok(Found::NotYet)
            }
        };
        let (code, out, err) = run_with(&stub, &discover, |c, o, e| c.preview(T, o, e));
        assert_eq!(code, ExitCode::Failed, "{switch}: {out}{err}");
        assert!(
            out.contains(super::super::UNVERIFIED) || err.contains(super::super::UNVERIFIED),
            "{switch}: {out}{err}"
        );
        assert!(
            !format!("{out}{err}").contains("stopped during")
                && !format!("{out}{err}").contains("restarted"),
            "{switch}: {out}{err}"
        );
    }
    // In the same run: complete, labelled this run, no missing-probe line.
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.exited("daemon_restart", None);
    s.evidence(
        "probe",
        Some(RUN),
        &json!([{ "pid": PID, "start_time": "st-4242", "observed": "gone" }]),
        &json!(["unrecorded_session_members"]),
    );
    let stub = Daemon::new(
        summary(Some(ExitReason::DaemonRestart), false, Some(PID)),
        s,
    )
    .start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(out.contains(" probe (this daemon run)"), "{out}");
    assert!(
        out.contains(&format!("pid {PID}, start time st-4242: gone (dead)")),
        "{out}"
    );
    assert!(
        out.contains(
            "not covered: members of the old session without a recorded identity were not checked"
        ),
        "{out}"
    );
    assert!(!out.contains(NO_PROBE), "{out}");
}

/// An assertion committed between the summary and the first page: interrupted, and no
/// confirmation command; a probe after a confirmation is listed under it.
#[test]
fn preview_after_a_confirmation() {
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.exited("daemon_restart", None);
    s.evidence(
        "probe",
        Some("run-0"),
        &json!([{ "pid": PID, "start_time": "st-4242", "observed": "alive" }]),
        &json!(["unrecorded_session_members"]),
    );
    let c = s.confirmed();
    s.evidence(
        "probe",
        Some(RUN),
        &json!([{ "pid": PID, "start_time": "st-4242", "observed": "gone" }]),
        &json!(["unrecorded_session_members"]),
    );
    // Confirmed during the read.
    let mut d = Daemon::new(
        summary(Some(ExitReason::DaemonRestart), false, Some(PID)),
        s.clone(),
    );
    d.later = summary(Some(ExitReason::DaemonRestart), true, Some(PID));
    let stub = d.start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Failed, "{out}");
    assert!(
        out.contains("(the launch changed during the read)"),
        "{out}"
    );
    assert!(!out.contains("To confirm"), "{out}");
    // Confirmed before: complete, the later probe under the confirmation.
    let stub = Daemon::new(summary(Some(ExitReason::DaemonRestart), true, Some(PID)), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(&"death: confirmed (method: owner_asserted)"),
        "{out}"
    );
    let under = format!("  after the confirmation by owner_asserted at {}:", at(c));
    let i = lines.iter().position(|l| *l == under).expect(&out);
    assert!(
        lines[i + 1].starts_with("    ") && lines[i + 1].ends_with(" probe (this daemon run)"),
        "{out}"
    );
    assert!(
        lines[..i]
            .iter()
            .any(|l| l.ends_with(" probe (an earlier daemon run)")),
        "{out}"
    );
    assert!(!out.contains("To confirm"), "{out}");
}

/// Launches a 10a1 daemon left without evidence, per reason, and the recorded identity.
#[test]
fn preview_missing_lines() {
    // daemon_restart: the running-daemon line; the start time not read.
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.exited("daemon_restart", None);
    let stub = Daemon::new(
        summary(Some(ExitReason::DaemonRestart), false, Some(PID)),
        s,
    )
    .start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(out.contains(NO_PROBE), "{out}");
    assert!(out.contains("evidence: none"), "{out}");
    assert!(
        out.contains(&format!(
            "recorded identity: pid {PID}, start time: not read by this preview (recorded in `launch_started`)"
        )),
        "{out}"
    );
    // stopped and exit: the no-report line.
    for (reason, r) in [("stopped", ExitReason::Stopped), ("exit", ExitReason::Exit)] {
        let mut s = Stream::default();
        s.reserved();
        s.started();
        s.exited(reason, None);
        let stub = Daemon::new(summary(Some(r), false, Some(PID)), s).start();
        let (code, out, _) = preview(&stub);
        assert_eq!(code, ExitCode::Ok, "{out}");
        assert!(out.contains(NO_REPORT), "{out}");
        assert!(!out.contains(NO_PROBE), "{out}");
    }
    // spawn_failed with failed_start: the identity printed.
    let mut s = Stream::default();
    s.reserved();
    s.exited(
        "spawn_failed",
        Some(json!({ "pid": 77, "start_time": "st-77" })),
    );
    let stub = Daemon::new(summary(Some(ExitReason::SpawnFailed), false, None), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(out.contains("recorded identity: none"), "{out}");
    assert!(
        out.contains("identity recorded at the failed start: pid 77, start time st-77"),
        "{out}"
    );
    assert!(out.contains(NO_REPORT), "{out}");
    // A prober candidate (RD1): the running-daemon line too.
    assert!(out.contains(NO_PROBE), "{out}");
    // The same launch after the owner's assertion is no candidate either: no running-daemon
    // line.
    let mut s = Stream::default();
    s.reserved();
    s.exited(
        "spawn_failed",
        Some(json!({ "pid": 77, "start_time": "st-77" })),
    );
    s.push(
        "launch_death_confirmed",
        Some(L),
        json!({ "launch_id": L, "method": "owner_asserted" }),
    );
    let stub = Daemon::new(summary(Some(ExitReason::SpawnFailed), true, None), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(
        out.contains("death: confirmed (method: owner_asserted)"),
        "{out}"
    );
    assert!(!out.contains(NO_PROBE), "{out}");
    // A failed start recorded without a start time is no candidate: no running-daemon line.
    let mut s = Stream::default();
    s.reserved();
    s.exited("spawn_failed", Some(json!({ "pid": 77 })));
    let stub = Daemon::new(summary(Some(ExitReason::SpawnFailed), false, None), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(out.contains(NO_REPORT), "{out}");
    assert!(!out.contains(NO_PROBE), "{out}");
    // unknown_after_crash: the scan reaches the reservation; its agent and recorded_at.
    let mut s = Stream::default();
    s.pad(2);
    let r = s.reserved();
    s.exited("unknown_after_crash", None);
    let stub = Daemon::new(summary(Some(ExitReason::UnknownAfterCrash), false, None), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(
        out.contains(&format!(
            "No process identity was recorded (agent codex, reserved at {}).",
            at(r)
        )),
        "{out}"
    );
    assert!(out.ends_with(&format!("{}\n", confirm_line())), "{out}");
    // A stop whose end was proven: no evidence, and no "no report" line.
    let mut s = Stream::default();
    s.reserved();
    s.started();
    s.push(
        "launch_exited",
        Some(L),
        json!({ "launch_id": L, "code": null, "signal": 9,
                "reason": "stopped", "death_confirmed": true }),
    );
    s.push(
        "launch_death_confirmed",
        Some(L),
        json!({ "launch_id": L, "method": "session_scan" }),
    );
    let stub = Daemon::new(summary(Some(ExitReason::Stopped), true, Some(PID)), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(!out.contains(NO_REPORT), "{out}");
    assert!(
        out.contains("The launch's end was proven when it exited, so no check report was needed."),
        "{out}"
    );
}

/// A failed start's recorded child: its `spawn_failed` evidence without a leader identity,
/// then this run's probe of the child on Windows. The scope lines speak of what has no
/// recorded identity, the leader for `no_identity`; the running-daemon line is gone.
#[test]
fn preview_failed_start_probed() {
    let child = json!([{ "pid": 77, "start_time": "st-77", "observed": "unresolved" }]);
    let gone = json!([{ "pid": 77, "start_time": "st-77", "observed": "gone" }]);
    let mut s = Stream::default();
    s.reserved();
    s.exited(
        "spawn_failed",
        Some(json!({ "pid": 77, "start_time": "st-77" })),
    );
    s.evidence(
        "spawn_failed",
        None,
        &child,
        &json!(["no_identity", "tree_unproven"]),
    );
    s.evidence(
        "probe",
        Some(RUN),
        &gone,
        &json!(["unrecorded_job_and_outside_job"]),
    );
    let stub = Daemon::new(summary(Some(ExitReason::SpawnFailed), false, None), s).start();
    let (code, out, _) = preview(&stub);
    assert_eq!(code, ExitCode::Ok, "{out}");
    assert!(!out.contains(NO_PROBE), "{out}");
    assert!(out.contains(" probe (this daemon run)"), "{out}");
    assert!(
        out.contains("  not covered: no process identity was recorded for the launch's leader\n"),
        "{out}"
    );
    assert!(
        out.contains(
            "  not covered: processes without a recorded identity were not checked: members of the old job (ended when the old daemon exited, not observed) and processes outside the job\n"
        ),
        "{out}"
    );
    assert!(out.ends_with(&format!("{}\n", confirm_line())), "{out}");
}

/// No launch, or a launch not exited: printed, exit 0, no page read.
#[test]
fn preview_without_an_exited_launch() {
    let none = serde_json::to_string(&ThreadSummary {
        thread_id: T,
        closed: false,
        process_state: ProcessState::Exited,
        turn_state: TurnState::Idle,
        task_state: TaskState::None,
        attention_count: 0,
        launch: None,
    })
    .unwrap();
    for (s, line) in [
        (none, "The thread has no launch.".to_owned()),
        (
            summary(None, false, Some(PID)),
            format!("Launch {L} has not exited; stop it with `chaldea stop {T}`."),
        ),
    ] {
        let stub = Daemon::new(s, Stream::default()).start();
        let (code, out, _) = preview(&stub);
        assert_eq!(code, ExitCode::Ok);
        assert_eq!(out.trim(), line);
        assert!(pages(&stub).is_empty());
        assert!(stub.posts().is_empty());
    }
}
