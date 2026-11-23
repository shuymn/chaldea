//! The command runner with fault points (design §2.8; PR7-CL-8's 7c cases, CL-18, CL-19,
//! CL-20, CL-23). Over HTTP where the case allows it; through `commands::run` directly
//! where a test needs an exact deadline or the reply's cutoff gate (which the guard's
//! credential read would otherwise take).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use chaldea_core::auth::TokenKind;
use chaldea_core::command::{ReceiptStatus, request_identity};
use chaldea_core::ids::{CommandId, StreamId};
use proptest::prelude::*;
use serde_json::value::RawValue;

use super::eventually;
use super::shared;
use super::support::commands::{CounterArgs, Probe, slow_body};
use super::support::http::{
    Reply, Req, TestServer, connect, parse, read_to_end, send, settled, write,
};
use super::support::{TestStore, soon};
use crate::http::auth::sha256;
use crate::http::commands::{Admitted, prepare_args, run};
use crate::http::{ApiError, CommandRegistry, MonotonicClock, Shared, StoreFatal};
use crate::store::test_faults::CommitFault;
use crate::store::{SubmitError, WriteError};

fn cid(n: u128) -> CommandId {
    CommandId::from_u128(0xC0DE_1000 + n)
}

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

fn spawn_send(srv: &TestServer, req: &Req) -> tokio::task::JoinHandle<Reply> {
    let (port, bytes) = (srv.port, req.bytes());
    tokio::spawn(async move { send(port, &bytes).await })
}

async fn joined<T>(h: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(20), h)
        .await
        .expect("done within 20 s")
        .unwrap()
}

async fn reached(rx: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("the gate was reached within 10 s")
        .unwrap();
}

async fn reached_sync(rx: std::sync::mpsc::Receiver<()>) {
    tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the hold was reached within 10 s");
}

/// A command for `commands::run`, prepared as the dispatcher prepares it.
fn admitted(s: &Shared, name: &str, secret: &str, id: CommandId, args: &str) -> Admitted {
    let handler = s.commands.handler(name).expect("a test handler");
    let raw: Box<RawValue> = serde_json::from_str(args).unwrap();
    let (prepared, identity) = prepare_args(handler.as_ref(), name, &raw).unwrap();
    Admitted {
        command_id: id,
        token_sha256: sha256(secret),
        identity,
        intent: prepared.intent,
        handler,
    }
}

/// Holds the writer and fills its queue (256 jobs); returns the release and the queued
/// tickets.
async fn fill_queue(
    srv: &TestServer,
) -> (
    std::sync::mpsc::Sender<()>,
    Vec<crate::store::WriteTicket<()>>,
) {
    let writer = srv.hold_writer().await;
    let mut queued = Vec::new();
    loop {
        match srv.store.store().submit(soon(), |_tx| Ok(())) {
            Ok(t) => queued.push(t),
            Err(SubmitError::QueueFull) => break,
            Err(e) => panic!("{e}"),
        }
        assert!(queued.len() <= 1024, "the queue never filled");
    }
    assert_eq!(queued.len(), 256);
    (writer, queued)
}

fn host_events_of(srv: &TestServer, id: CommandId) -> usize {
    srv.store
        .read(|tx| tx.events_after(StreamId::Host, 0, 1000))
        .events
        .iter()
        .filter(|e| e.command_id == Some(id))
        .count()
}

/// 32 concurrent requests with one `command_id` reach the writer as 32 jobs (all queued
/// behind a held writer before any runs): the first runs the token check, the receipt, and
/// the intent in one transaction, so every other job finds the completed receipt; one
/// intent, one event, and 32 equal replies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicates_one_intent() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let writer = srv.hold_writer().await;
    let before = s.faults.submits();
    let req = srv.command("test_counter", &owner, cid(1), r#"{"n":5}"#);
    let replies: Vec<_> = (0..32).map(|_| spawn_send(&srv, &req)).collect();
    eventually(Duration::from_secs(10), "32 intent jobs submitted", || {
        s.faults.submits() == before + 32
    })
    .await;
    writer.send(()).unwrap();
    let mut got = Vec::new();
    for r in replies {
        let r = joined(r).await;
        got.push((r.status, r.body));
    }
    assert!(got.iter().all(|g| *g == got[0]), "{got:?}");
    assert_eq!(got[0].0, 200);
    assert_eq!(probe.intents(), 1);
    assert_eq!(host_events_of(&srv, cid(1)), 1);
    srv.stop().await;
}

/// The intent job is taken after its `not_after` (a test job occupies the writer until
/// just past the deadline) and `Expired` arrives within the grace: 408, no receipt, nothing
/// run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_observed_is_408() {
    let (srv, probe) = TestServer::with_commands(|c| {
        c.write_reply_grace = Duration::from_secs(1);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let writer = srv.hold_writer().await;
    let before = s.faults.submits();
    let deadline = s.cfg.clock.now() + Duration::from_millis(300);
    let cmd = admitted(&s, "test_counter", &owner, cid(1), r#"{"n":1}"#);
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    // A lower bound: the deadline has passed; the grace (1 s) has not.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(50),
    ))
    .await;
    writer.send(()).unwrap();
    let r = joined(waiter).await;
    assert_eq!(r.err(), Some(ApiError::request_timeout()));
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// The waiter is held at the top of its loop, before it reads the progress, while the
/// intent job waits behind the writer past `deadline + write_reply_grace`; the writer then
/// replies `Expired` and the runner publishes it. Released, the waiter reads that reply
/// after the reply time: 503 `outcome_unknown`, not 408, whatever the clock read before
/// the gate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_taken_after_the_grace_is_outcome_unknown() {
    let (srv, probe) = TestServer::with_commands(|c| {
        c.write_reply_grace = Duration::from_millis(100);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let base = srv.server.tasks();
    let writer = srv.hold_writer().await;
    let (gate_reached, release) = s.faults.arm_progress_gate();
    let before = s.faults.submits();
    let deadline = s.cfg.clock.now() + Duration::from_millis(300);
    let cmd = admitted(&s, "test_counter", &owner, cid(1), r#"{"n":1}"#);
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    reached(gate_reached).await;
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    // Lower bounds: the reply time and the job's `not_after` have passed.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(150),
    ))
    .await;
    writer.send(()).unwrap();
    // The runner published `Done(Expired)` and ended.
    eventually(Duration::from_secs(10), "the runner ends", || {
        srv.server.tasks() == base
    })
    .await;
    release.send(()).unwrap();
    let r = joined(waiter).await;
    assert_eq!(r.err(), Some(ApiError::outcome_unknown()));
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// The command path's sibling of `writes::reply_after_the_grace_is_outcome_unknown`: the
/// waiter is held at its gate while the intent job waits behind the writer past `deadline +
/// write_reply_grace`; the writer then replies `Expired` and the runner publishes it.
/// Released, the waiter takes that reply after the reply time: 503 `outcome_unknown`, not
/// 408.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_after_the_grace_is_outcome_unknown() {
    let (srv, probe) = TestServer::with_commands(|c| {
        c.write_reply_grace = Duration::from_millis(100);
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let writer = srv.hold_writer().await;
    let (gate_reached, release) = s.faults.arm_cutoff_gate();
    let before = s.faults.submits();
    let deadline = s.cfg.clock.now() + Duration::from_millis(300);
    let cmd = admitted(&s, "test_counter", &owner, cid(1), r#"{"n":1}"#);
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    reached(gate_reached).await;
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    // Lower bounds: the reply time and the job's `not_after` have passed.
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(150),
    ))
    .await;
    writer.send(()).unwrap();
    drop(release);
    let r = joined(waiter).await;
    assert_eq!(r.err(), Some(ApiError::outcome_unknown()));
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// The completion's first attempts find the writer's queue full; the runner retries
/// without starting the effect again, and commits once the queue drains.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_under_contention() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"q"}"#);
    let (effect_reached, effect_release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    let (writer, queued) = fill_queue(&srv).await;
    let before = s.faults.submits();
    effect_release.send(()).unwrap();
    eventually(
        Duration::from_secs(10),
        "two completion attempts refused",
        || s.faults.submits() >= before + 2,
    )
    .await;
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
    writer.send(()).unwrap();
    for t in queued {
        settled(t).await.unwrap();
    }
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1)).unwrap().status != ReceiptStatus::InProgress
    })
    .await;
    let _ = joined(first).await;
    assert_eq!(probe.effects(), 1, "the effect ran again");
    let replay = srv.send(&req).await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("q").as_bytes())
    );
    assert!(srv.sink.0.lock().unwrap().is_empty());
    srv.stop().await;
}

/// Shutdown starts while the effect is held; the effect is released before the drain
/// deadline: the runner completes the receipt and is counted under `exited`, and after a
/// restart a retry replays the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effect_released_after_closing_completes() {
    let (mut srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"close"}"#);
    let (effect_reached, effect_release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    let release_after_closing = async {
        eventually(Duration::from_secs(10), "the registry is closing", || {
            s.registry.is_closing()
        })
        .await;
        effect_release.send(()).unwrap();
    };
    let (report, ()) = tokio::join!(
        srv.shutdown_http(Instant::now() + Duration::from_secs(10)),
        release_after_closing
    );
    assert_eq!(report.exited.runner, 1, "{report:?}");
    assert_eq!(report.abort_requested.total(), 0, "{report:?}");
    let _ = joined(first).await;
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::Completed(chaldea_core::command::CommandResult {
            http_status: 200,
            body: slow_body("close"),
        })
    );
    // The restart.
    let mut store = srv.store;
    let store = tokio::task::spawn_blocking(move || {
        store.reopen();
        store
            .run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
            .unwrap();
        store
    })
    .await
    .unwrap();
    let (srv, probe2) = TestServer::with_commands_on(store, |_| {}).await;
    let replay = srv
        .send(&srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"close"}"#))
        .await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("close").as_bytes())
    );
    assert_eq!((probe.effects(), probe2.effects()), (1, 0));
    srv.stop().await;
}

/// The server's clock on tokio's time, which a test pauses and advances: the runner's
/// budget checks and the timers it sets move together.
struct TokioClock;

impl MonotonicClock for TokioClock {
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }

    fn real(&self, at: Instant) -> Instant {
        at
    }
}

/// A server on [`TokioClock`] whose `test_slow_effect` command has its effect resolved
/// with tokio's time paused and the writer's queue full, which stays full: every
/// completion attempt is refused with `QueueFull` at once, so the runner's progress
/// depends on tokio's timers only. Returns once the runner gave up (at most 120 s of
/// paused time), with the server's clock at the first attempt.
struct Exhausted {
    srv: TestServer,
    probe: Arc<Probe>,
    first: Instant,
    /// Whether `ReceiptUncompleted` had been reported 2 ms before and 2 ms after the
    /// budget's end.
    reported: (bool, bool),
    writer: std::sync::mpsc::Sender<()>,
    queued: Vec<crate::store::WriteTicket<()>>,
}

async fn exhaust_the_budget() -> Exhausted {
    let (srv, probe) = TestServer::with_commands(|c| c.clock = Arc::new(TokioClock)).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let (effect_reached, effect_release) = probe.hold_effect();
    let deadline = s.cfg.clock.now() + Duration::from_secs(5);
    let cmd = admitted(
        &s,
        "test_slow_effect",
        &owner,
        cid(1),
        r#"{"tag":"budget"}"#,
    );
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    reached(effect_reached).await;
    let (writer, queued) = fill_queue(&srv).await;
    tokio::time::pause();
    effect_release.send(()).unwrap();
    eventually(
        Duration::from_secs(10),
        "the first completion attempt",
        || !s.faults.completion_attempts().is_empty(),
    )
    .await;
    let first = s.faults.completion_attempts()[0];
    let budget_end = tokio::time::Instant::from_std(first + Duration::from_secs(60));
    let told = || !srv.sink.0.lock().unwrap().is_empty();
    tokio::time::sleep_until(budget_end - Duration::from_millis(2)).await;
    let before_end = told();
    tokio::time::sleep_until(budget_end + Duration::from_millis(2)).await;
    let after_end = told();
    // The waiter replied at its reply time, long before.
    assert_eq!(
        joined(waiter).await.err(),
        Some(ApiError::in_progress()),
        "the waiter's reply"
    );
    // Without a report by then, the runner may still be retrying: wait out the rest
    // of the 120 s of paused time before the checks.
    let until = budget_end + Duration::from_secs(60);
    while !told() && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tokio::time::resume();
    Exhausted {
        srv,
        probe,
        first,
        reported: (before_end, after_end),
        writer,
        queued,
    }
}

impl Exhausted {
    async fn stop(self) {
        self.writer.send(()).unwrap();
        for t in self.queued {
            tokio::time::timeout(Duration::from_secs(10), t)
                .await
                .expect("a queued job within 10 s")
                .unwrap();
        }
        self.srv.stop().await;
    }
}

/// Retryable failures (`QueueFull`) persist past the 60 s budget, driven by tokio's timers:
/// no attempt is admitted at or after the budget's end, the runner gives up at it and
/// reports `ReceiptUncompleted`, the effect ran once, and the receipt stays `in_progress`.
#[tokio::test]
async fn completion_budget_ends_admission() {
    let x = exhaust_the_budget().await;
    let s = shared(&x.srv);
    assert_eq!(
        x.reported,
        (false, true),
        "reported (2 ms before, 2 ms after) the end"
    );
    let budget_end = x.first + Duration::from_secs(60);
    let attempts = s.faults.completion_attempts();
    assert!(
        attempts.iter().all(|a| *a < budget_end),
        "an attempt at or after the budget's end"
    );
    assert_eq!(
        *x.srv.sink.0.lock().unwrap(),
        [StoreFatal::ReceiptUncompleted { command_id: cid(1) }]
    );
    assert_eq!(x.probe.effects(), 1);
    assert_eq!(
        x.srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
    x.stop().await;
}

/// The pauses between completion attempts: 10 ms, doubling to 1 s, the last cut at the
/// budget's end (the attempt after the 66th would start past it). A tokio timer wakes on
/// the millisecond tick after its deadline, so each pause may be 1 ms longer.
#[tokio::test]
async fn completion_backoff_schedule() {
    let x = exhaust_the_budget().await;
    let s = shared(&x.srv);
    let attempts = s.faults.completion_attempts();
    let gaps: Vec<u128> = attempts
        .windows(2)
        .map(|w| (w[1] - w[0]).as_millis())
        .collect();
    let mut want = vec![10, 20, 40, 80, 160, 320, 640];
    want.extend([1000; 58]);
    assert_eq!(gaps.len(), want.len(), "{gaps:?}");
    assert!(
        gaps.iter()
            .zip(&want)
            .all(|(g, w)| *g == *w || *g == *w + 1),
        "{gaps:?}"
    );
    assert_eq!(
        x.reported,
        (false, true),
        "the last pause is cut at the end"
    );
    x.stop().await;
}

/// A completion attempt admitted before the 60 s budget ends and held in the writer past
/// it, on the server's clock and on tokio's timers alike (paused time advanced 61 s), is
/// awaited: it commits, nothing is reported, and the result replays.
#[tokio::test]
async fn last_admitted_attempt_is_awaited() {
    let (srv, probe) = TestServer::with_commands(|c| c.clock = Arc::new(TokioClock)).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let base = srv.server.tasks();
    let (effect_reached, effect_release) = probe.hold_effect();
    let deadline = s.cfg.clock.now() + Duration::from_secs(5);
    let cmd = admitted(&s, "test_slow_effect", &owner, cid(1), r#"{"tag":"b"}"#);
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    reached(effect_reached).await;
    let (hold_reached, hold_release) = s.faults.arm_completion_hold();
    effect_release.send(()).unwrap();
    reached_sync(hold_reached).await;
    let first = s.faults.completion_attempts()[0];
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;
    assert!(s.cfg.clock.now() > first + Duration::from_secs(60));
    tokio::time::resume();
    hold_release.send(()).unwrap();
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1)).unwrap().status != ReceiptStatus::InProgress
    })
    .await;
    eventually(Duration::from_secs(10), "the runner ends", || {
        srv.server.tasks() == base
    })
    .await;
    assert!(srv.sink.0.lock().unwrap().is_empty());
    assert_eq!(s.faults.completion_attempts().len(), 1);
    assert_eq!(joined(waiter).await.err(), Some(ApiError::in_progress()));
    let replay = srv
        .send(&srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"b"}"#))
        .await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("b").as_bytes())
    );
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// A command's own budget (`test_long_deadline`: 30 s) governs its request, not the server's
/// short deadline (2 s), measured from the request's origin: with the body incomplete, the
/// server's clock moves 20 s on, and the body completed then runs the command; moved 31 s
/// on, it is 408 with nothing submitted and no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_budget_is_its_own() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    assert_eq!(s.cfg.short_deadline, Duration::from_secs(2));
    for (n, skew, want) in [
        (1, 20, (200, None)),
        (2, 31, (408, code("request_timeout"))),
    ] {
        let bytes = srv
            .command("test_long_deadline", &owner, cid(n), "{}")
            .bytes();
        let (head, last) = bytes.split_at(bytes.len() - 1);
        let (polls, submits) = (s.faults.polls(), s.faults.submits());
        let mut c = connect(srv.port).await;
        write(&mut c, head).await;
        eventually(Duration::from_secs(10), "the body is polled", || {
            s.faults.polls() > polls
        })
        .await;
        srv.clock.skew(Duration::from_secs(skew));
        write(&mut c, last).await;
        let (out, end) = read_to_end(&mut c).await;
        srv.clock.skew(Duration::ZERO);
        let r = parse(&out, end);
        assert_eq!(r.outcome(), want, "skewed {skew} s: {r:?}");
        if r.status == 408 {
            assert_eq!(s.faults.submits(), submits, "a write was submitted");
            assert_eq!(srv.receipt(cid(n)), None);
        } else {
            assert_eq!(r.body, br#"{"long":true}"#);
        }
    }
    srv.stop().await;
}

/// A command whose body is still incomplete at its deadline is 408 with nothing submitted
/// and no receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incomplete_command_body_is_408() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let bytes = srv
        .command("test_counter", &owner, cid(1), r#"{"n":1}"#)
        .bytes();
    let submits = s.faults.submits();
    let mut c = connect(srv.port).await;
    write(&mut c, &bytes[..bytes.len() - 1]).await;
    let (out, end) = read_to_end(&mut c).await;
    assert_eq!(parse(&out, end).outcome(), (408, code("request_timeout")));
    assert_eq!(s.faults.submits(), submits, "a write was submitted");
    assert_eq!(srv.receipt(cid(1)), None);
    assert_eq!(probe.intents(), 0);
    srv.stop().await;
}

/// The writer fails while the effect runs: the completion cannot commit, `Failed` and
/// `ReceiptUncompleted` reach the sink, the effect is not run again, and the waiting
/// request gets 503 `outcome_unknown`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completion_failure_is_fatal() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"f"}"#);
    let (effect_reached, effect_release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let r = srv
        .store
        .store()
        .submit(soon(), |_tx| Ok(()))
        .map(settled)
        .unwrap()
        .await;
    assert!(matches!(r, Err(WriteError::CommitUnknown(_))), "{r:?}");
    effect_release.send(()).unwrap();
    assert_eq!(
        joined(first).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert_eq!(
        *srv.sink.0.lock().unwrap(),
        [
            StoreFatal::Failed,
            StoreFatal::ReceiptUncompleted { command_id: cid(1) }
        ]
    );
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// The intent's commit outcome is unknown after its request has replied (waiter gone): the
/// runner still reports `CommitUnknown`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_commit_unknown_reaches_the_sink() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let (intent_reached, intent_release) = probe.hold_intent();
    *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let first = spawn_send(
        &srv,
        &srv.command("test_counter", &owner, cid(1), r#"{"n":1}"#),
    );
    reached_sync(intent_reached).await;
    assert_eq!(
        joined(first).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert!(srv.sink.0.lock().unwrap().is_empty());
    intent_release.send(()).unwrap();
    let sink = srv.sink.clone();
    eventually(Duration::from_secs(10), "the sink is told", move || {
        !sink.0.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(*srv.sink.0.lock().unwrap(), [StoreFatal::CommitUnknown]);
    srv.stop().await;
}

/// `Intent::Pending` from a handler without an effect is refused inside the job, which
/// rolls back the receipt row and the intent's own write: 500 `internal`, no receipt, and
/// no event, on every retry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_without_effect_rolls_back() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let req = srv.command("test_refuse", &owner, cid(1), r#"{"how":"pending"}"#);
    for _ in 0..2 {
        assert_eq!(srv.send(&req).await.outcome(), (500, code("internal")));
        assert_eq!(srv.receipt(cid(1)), None);
        assert_eq!(host_events_of(&srv, cid(1)), 0);
    }
    srv.stop().await;
}

/// The store outcomes of the intent job through the writer's fault points, mapped by §2.8's
/// table; fatal ones reach the sink.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn store_outcomes_map_to_statuses() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let counter = |n: u128| srv.command("test_counter", &owner, cid(n), r#"{"n":1}"#);
    // RolledBack(Busy): `COMMIT` busy, nothing committed.
    *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    let r = srv.send(&counter(1)).await;
    assert_eq!(r.outcome(), (503, code("busy")));
    assert_eq!(r.body, br#"{"code":"busy","retry_after":1}"#);
    assert_eq!(srv.receipt(cid(1)), None);
    // QueueFull.
    let (writer, queued) = fill_queue(&srv).await;
    assert_eq!(srv.send(&counter(2)).await.outcome(), (503, code("busy")));
    writer.send(()).unwrap();
    for t in queued {
        settled(t).await.unwrap();
    }
    assert_eq!(srv.receipt(cid(2)), None);
    // Rejected(Refused): the command's own refusal; Panicked: 500.
    let refuse = |n: u128, how: &str| {
        srv.command(
            "test_refuse",
            &owner,
            cid(n),
            &format!(r#"{{"how":"{how}"}}"#),
        )
    };
    assert_eq!(
        srv.send(&refuse(3, "forbidden")).await.outcome(),
        (403, code("forbidden"))
    );
    assert_eq!(
        srv.send(&refuse(4, "panic")).await.outcome(),
        (500, code("internal"))
    );
    assert_eq!((srv.receipt(cid(3)), srv.receipt(cid(4))), (None, None));
    // CommitUnknown, then Failed for every later submission.
    *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    assert_eq!(
        srv.send(&counter(5)).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert_eq!(
        srv.send(&counter(6)).await.outcome(),
        (503, code("store_failed"))
    );
    assert_eq!(
        *srv.sink.0.lock().unwrap(),
        [StoreFatal::CommitUnknown, StoreFatal::Failed]
    );
    // The intents of cid 1 (rolled back at its busy commit) and cid 5 ran; no other did.
    assert_eq!(probe.intents(), 2);
    srv.stop().await;

    // WriterGone: the writer thread ends with the intent job queued; then Closed.
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let writer = srv.hold_writer().await;
    srv.store
        .store()
        .faults()
        .panic_outside_job
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let before = s.faults.submits();
    let first = spawn_send(
        &srv,
        &srv.command("test_counter", &owner, cid(7), r#"{"n":1}"#),
    );
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    writer.send(()).unwrap();
    assert_eq!(
        joined(first).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert_eq!(*srv.sink.0.lock().unwrap(), [StoreFatal::WriterGone]);
    let r = srv
        .send(&srv.command("test_counter", &owner, cid(8), r#"{"n":1}"#))
        .await;
    assert_eq!(r.outcome(), (503, code("shutting_down")));
    // The writer is gone: the store is dropped without the writer-exit check of `stop`.
    let mut srv = srv;
    srv.shutdown_http(Instant::now() + Duration::from_secs(10))
        .await;
}

/// A test table naming a command of `MVP_COMMANDS` again is refused.
#[test]
#[should_panic(expected = "command send is in the table twice")]
fn mvp_plus_refuses_a_repeated_name() {
    static AGAIN: &[chaldea_core::auth::CommandSpec] = &[chaldea_core::auth::CommandSpec {
        name: "send",
        action: chaldea_core::auth::Action::Send,
        owner_pr: 7,
    }];
    let _ = CommandRegistry::mvp_plus(AGAIN);
}

/// The result is replied only after its completion committed: the completion is observed
/// submitted behind a held writer before the first reply is read, and the waiting request
/// gets 409 `in_progress` at its reply time with the receipt still `in_progress`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_result_follows_its_completion() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let req = srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"c"}"#);
    let (effect_reached, release) = probe.hold_effect();
    let first = spawn_send(&srv, &req);
    reached(effect_reached).await;
    let writer = srv.hold_writer().await;
    let before = s.faults.submits();
    // The effect resolves now; its completion is submitted and waits behind the writer.
    release.send(()).unwrap();
    eventually(
        Duration::from_secs(10),
        "the completion is submitted",
        || s.faults.submits() == before + 1,
    )
    .await;
    assert_eq!(joined(first).await.outcome(), (409, code("in_progress")));
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
    writer.send(()).unwrap();
    eventually(Duration::from_secs(10), "the receipt completes", || {
        srv.receipt(cid(1)).unwrap().status != ReceiptStatus::InProgress
    })
    .await;
    let replay = srv.send(&req).await;
    assert_eq!(
        (replay.status, replay.body.as_slice()),
        (200, slow_body("c").as_bytes())
    );
    srv.stop().await;
}

/// The reply table's statuses and bodies for the receipt lookups.
#[test]
fn receipt_replies() {
    assert_eq!(
        (
            ApiError::in_progress().status(),
            ApiError::in_progress().retry_after()
        ),
        (StatusCode::CONFLICT, Some(1))
    );
    assert_eq!(ApiError::conflict().status(), StatusCode::CONFLICT);
    assert_eq!(ApiError::conflict().retry_after(), None);
    assert_eq!(
        ApiError::unknown_after_crash().status(),
        StatusCode::CONFLICT
    );
    assert_eq!(ApiError::bad_args().status(), StatusCode::BAD_REQUEST);
}

// ---- request identity (PR7-CL-23) -----------------------------------------------------------

/// The identity of `args` under `name`, as the dispatcher computes it.
fn identity(r: &CommandRegistry, name: &str, args: &str) -> Result<[u8; 32], ApiError> {
    let handler = r.handler(name).expect("a test handler");
    let raw: Box<RawValue> = serde_json::from_str(args).unwrap();
    prepare_args(handler.as_ref(), name, &raw).map(|(_, id)| id)
}

/// The test registry, for the identity tests (no server).
fn identity_registry() -> (TestStore, CommandRegistry) {
    let store = TestStore::new();
    let commands = super::support::commands::registry(store.store(), &Arc::new(Probe::default()));
    (store, commands)
}

/// A JSON spelling of `v` with each object's members in an order picked by `seed`, optional
/// whitespace, and each float spelled one of three ways.
fn spell(v: &serde_json::Value, seed: &mut u64, out: &mut String) {
    fn next(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *seed >> 33
    }
    let ws = |n: u64| [" ", "", "\n\t", ""][usize::try_from(n % 4).unwrap()];
    match v {
        serde_json::Value::Object(m) => {
            let mut members: Vec<(&String, &serde_json::Value)> = m.iter().collect();
            for i in (1..members.len()).rev() {
                let j = usize::try_from(next(seed)).unwrap() % (i + 1);
                members.swap(i, j);
            }
            out.push('{');
            out.push_str(ws(next(seed)));
            for (i, (k, v)) in members.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap());
                out.push_str(ws(next(seed)));
                out.push(':');
                spell(v, seed, out);
            }
            out.push('}');
        }
        serde_json::Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                spell(x, seed, out);
            }
            out.push(']');
        }
        serde_json::Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            let s = match next(seed) % 3 {
                0 => format!("{f:?}"),
                1 => format!("{f:e}"),
                _ => format!("{f}"),
            };
            out.push_str(&s);
        }
        other => out.push_str(&serde_json::to_string(other).unwrap()),
    }
}

fn counter_args() -> impl Strategy<Value = CounterArgs> {
    (
        any::<u32>(),
        "[a-zé😀 ]{0,8}",
        -4000_i32..4000,
        proptest::collection::vec("[a-z]{0,3}", 0..4),
        proptest::collection::hash_map("[a-zA-Zé]{1,4}", "[a-z0-9]{0,3}", 0..6),
        proptest::collection::vec(
            proptest::collection::btree_map("[a-z]{1,3}", any::<u32>(), 0..4),
            0..3,
        ),
    )
        .prop_map(|(n, label, q, tags, env, layers)| CounterArgs {
            n,
            label,
            ratio: f64::from(q) / 4.0,
            tags,
            env,
            layers,
            target: None,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Requests that mean the same arguments share one identity, whatever their member
    /// order (at every level, a map field included), whitespace, or float spelling.
    #[test]
    fn identity_is_canonical(args in counter_args(), seed_a in any::<u64>(), seed_b in any::<u64>()) {
        let (_store, r) = identity_registry();
        let value = serde_json::to_value(&args).unwrap();
        let (mut a, mut b) = (String::new(), String::new());
        spell(&value, &mut seed_a.clone(), &mut a);
        spell(&value, &mut seed_b.clone(), &mut b);
        let ia = identity(&r, "test_counter", &a).unwrap();
        let ib = identity(&r, "test_counter", &b).unwrap();
        prop_assert_eq!(ia, ib, "{} vs {}", a, b);
    }
}

/// Decimal spellings of one value share one identity, also where the two parse paths of
/// `serde_json` round differently without its `float_roundtrip` feature.
#[test]
fn decimal_respelling_keeps_identity() {
    let (_store, r) = identity_registry();
    assert_eq!(
        identity(&r, "test_counter", r#"{"n":1,"ratio":10000000000000001}"#).unwrap(),
        identity(&r, "test_counter", r#"{"n":1,"ratio":10000000000000001.0}"#).unwrap()
    );
}

/// Any other argument value, and another command name with the same normalized arguments,
/// is another identity; `1.0` for an integer field is refused.
#[test]
fn identity_covers_name_and_values() {
    let (_store, r) = identity_registry();
    let base = r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#;
    let id = identity(&r, "test_counter", base).unwrap();
    for other in [
        r#"{"n":2,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"b","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.25,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x","y"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"w"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"K":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":2}]}"#,
    ] {
        assert_ne!(identity(&r, "test_counter", other).unwrap(), id, "{other}");
    }
    // The same normalized arguments under another name.
    let handler = r.handler("test_counter").unwrap();
    let raw: Box<RawValue> = serde_json::from_str(base).unwrap();
    let normalized = handler.prepare(&raw).unwrap().normalized;
    assert_eq!(request_identity("test_counter", &normalized), id);
    assert_ne!(request_identity("test_completed", &normalized), id);
    // Number spellings: an integer field refuses `1.0`.
    assert_eq!(
        identity(&r, "test_counter", r#"{"n":1.0}"#),
        Err(ApiError::bad_args())
    );
}
