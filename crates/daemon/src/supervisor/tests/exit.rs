//! A natural exit (design §2.3 "Natural exit"; PR10a1-CL-13).

use chaldea_core::payload::ExitReason;
use chaldea_pty_runtime::ChildExit;

use super::Rig;

/// PR10a1-CL-13 `natural_exit`: the leader's own exit is recorded `exit` with its code and
/// signal, after the exit op's close proved the tree dead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn natural_exit() {
    let rig = Rig::new().await;
    for (code, signal) in [(Some(3), None), (None, Some(15))] {
        let (t, l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
        child.exit(ChildExit { code, signal });
        rig.until("the exit is recorded", || {
            rig.launch(l).exit_reason.is_some()
        })
        .await;
        let row = rig.launch(l);
        assert_eq!(row.exit_reason, Some(ExitReason::Exit));
        assert_eq!((row.exit_code, row.exit_signal), (code, signal));
        assert!(row.death_confirmed);
        let exits: Vec<_> = rig
            .events(t)
            .into_iter()
            .filter(|(k, _)| k == "launch_exited")
            .collect();
        assert_eq!(exits.len(), 1);
        assert_eq!(exits[0].1["reason"], "exit");
    }
    rig.srv.stop().await;
}

/// An exit op that panics after it took the session (a fault at `ExitTookSession`) records
/// no exit, and reports `LaunchUnrecorded` for the launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwind_after_taking_the_session() {
    use crate::http::StoreFatal;
    use crate::supervisor::Pause;
    let rig = Rig::new().await;
    let (_t, l) = rig.spawn_codex(None).await;
    let fault = rig.pauses.arm_panic(Pause::ExitTookSession);
    rig.pty.take_child().unwrap().exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    fault.wait();
    fault.go();
    rig.until("LaunchUnrecorded reported", || {
        rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    })
    .await;
    assert_eq!(rig.launch(l).exit_reason, None);
    rig.srv.stop().await;
}

/// An exit op whose exit job committed, then an exit step's `after_commit` panicked: the
/// exit is recorded, and no `LaunchUnrecorded` is reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwind_after_the_commit() {
    use crate::http::StoreFatal;
    let (tx, rx) = std::sync::mpsc::channel();
    let step = std::sync::Arc::new(PanicAfterCommit(std::sync::Mutex::new(Some(tx))));
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        c.exit_steps.push(step);
    })
    .await;
    let (_t, l) = rig.spawn_codex(None).await;
    rig.pty.take_child().unwrap().exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    tokio::task::spawn_blocking(move || rx.recv_timeout(std::time::Duration::from_secs(20)))
        .await
        .unwrap()
        .expect("the after_commit panic unwound");
    assert_eq!(rig.launch(l).exit_reason, Some(ExitReason::Exit));
    assert!(
        !rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    );
    rig.srv.stop().await;
}

/// Panics in `after_commit` with an [`super::Unwound`] payload.
struct PanicAfterCommit(std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>);

impl crate::supervisor::exit::ExitStep for PanicAfterCommit {
    fn in_exit_job(
        &self,
        _tx: &mut crate::store::WriteTx<'_>,
        _exit: &crate::supervisor::exit::ExitFacts,
    ) -> Result<(), crate::store::StoreError> {
        Ok(())
    }

    fn after_commit(&self, _exit: &crate::supervisor::exit::ExitFacts) {
        if let Some(tx) = self.0.lock().unwrap().take() {
            std::panic::panic_any(super::Unwound(tx));
        }
    }
}

/// An exit op that panics after it took the session once admission closed: no
/// `LaunchUnrecorded`, as every op shutdown ends goes unreported (PR10a1-CL-22).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwind_after_admission_closed() {
    use crate::http::StoreFatal;
    use crate::supervisor::Pause;
    let rig = Rig::new().await;
    let (_t, l) = rig.spawn_codex(None).await;
    let fault = rig.pauses.arm_panic(Pause::ExitTookSession);
    rig.pty.take_child().unwrap().exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    let fault = tokio::task::spawn_blocking(move || {
        fault.wait();
        fault
    })
    .await
    .unwrap();
    rig.sup.tracker.close_admission();
    tokio::task::spawn_blocking(move || fault.go_and_unwind())
        .await
        .unwrap();
    assert!(
        !rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    );
    assert_eq!(rig.launch(l).exit_reason, None);
    rig.srv.stop().await;
}

/// One buffered hook delivery for `l`, written as PR 9's intake writes one for a launch
/// with no bound session.
pub(super) fn buffer_one(store: &super::support::TestStore, l: chaldea_core::ids::LaunchId) {
    use crate::store::{BufferedHook, HookReply, NewHookDelivery, NewRouting};
    let meta = serde_json::json!({
        "v": 1, "thread_header": null, "agent_header": "codex", "emitter_version": "1",
        "attempt": null, "chain": [], "chain_end": "x"
    })
    .to_string();
    let d = NewHookDelivery {
        launch_id: l,
        emit_id: uuid::Uuid::new_v4(),
        receipt_ordinal: 1,
        first_reply: HookReply {
            http_status: 200,
            body: "{}".to_owned(),
        },
        routing: NewRouting::Buffered(BufferedHook {
            body: br#"{"hook_event_name":"UserPromptSubmit"}"#.to_vec(),
            meta,
        }),
    };
    let job: crate::http::Job<()> = Box::new(move |tx| {
        tx.hook_delivery_record(d)?;
        Ok(())
    });
    tokio::task::block_in_place(|| store.run(job).unwrap());
}

pub(super) fn buffered(store: &super::support::TestStore) -> Vec<chaldea_core::ids::LaunchId> {
    fn launches(
        r: &crate::store::ReadTx<'_>,
    ) -> Result<Vec<chaldea_core::ids::LaunchId>, crate::store::StoreError> {
        r.buffered_launches()
    }
    store.read(launches)
}

/// PR10a1-CL-15 `buffer_resolved`: the exit job resolves the launch's buffered deliveries
/// in its own commit.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffer_resolved() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    buffer_one(&rig.srv.store, l);
    assert_eq!(buffered(&rig.srv.store), vec![l]);
    let r = rig
        .cmd("stop", &serde_json::json!({ "thread_id": t }))
        .await;
    assert_eq!(r.status, 200);
    assert!(buffered(&rig.srv.store).is_empty());
    assert!(
        rig.kinds(t).iter().any(|k| k == "raw_unknown"),
        "{:?}",
        rig.kinds(t)
    );
    rig.srv.stop().await;
}

/// PR10a1-CL-22 `unrecorded_exit_job`: a natural exit whose exit job cannot be submitted
/// (the writer retired by a job outside the supervisor) reports `LaunchUnrecorded`; the
/// row stays open, and after a restart the reconcilers record it `daemon_restart`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unrecorded_exit_job() {
    use crate::http::StoreFatal;
    use crate::store::test_faults::CommitFault;
    let rig = Rig::new().await;
    let (_t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let took = rig.pauses.arm(crate::supervisor::Pause::ExitTookSession);
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    took.wait();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let job: crate::http::Job<()> = Box::new(|_| Ok(()));
    assert!(tokio::task::block_in_place(|| rig.srv.store.run(job)).is_err());
    took.go();
    rig.until("LaunchUnrecorded reported", || {
        rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    })
    .await;
    assert_eq!(rig.launch(l).exit_reason, None, "the row stays open");
    drop(rig.sup);
    let (_, mut store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    store.reopen();
    super::restart::restart_all(&store);
    let row = store.read(move |r| r.launch(l)).unwrap();
    assert_eq!(row.exit_reason, Some(ExitReason::DaemonRestart));
}

/// PR10a1-CL-22: an exit op whose last lock round starts with the clock past its start +
/// 60 s: the lock, free by then, is not taken; `LaunchUnrecorded` is reported and the row
/// stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn last_round_ends_at_the_total() {
    use crate::http::StoreFatal;
    use crate::supervisor::{EXIT_TOTAL, Pause};
    let rig = Rig::new().await;
    rig.clock.resume();
    let (_t, l) = rig.spawn_codex(None).await;
    let held = rig.sup.entry(l).unwrap().ops.clone().lock_owned().await;
    let round = rig.pauses.arm(Pause::ExitRound);
    rig.pty.take_child().unwrap().exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    let round = tokio::task::spawn_blocking(move || {
        round.wait();
        round
    })
    .await
    .unwrap();
    rig.clock.skew(EXIT_TOTAL);
    drop(held);
    round.go();
    rig.until("LaunchUnrecorded reported", || {
        rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    })
    .await;
    assert_eq!(rig.launch(l).exit_reason, None, "the row stays open");
    rig.srv.stop().await;
}

/// Two units on launch `l`'s session, neither permitted: `a`, whose recheck holds the
/// writer until `gate` is sent, and `b`, queued behind it. A seal settles both `Closed`.
pub(super) struct Queued {
    pub gate: std::sync::mpsc::Sender<()>,
    pub a: chaldea_pty_runtime::CompletionReceiver,
    pub b: chaldea_pty_runtime::CompletionReceiver,
    pub b_origin: chaldea_pty_runtime::WriteOrigin,
}

pub(super) async fn queue_behind_recheck(rig: &Rig, l: chaldea_core::ids::LaunchId) -> Queued {
    use chaldea_pty_runtime::{Guard, WriteOrigin, WriteUnit, completion_channel};
    use std::sync::mpsc;
    use std::time::Duration;
    let entry = rig.sup.entry(l).unwrap();
    let (gate, gate_rx) = mpsc::channel::<()>();
    let (entered_tx, entered) = mpsc::channel::<()>();
    let (a_done, a) = completion_channel();
    let (b_done, b) = completion_channel();
    let b_origin = WriteOrigin::Input {
        input_id: chaldea_core::ids::InputId(uuid::Uuid::new_v4()),
    };
    let far = std::time::Instant::now() + Duration::from_secs(60);
    let origin = b_origin.clone();
    entry
        .with_session(&rig.sup, rig.sup.now() + Duration::from_secs(5), move |s| {
            let mut g = Guard::until(far);
            g.recheck = Some(Box::new(move |_| {
                let _ = entered_tx.send(());
                let _ = gate_rx.recv_timeout(Duration::from_secs(60));
                Ok(())
            }));
            s.submit(WriteUnit::bytes(WriteOrigin::Key, b"a".to_vec()), g, a_done)
                .unwrap();
            s.submit(
                WriteUnit::bytes(origin, b"b".to_vec()),
                Guard::until(far),
                b_done,
            )
            .unwrap();
        })
        .await
        .unwrap();
    entered
        .recv_timeout(Duration::from_secs(20))
        .expect("the writer is in `a`'s recheck");
    Queued {
        gate,
        a,
        b,
        b_origin,
    }
}

impl Queued {
    /// Releases the writer once `b` settled (the seal settles it), on a thread: `b`'s record.
    pub(super) fn release_after_seal(
        self,
    ) -> std::thread::JoinHandle<chaldea_pty_runtime::WriteRecord> {
        std::thread::spawn(move || {
            let r = self
                .b
                .recv_deadline(std::time::Instant::now() + std::time::Duration::from_secs(20))
                .expect("the seal settles `b`");
            let _ = self.gate.send(());
            drop(self.a);
            r
        })
    }
}

/// A stand-in for 10b's exit steps: the `ExitFacts.sealed` of every exit job, by launch.
#[derive(Default)]
pub(super) struct SealedSeen(
    pub  std::sync::Mutex<
        Vec<(
            chaldea_core::ids::LaunchId,
            Vec<chaldea_pty_runtime::WriteRecord>,
        )>,
    >,
);

impl crate::supervisor::exit::ExitStep for SealedSeen {
    fn in_exit_job(
        &self,
        _tx: &mut crate::store::WriteTx<'_>,
        exit: &crate::supervisor::exit::ExitFacts,
    ) -> Result<(), crate::store::StoreError> {
        self.0
            .lock()
            .unwrap()
            .push((exit.launch_id, exit.sealed.clone()));
        Ok(())
    }
}

impl SealedSeen {
    fn of(&self, l: chaldea_core::ids::LaunchId) -> Vec<chaldea_pty_runtime::WriteRecord> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(x, _)| *x == l)
            .flat_map(|(_, s)| s.clone())
            .collect()
    }
}

/// PR10a1-CL-29 `sealed_on_every_path`: a unit queued and not permitted, then a natural
/// exit, a stop whose `StopRequested` job rolls back (its exit job applies it), and a
/// shutdown close: each exit job's `ExitFacts.sealed` holds the unit's record, settled
/// `Closed`. A second seal of one session settles nothing and leaves `settled` as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_on_every_path() {
    use crate::store::test_faults::CommitFault;
    use chaldea_pty_runtime::{Refusal, WriteOutcome};
    let seen = std::sync::Arc::new(SealedSeen::default());
    let step = seen.clone();
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        c.exit_steps.push(step);
    })
    .await;
    let check = |l, r: &chaldea_pty_runtime::WriteRecord, origin, path: &str| {
        assert_eq!(
            r.outcome,
            WriteOutcome::NotStarted(Refusal::Closed),
            "{path}"
        );
        assert_eq!(r.origin, origin, "{path}");
        let sealed = seen.of(l);
        assert!(sealed.contains(r), "{path}: {sealed:?}");
    };
    // A natural exit.
    let (_t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let q = queue_behind_recheck(&rig, l).await;
    let origin = q.b_origin.clone();
    let b = q.release_after_seal();
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit is recorded", || {
        rig.launch(l).exit_reason.is_some()
    })
    .await;
    check(l, &b.join().unwrap(), origin, "natural exit");
    // A stop whose `StopRequested` job rolls back.
    let (t, l) = rig.spawn_codex(None).await;
    let q = queue_behind_recheck(&rig, l).await;
    let origin = q.b_origin.clone();
    let b = q.release_after_seal();
    let held = rig.pauses.arm(crate::supervisor::Pause::InStopRequested);
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "stop",
            &rig.owner,
            super::cid(),
            &serde_json::json!({ "thread_id": t }).to_string(),
        ),
    );
    held.wait();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    held.go();
    assert_eq!(reply.await.unwrap().status, 200);
    check(l, &b.join().unwrap(), origin, "stop, rolled back");
    // A second seal.
    let (_t, l) = rig.spawn_codex(None).await;
    let q = queue_behind_recheck(&rig, l).await;
    let entry = rig.sup.entry(l).unwrap();
    {
        let guard = entry.ops.clone().lock_owned().await;
        entry.seal(&guard);
        let first = entry.settled();
        assert!(first.iter().any(|r| r.origin == q.b_origin), "{first:?}");
        assert!(guard.as_ref().unwrap().seal().is_empty());
        entry.seal(&guard);
        assert_eq!(entry.settled(), first);
    }
    let _ = q.gate.send(());
    // A shutdown close (last: it closes admission).
    let (_t, l) = rig.spawn_codex(None).await;
    let q = queue_behind_recheck(&rig, l).await;
    let origin = q.b_origin.clone();
    let b = q.release_after_seal();
    let end = rig.sup.now() + std::time::Duration::from_secs(5);
    crate::supervisor::components::shutdown(rig.sup.clone(), end).await;
    check(l, &b.join().unwrap(), origin, "shutdown close");
    rig.srv.stop().await;
}

fn launch_exits(rig: &Rig, t: chaldea_core::ids::ThreadId) -> Vec<serde_json::Value> {
    rig.events(t)
        .into_iter()
        .filter(|(k, _)| k == "launch_exited")
        .map(|(_, p)| p)
        .collect()
}

fn stop_req(rig: &Rig, t: chaldea_core::ids::ThreadId) -> super::support::http::Req {
    rig.srv.command(
        "stop",
        &rig.owner,
        super::cid(),
        &serde_json::json!({ "thread_id": t }).to_string(),
    )
}

/// PR10a1-CL-13 `sealing_op_records`: the child exits while a stop holds the launch after
/// its seal: the exit op returns before its lock wait, and the stop records the one
/// `launch_exited` (`stopped`). When the stop's exit job then fails (the writer retired),
/// the exit op still records and reports nothing; the row stays open until the reconcilers
/// record it `daemon_restart`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealing_op_records() {
    use crate::http::StoreFatal;
    use crate::store::test_faults::CommitFault;
    use crate::supervisor::Pause;
    // The stop records.
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let child = rig.pty.take_child().unwrap();
    let seal = rig.pauses.arm(Pause::StopAfterSeal);
    let start = rig.pauses.arm(Pause::ExitStart);
    let reply = super::support::http::spawn_send(&rig.srv, &stop_req(&rig, t));
    seal.wait();
    child.exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    start.wait();
    start.go();
    seal.go();
    assert_eq!(reply.await.unwrap().status, 200);
    let exits = launch_exits(&rig, t);
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0]["reason"], "stopped");
    assert!(!rig.pauses.passed(Pause::ExitBeforeLock));
    assert_eq!(rig.launch(l).exit_reason, Some(ExitReason::Stopped));
    rig.srv.stop().await;
    // The stop's exit job fails: nothing recorded or reported by the exit op.
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let start = rig.pauses.arm(Pause::ExitStart);
    let written = rig.pauses.arm(Pause::StopWritten);
    let reply = super::support::http::spawn_send(&rig.srv, &stop_req(&rig, t));
    written.wait();
    start.wait();
    start.go();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    let job: crate::http::Job<()> = Box::new(|_| Ok(()));
    assert!(tokio::task::block_in_place(|| rig.srv.store.run(job)).is_err());
    written.go();
    assert_eq!(reply.await.unwrap().status, 503);
    assert!(!rig.pauses.passed(Pause::ExitBeforeLock));
    assert!(
        !rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l }),
        "{:?}",
        rig.sink.0
    );
    assert_eq!(rig.launch(l).exit_reason, None, "the row stays open");
    drop(rig.sup);
    let (_, mut store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    store.reopen();
    super::restart::restart_all(&store);
    let row = store.read(move |r| r.launch(l)).unwrap();
    assert_eq!(row.exit_reason, Some(ExitReason::DaemonRestart));
}

/// PR10a1-CL-13 `stop_owns_exit`: the kill's exit reaches an exit op while the stop's
/// `StopRequested` job is held in the writer: the exit op returns before its lock wait, so
/// its 60 s bound never runs and it reports no `LaunchUnrecorded`; once the ticket answers,
/// the stop records one `launch_exited` (`stopped`) with its accepted proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_owns_exit() {
    use crate::http::StoreFatal;
    use crate::supervisor::Pause;
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let held = rig.pauses.arm(Pause::InStopRequested);
    let start = rig.pauses.arm(Pause::ExitStart);
    let reply = super::support::http::spawn_send(&rig.srv, &stop_req(&rig, t));
    held.wait();
    start.wait();
    start.go();
    held.go();
    let r = reply.await.unwrap();
    assert_eq!(super::json(&r)["death_confirmed"], true);
    assert!(!rig.pauses.passed(Pause::ExitBeforeLock));
    assert!(
        !rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&StoreFatal::LaunchUnrecorded { launch_id: l })
    );
    let exits = launch_exits(&rig, t);
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0]["reason"], "stopped");
    assert!(rig.launch(l).death_confirmed);
    rig.srv.stop().await;
}

/// One input's events on thread `t`: created `queued` with `launch`, then each step.
fn input_events(
    t: chaldea_core::ids::ThreadId,
    launch: Option<chaldea_core::ids::LaunchId>,
    i: chaldea_core::ids::InputId,
    steps: &[chaldea_core::input::InputInput],
) -> Vec<crate::store::NewEvent> {
    use super::support::event;
    use chaldea_core::event::TypedEvent;
    use chaldea_core::ids::{Actor, StreamId};
    use chaldea_core::input::{InputState, step};
    use chaldea_core::payload::{
        Content, InputCreated, InputEncoding, InputInitialState, InputOrigin, InputStateChanged,
    };
    let mut out = vec![event(
        StreamId::Thread(t),
        &format!("test-input:{i}"),
        TypedEvent::InputCreated(InputCreated {
            input_id: i,
            launch_id: launch,
            initial_state: InputInitialState::Queued,
            text: Content::text("hello"),
            encoding: InputEncoding::BracketedPaste,
            origin: InputOrigin::Cli,
            created_by: Actor::Human,
        }),
    )];
    let mut state = InputState::Queued;
    for (n, how) in steps.iter().enumerate() {
        let to = step(state, how).unwrap();
        let mut e = event(
            StreamId::Thread(t),
            &format!("test-input:{i}:{n}"),
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: i,
                from: state,
                to,
                reason: how.into(),
                sent_after_seq: None,
                by: None,
            }),
        );
        e.launch_id = launch;
        out.push(e);
        state = to;
    }
    out
}

/// A test exit step that cancels one `queued` input before the sweep runs.
struct CancelFirst(chaldea_core::ids::InputId);

impl crate::supervisor::exit::ExitStep for CancelFirst {
    fn in_exit_job(
        &self,
        tx: &mut crate::store::WriteTx<'_>,
        exit: &crate::supervisor::exit::ExitFacts,
    ) -> Result<(), crate::store::StoreError> {
        use chaldea_core::input::InputInput;
        let mut e = input_events(
            exit.thread_id,
            Some(exit.launch_id),
            self.0,
            &[InputInput::Cancelled],
        );
        e.remove(0);
        e[0].idempotency_key = chaldea_core::ids::IdempotencyKey(format!("step-cancel:{}", self.0));
        tx.append(e)?;
        Ok(())
    }
}

/// PR10a1-CL-14 `inputs_recovered`: a natural exit's job moves the thread's inputs still
/// `queued`, `sending`, or `sent` of the launch, and the queue of no launch, after the
/// registered exit steps: an input a test step cancelled first is left as it settled; an
/// `unconfirmed` input keeps its state and gets no event; all in the exit job, after
/// `launch_exited`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inputs_recovered() {
    use chaldea_core::ids::InputId;
    use chaldea_core::input::InputInput as I;
    let id = || InputId(uuid::Uuid::new_v4());
    let (queued, sending, sent, unassigned, unconfirmed, cancelled) =
        (id(), id(), id(), id(), id(), id());
    let step = std::sync::Arc::new(CancelFirst(cancelled));
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, move |c| {
        c.exit_steps.push(step);
    })
    .await;
    let (t, l) = rig.spawn_codex(None).await;
    let open = I::GateOpened { sent_after_seq: 1 };
    let mut events = Vec::new();
    for (i, launch, steps) in [
        (queued, Some(l), vec![]),
        (sending, Some(l), vec![open]),
        (sent, Some(l), vec![open, I::WriteCompleted]),
        (unassigned, None, vec![]),
        (unconfirmed, Some(l), vec![open, I::WriteOutcomeUnknown]),
        (cancelled, Some(l), vec![]),
    ] {
        events.extend(input_events(t, launch, i, &steps));
    }
    rig.srv.append(events).await;
    let before = rig.events(t).len();
    rig.pty.take_child().unwrap().exit(ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit recorded", || rig.launch(l).exit_reason.is_some())
        .await;
    let after: Vec<_> = rig.events(t).split_off(before);
    let moved = |i: InputId| -> Vec<(String, String)> {
        after
            .iter()
            .filter(|(k, p)| k == "input_state_changed" && p["input_id"] == i.to_string())
            .map(|(_, p)| {
                (
                    p["from"].as_str().unwrap().to_owned(),
                    p["to"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let pair = |a: &str, b: &str| vec![(a.to_owned(), b.to_owned())];
    assert_eq!(moved(queued), pair("queued", "cancelled"));
    assert_eq!(moved(sending), pair("sending", "unconfirmed"));
    assert_eq!(moved(sent), pair("sent", "unconfirmed"));
    assert_eq!(moved(unassigned), pair("queued", "cancelled"));
    assert_eq!(moved(unconfirmed), vec![], "kept");
    assert_eq!(
        moved(cancelled),
        pair("queued", "cancelled"),
        "the step's move only"
    );
    // Order: `launch_exited`, the step's move, then the sweep's.
    let at =
        |pred: &dyn Fn(&(String, serde_json::Value)) -> bool| after.iter().position(pred).unwrap();
    let exited = at(&|(k, _)| k == "launch_exited");
    let stepped = at(&|(_, p)| p["input_id"] == cancelled.to_string());
    let swept = at(&|(_, p)| p["input_id"] == queued.to_string());
    assert!(exited < stepped && stepped < swept, "{after:?}");
    rig.srv.stop().await;
}
