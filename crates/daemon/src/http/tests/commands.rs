//! The command runner with fault points (design §2.8; PR7-CL-8's command cases, CL-18, CL-19,
//! CL-20, CL-23). Over HTTP where the case allows it; through `commands::run` directly
//! where a test needs an exact deadline or the reply's cutoff gate (which the guard's
//! credential read would otherwise take).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use chaldea_core::auth::TokenKind;
use chaldea_core::command::{ReceiptStatus, request_identity};
use chaldea_core::ids::CommandId;
use proptest::prelude::*;
use serde_json::value::RawValue;

use super::eventually;
use super::support::commands::{CounterArgs, Probe, counter, slow_body};
use super::support::http::{
    DEADLINE, TestServer, connect, host_events_of, parse, reached, reached_sync, read_to_end,
    settled, spawn_send, write,
};
use super::support::soon;
use super::{TokioClock, shared};
use crate::http::auth::sha256;
use crate::http::commands::{Admitted, prepare_args, run};
use crate::http::{
    ApiError, CommandRegistry, MonotonicClock, RequestStart, Shared, StoreFatal, WriteFailure,
};
use crate::store::test_faults::CommitFault;
use crate::store::tests::support::waits;
use crate::store::{SubmitError, WriteError};

fn cid(n: u128) -> CommandId {
    CommandId::from_u128(0xC0DE_1000 + n)
}

fn code(s: &str) -> Option<String> {
    Some(s.to_owned())
}

async fn joined<T>(h: tokio::task::JoinHandle<T>) -> T {
    waits::bounded(Duration::from_secs(20), h)
        .await
        .expect("done within 20 s")
        .unwrap()
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
        start: RequestStart(s.cfg.clock.now()),
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

/// The ticket-park signal fires on an admitted write's parked ticket: a submission refused
/// before it (`QueueFull`) does not consume it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ticket_park_signal_skips_a_refused_submission() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let s = shared(&srv);
    let parked = s.faults.arm_ticket_park();
    let (writer, queued) = fill_queue(&srv).await;
    let refused = crate::http::ctx::run_write(&s, soon(), Box::new(|_tx| Ok(())), None).await;
    assert!(
        matches!(refused, Err(WriteFailure::Submit(SubmitError::QueueFull))),
        "{refused:?}"
    );
    writer.send(()).unwrap();
    for t in queued {
        settled(t).await.unwrap();
    }
    let writer = srv.hold_writer().await;
    let s2 = s.clone();
    let admitted = tokio::spawn(async move {
        crate::http::ctx::run_write(&s2, soon(), Box::new(|_tx| Ok(())), None).await
    });
    waits::bounded(Duration::from_secs(10), parked)
        .await
        .expect("parked in time")
        .expect("the signal of the admitted ticket");
    writer.send(()).unwrap();
    assert!(joined(admitted).await.is_ok());
    srv.stop().await;
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
    waits::pause_until(tokio::time::Instant::from_std(
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

/// A clock on real time that, once armed, gives the next reader a chosen reading and holds
/// it right after; the reached receiver gets that reading.
#[derive(Default)]
struct HeldClock {
    hold: std::sync::Mutex<Option<ClockHold>>,
}

/// The reading to give, where to report it, and the release to wait for.
type ClockHold = (
    Instant,
    std::sync::mpsc::Sender<Instant>,
    std::sync::mpsc::Receiver<()>,
);

impl HeldClock {
    /// The next reading is `at`; the returned receiver gets it when it is taken, and the
    /// reader goes on once the returned sender is used or dropped (at most 20 s).
    fn arm(
        &self,
        at: Instant,
    ) -> (
        std::sync::mpsc::Receiver<Instant>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.hold.lock().unwrap() = Some((at, reached_tx, release_rx));
        (reached_rx, release_tx)
    }
}

impl MonotonicClock for HeldClock {
    fn now(&self) -> Instant {
        let held = self.hold.lock().unwrap().take();
        let Some((at, reached, release)) = held else {
            return Instant::now();
        };
        let _ = reached.send(at);
        let _ = release.recv_timeout(Duration::from_secs(20));
        at
    }

    fn real(&self, at: Instant) -> Instant {
        at
    }
}

/// The waiter's first reading of the clock in its loop is the deadline (before the reply
/// time, whenever the waiter takes it) and is held there while the intent job, behind the
/// writer past `deadline + write_reply_grace`, is answered `Expired` and the runner
/// publishes it. Released, the waiter must not send that reply with the early reading: 503
/// `outcome_unknown`, not 408. (With the clock read before the progress, the held reading
/// would pair with the late `Done(Expired)`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn progress_taken_after_the_grace_is_outcome_unknown() {
    let clock = Arc::new(HeldClock::default());
    let c = clock.clone();
    let (srv, probe) = TestServer::with_commands(move |cfg| {
        cfg.write_reply_grace = Duration::from_millis(100);
        cfg.clock = c;
    })
    .await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let base = srv.server.tasks();
    let writer = srv.hold_writer().await;
    let (gate_reached, gate_release) = s.faults.arm_progress_gate();
    let before = s.faults.submits();
    let deadline = s.cfg.clock.now() + Duration::from_millis(300);
    let reply_by = deadline + Duration::from_millis(100);
    let cmd = admitted(&s, "test_counter", &owner, cid(1), r#"{"n":1}"#);
    let w = s.clone();
    let waiter = tokio::spawn(async move { run(&w, deadline, cmd).await });
    reached(gate_reached).await;
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    // The waiter's next reading is the one its loop takes; it is the deadline, held.
    let (clock_reached, clock_release) = clock.arm(deadline);
    gate_release.send(()).unwrap();
    let sample =
        tokio::task::spawn_blocking(move || clock_reached.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the waiter read the clock within 10 s");
    assert!(
        sample < reply_by,
        "the held reading is before the reply time"
    );
    // Lower bounds: the reply time and the job's `not_after` have passed.
    waits::pause_until(tokio::time::Instant::from_std(
        deadline + Duration::from_millis(150),
    ))
    .await;
    writer.send(()).unwrap();
    // The runner published `Done(Expired)` and ended.
    eventually(Duration::from_secs(10), "the runner ends", || {
        srv.server.tasks() == base
    })
    .await;
    clock_release.send(()).unwrap();
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
    waits::pause_until(tokio::time::Instant::from_std(
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
    waits::pause_until(budget_end - Duration::from_millis(2)).await;
    let before_end = told();
    waits::pause_until(budget_end + Duration::from_millis(2)).await;
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
        waits::pause(Duration::from_secs(1)).await;
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
            waits::bounded(Duration::from_secs(10), t)
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

/// The pauses between completion attempts, as scheduled: 10 ms, doubling to 1 s, and the
/// 66th cut to end exactly at the budget's end (60 s after the first attempt), after which
/// no attempt starts. A tokio timer wakes on the millisecond tick at or after its
/// deadline, so an attempt may start up to 1 ms after its scheduled wake; nothing else is
/// tolerated.
#[tokio::test]
async fn completion_backoff_schedule() {
    let x = exhaust_the_budget().await;
    let s = shared(&x.srv);
    let attempts = s.faults.completion_attempts();
    let pauses = s.faults.completion_pauses();
    let budget_end = x.first + Duration::from_secs(60);
    let mut want: Vec<Duration> = [10, 20, 40, 80, 160, 320, 640]
        .into_iter()
        .map(Duration::from_millis)
        .collect();
    want.extend([Duration::from_secs(1); 58]);
    assert_eq!(attempts.len(), 66);
    assert_eq!(pauses.len(), 66);
    let scheduled: Vec<Duration> = pauses.iter().map(|(from, wake)| *wake - *from).collect();
    assert_eq!(scheduled[..65], want[..], "{scheduled:?}");
    let (last_from, last_wake) = pauses[65];
    assert_eq!(
        last_wake, budget_end,
        "the last pause ends at the budget's end"
    );
    assert!(last_wake - last_from < Duration::from_secs(1));
    for (i, (from, wake)) in pauses.iter().enumerate() {
        assert_eq!(*from, attempts[i], "pause {i} is scheduled at its attempt");
        if let Some(next) = attempts.get(i + 1) {
            let late = next.checked_duration_since(*wake);
            assert!(
                late.is_some_and(|l| l <= Duration::from_millis(1)),
                "attempt {} started {late:?} after its wake ({:?} before it if None)",
                i + 1,
                wake.checked_duration_since(*next)
            );
        }
    }
    assert_eq!(x.reported, (false, true), "no attempt after the last pause");
    x.stop().await;
}

/// Each completion attempt's `not_after`, as `run_write` received it, is 5 s after its
/// admission, cut at the budget's end: the attempts of the last 5 s get the budget's end.
#[tokio::test]
async fn completion_attempt_not_after() {
    let x = exhaust_the_budget().await;
    let s = shared(&x.srv);
    let budget_end = x.first + Duration::from_secs(60);
    let admitted_at = s.faults.completion_attempts();
    // The completion attempts are the last jobs `run_write` submitted.
    let submitted = s.faults.submitted_not_afters();
    let passed = &submitted[submitted.len() - admitted_at.len()..];
    let attempts: Vec<(Instant, Instant)> = admitted_at
        .iter()
        .copied()
        .zip(passed.iter().copied())
        .collect();
    let mut cut = 0;
    for (at, not_after) in &attempts {
        if *at + Duration::from_secs(5) < budget_end {
            assert_eq!(*not_after, *at + Duration::from_secs(5));
        } else {
            assert_eq!(*not_after, budget_end);
            cut += 1;
        }
    }
    assert!(
        cut > 0 && cut < attempts.len(),
        "{cut} of {} cut",
        attempts.len()
    );
    x.stop().await;
}

/// A completion attempt admitted before the 60 s budget ends and held in the writer past
/// it, on the server's clock and on tokio's timers alike (paused time slept through to 61 s
/// after the first attempt, so every timer due by then has fired and been handled), is
/// awaited: nothing is reported while it is held; released, it commits, and the result
/// replays.
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
    waits::pause_until(tokio::time::Instant::from_std(
        first + Duration::from_secs(61),
    ))
    .await;
    assert!(s.cfg.clock.now() > first + Duration::from_secs(60));
    assert!(srv.sink.0.lock().unwrap().is_empty(), "reported while held");
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );
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
/// short deadline (set to [`DEADLINE`], 2 s), measured from the connection's first head byte: the first byte is
/// read, the server's clock moves 20 s on, and the rest of the head and all but the last
/// body byte follow. The body completed then runs the command; with the clock moved 31 s
/// on first, it is 408 with nothing submitted and no receipt (measured from the head's
/// end, it would have run).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_budget_is_its_own() {
    let (srv, _probe) = TestServer::with_commands(|c| c.short_deadline = DEADLINE).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    for (n, skew, want) in [
        (1, 20, (200, None)),
        (2, 31, (408, code("request_timeout"))),
    ] {
        let bytes = srv
            .command("test_long_deadline", &owner, cid(n), "{}")
            .bytes();
        let (first, rest) = bytes.split_at(1);
        let (rest, last) = rest.split_at(rest.len() - 1);
        let (firsts, polls, submits) =
            (s.faults.first_bytes(), s.faults.polls(), s.faults.submits());
        let mut c = connect(srv.port).await;
        write(&mut c, first).await;
        eventually(Duration::from_secs(10), "the first byte is read", || {
            s.faults.first_bytes() > firsts
        })
        .await;
        srv.clock.skew(Duration::from_secs(20));
        write(&mut c, rest).await;
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
    let (srv, probe) = TestServer::with_commands(|c| c.short_deadline = DEADLINE).await;
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
    let (srv, probe) = TestServer::with_commands(|c| c.short_deadline = DEADLINE).await;
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
    let (srv, probe) = TestServer::with_commands(|c| c.short_deadline = DEADLINE).await;
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

/// The identity of `args` under `test_counter`, as the dispatcher computes it.
fn identity(args: &str) -> Result<[u8; 32], ApiError> {
    let raw: Box<RawValue> = serde_json::from_str(args).unwrap();
    prepare_args(counter().as_ref(), "test_counter", &raw).map(|(_, id)| id)
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
        let value = serde_json::to_value(&args).unwrap();
        let (mut a, mut b) = (String::new(), String::new());
        spell(&value, &mut seed_a.clone(), &mut a);
        spell(&value, &mut seed_b.clone(), &mut b);
        let ia = identity(&a).unwrap();
        let ib = identity(&b).unwrap();
        prop_assert_eq!(ia, ib, "{} vs {}", a, b);
    }
}

/// Decimal spellings of one value share one identity, also where the two parse paths of
/// `serde_json` round differently without its `float_roundtrip` feature.
#[test]
fn decimal_respelling_keeps_identity() {
    assert_eq!(
        identity(r#"{"n":1,"ratio":10000000000000001}"#).unwrap(),
        identity(r#"{"n":1,"ratio":10000000000000001.0}"#).unwrap()
    );
}

/// Any other argument value, and another command name with the same normalized arguments,
/// is another identity; `1.0` for an integer field is refused.
#[test]
fn identity_covers_name_and_values() {
    let base = r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#;
    let id = identity(base).unwrap();
    for other in [
        r#"{"n":2,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"b","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.25,"tags":["x"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x","y"],"env":{"k":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"w"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"K":"v"},"layers":[{"a":1}]}"#,
        r#"{"n":1,"label":"a","ratio":0.5,"tags":["x"],"env":{"k":"v"},"layers":[{"a":2}]}"#,
    ] {
        assert_ne!(identity(other).unwrap(), id, "{other}");
    }
    // The same normalized arguments under another name.
    let raw: Box<RawValue> = serde_json::from_str(base).unwrap();
    let normalized = counter().prepare(&raw).unwrap().normalized;
    assert_eq!(request_identity("test_counter", &normalized), id);
    assert_ne!(request_identity("test_completed", &normalized), id);
    // Number spellings: an integer field refuses `1.0`.
    assert_eq!(identity(r#"{"n":1.0}"#), Err(ApiError::bad_args()));
}

/// A handler whose `revokes()` is true nudges the sockets once its intent committed: no
/// nudge while the intent job is held, one after, and the revocation is visible to a read
/// when the nudge arrives. A handler whose `revokes()` is false never nudges.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn revoking_intent_nudges() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    for (n, name, nudges) in [(1, "test_revoke", 1), (2, "test_revoke_quiet", 0)] {
        let victim = srv.token(TokenKind::Owner, None, None).await;
        let id = srv.token_row(&victim).unwrap().token_id;
        let mut rx = s.revocations.subscribe();
        let before = *rx.borrow_and_update();
        // Each nudge records whether the revocation was visible to a read when it arrived.
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (store, sha, record) = (srv.store.store().clone(), sha256(&victim), seen.clone());
        let watcher = tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let row = store
                    .read(soon(), move |tx| tx.token_by_hash(&sha))
                    .unwrap();
                record
                    .lock()
                    .unwrap()
                    .push(row.is_some_and(|r| r.revoked_at.is_some()));
            }
        });
        let (intent_reached, intent_release) = probe.hold_intent();
        let reply = spawn_send(
            &srv,
            &srv.command(name, &owner, cid(n), &format!(r#"{{"token":"{id}"}}"#)),
        );
        reached_sync(intent_reached).await;
        assert_eq!(
            *s.revocations.borrow(),
            before,
            "{name}: no nudge before the commit"
        );
        intent_release.send(()).unwrap();
        assert_eq!(joined(reply).await.status, 200, "{name}");
        // `after_ok` runs before the runner publishes the reply.
        assert_eq!(*s.revocations.borrow(), before + nudges, "{name}");
        if nudges == 1 {
            eventually(Duration::from_secs(10), "the nudge is read", || {
                !seen.lock().unwrap().is_empty()
            })
            .await;
            assert_eq!(*seen.lock().unwrap(), [true], "{name}");
        }
        watcher.abort();
        assert!(
            srv.token_row(&victim).unwrap().revoked_at.is_some(),
            "{name}"
        );
    }
    srv.stop().await;
}

/// A revoking intent whose commit outcome is unknown: the revocation may be visible, no
/// nudge runs (the nudge is guaranteed for acknowledged commits only), and `CommitUnknown`
/// is delivered to the sink. This checks that delivery; that the daemon's sink then stops
/// the daemon is the lifecycle gate's (`lifecycle::tests::gate`).
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn revoking_commit_unknown_is_fatal() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let victim = srv.token(TokenKind::Owner, None, None).await;
    let id = srv.token_row(&victim).unwrap().token_id;
    let before = *s.revocations.borrow();
    let (intent_reached, intent_release) = probe.hold_intent();
    let reply = spawn_send(
        &srv,
        &srv.command(
            "test_revoke",
            &owner,
            cid(1),
            &format!(r#"{{"token":"{id}"}}"#),
        ),
    );
    reached_sync(intent_reached).await;
    *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    intent_release.send(()).unwrap();
    assert_eq!(
        joined(reply).await.outcome(),
        (503, code("outcome_unknown"))
    );
    assert_eq!(
        *s.revocations.borrow(),
        before,
        "no nudge without an acknowledged commit"
    );
    assert!(srv.token_row(&victim).unwrap().revoked_at.is_some());
    assert_eq!(*srv.sink.0.lock().unwrap(), [StoreFatal::CommitUnknown]);
    srv.stop().await;
}

/// How an effect ends decides the reply and the report: `Unrecorded` answers 503
/// `outcome_unknown` at once with no completion attempt and reports `ReceiptUncompleted`;
/// after the request's reply time it reports once the effect ends; `ShuttingDown` answers
/// the same with no report. The receipt stays `in_progress` in each. The cases answered at
/// once run on a stopped clock, so their reply time never passes; the case after the reply
/// time runs on its own server with the short budget ([`DEADLINE`]).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effect_outcomes() {
    let cmd = |srv: &TestServer, owner: &str, n, outcome: &str| {
        srv.command(
            "test_effect_outcome",
            owner,
            cid(n),
            &format!(r#"{{"outcome":"{outcome}"}}"#),
        )
    };

    // Unrecorded while the request waits.
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    srv.clock.stop();
    let before = s.faults.submits();
    let r = srv.send(&cmd(&srv, &owner, 1, "unrecorded")).await;
    assert_eq!(r.outcome(), (503, code("outcome_unknown")));
    assert_eq!(
        s.faults.submits(),
        before + 1,
        "the intent only: no completion"
    );
    assert_eq!(
        std::mem::take(&mut *srv.sink.0.lock().unwrap()),
        [StoreFatal::ReceiptUncompleted { command_id: cid(1) }]
    );
    assert_eq!(
        srv.receipt(cid(1)).unwrap().status,
        ReceiptStatus::InProgress
    );

    // ShuttingDown.
    let before = s.faults.submits();
    let r = srv.send(&cmd(&srv, &owner, 3, "shutting_down")).await;
    assert_eq!(r.outcome(), (503, code("outcome_unknown")));
    assert_eq!(s.faults.submits(), before + 1);
    assert!(srv.sink.0.lock().unwrap().is_empty());
    assert_eq!(
        srv.receipt(cid(3)).unwrap().status,
        ReceiptStatus::InProgress
    );
    assert_eq!(probe.effects(), 2);
    srv.stop().await;

    // Unrecorded after the request's reply time.
    let (srv, probe) = TestServer::with_commands(|c| c.short_deadline = DEADLINE).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let (effect_reached, release) = probe.hold_effect();
    let first = spawn_send(&srv, &cmd(&srv, &owner, 2, "unrecorded"));
    reached(effect_reached).await;
    assert_eq!(joined(first).await.outcome(), (409, code("in_progress")));
    assert!(srv.sink.0.lock().unwrap().is_empty());
    release.send(()).unwrap();
    let sink = srv.sink.clone();
    eventually(Duration::from_secs(10), "the sink is told", move || {
        !sink.0.lock().unwrap().is_empty()
    })
    .await;
    assert_eq!(
        std::mem::take(&mut *srv.sink.0.lock().unwrap()),
        [StoreFatal::ReceiptUncompleted { command_id: cid(2) }]
    );
    assert_eq!(
        srv.receipt(cid(2)).unwrap().status,
        ReceiptStatus::InProgress
    );
    assert_eq!(probe.effects(), 1);
    srv.stop().await;
}

/// The nudge a component takes from the server bumps the sockets' revocation counter, and
/// holds the server weakly: once the server is gone it does nothing.
#[tokio::test]
async fn revocation_nudge_from_the_server() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let s = shared(&srv);
    let nudge = srv.server.revocation_nudge();
    let before = *s.revocations.borrow();
    nudge.clone().nudge();
    assert_eq!(*s.revocations.borrow(), before + 1);
    let weak = Arc::downgrade(&s);
    drop(s);
    srv.stop().await;
    assert!(weak.upgrade().is_none(), "the server is gone");
    nudge.nudge();
}

/// The launch commands' codes (supervisor §2.2) are handler replies at one status each: a
/// result with the code at that status is stored and answered as it is; at any other status
/// it is 500 `internal`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_codes_at_their_statuses() {
    let (srv, _probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let codes = [
        ("thread_closed", 409),
        ("launch_changed", 409),
        ("launch_live", 409),
        ("death_unconfirmed", 409),
        ("worktree_unavailable", 409),
        ("no_session", 409),
        ("prompt_rejected", 400),
        ("prompt_too_large", 400),
        ("launch_busy", 504),
    ];
    let mut n = 0;
    for (c, status) in codes {
        for (at, want) in [
            (status, (status, code(c))),
            (
                if status == 409 { 400 } else { 409 },
                (500, code("internal")),
            ),
        ] {
            n += 1;
            let args = format!(r#"{{"text":"x","code":"{c}","status":{at}}}"#);
            // The time taken and the sink tell a missed deadline from an early 503.
            let started = std::time::Instant::now();
            let r = srv
                .send(&srv.command("test_completed", &owner, cid(n), &args))
                .await;
            let took = started.elapsed();
            assert_eq!(
                r.outcome(),
                want,
                "{c} at {at}: answered in {took:?} (budget {:?}); sink {:?}",
                shared(&srv).cfg.short_deadline + shared(&srv).cfg.write_reply_grace,
                srv.sink.0.lock().unwrap(),
            );
        }
    }
    srv.stop().await;
}

/// Through the HTTP path: `dispatch` hands the intent the request's `RequestStart` (its first
/// head byte), not a later reading. The server's clock moves 1 s, within the budget,
/// while the guard's token read is held, after the request was received.
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn dispatch_copies_request_start() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    srv.clock.stop();
    let origin = srv.clock.now();
    let (reached_tx, reached) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    shared(&srv).faults.arm_before_read(0, move || {
        let _ = reached_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(10));
    });
    let reply = spawn_send(&srv, &srv.command("test_ctx", &owner, cid(1), "{}"));
    reached_sync(reached).await;
    srv.clock.advance(Duration::from_secs(1));
    release.send(()).unwrap();
    assert_eq!(joined(reply).await.status, 200);
    let seen = probe.ctx_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, origin, "the first head byte's reading");
    srv.stop().await;
}

/// The intent sees the request's `RequestStart` as `request_start`, and as `job_start` the
/// server's clock read in its job: a clock moved while the job waits behind the writer is
/// seen.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intent_sees_request_and_job_start() {
    let (srv, probe) = TestServer::with_commands(|_| {}).await;
    let owner = srv.token(TokenKind::Owner, None, None).await;
    let s = shared(&srv);
    let origin = s.cfg.clock.now() - Duration::from_secs(1);
    let mut cmd = admitted(&s, "test_ctx", &owner, cid(1), "{}");
    cmd.start = RequestStart(origin);
    let writer = srv.hold_writer().await;
    let before = s.faults.submits();
    let w = s.clone();
    let reply = tokio::spawn(async move { run(&w, origin + Duration::from_secs(60), cmd).await });
    eventually(Duration::from_secs(10), "the intent job is queued", || {
        s.faults.submits() == before + 1
    })
    .await;
    let moved = s.cfg.clock.now() + Duration::from_secs(2);
    srv.clock.skew(Duration::from_secs(2));
    writer.send(()).unwrap();
    assert_eq!(joined(reply).await.unwrap().status(), 200);
    let seen = probe.ctx_seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, origin);
    assert!(seen[0].1 >= moved, "the job's reading, not the request's");
    srv.clock.skew(Duration::ZERO);
    srv.stop().await;
}
