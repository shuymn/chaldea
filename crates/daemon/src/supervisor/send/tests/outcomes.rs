//! Write outcomes (send §2.3 steps 4–5, "After exit"): each record's move, the stall, the
//! dispatch wait, a worker held in a retry, and completions around the exit job.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, InputId};
use chaldea_core::input::InputState;
use chaldea_core::state::AttentionCause;
use chaldea_pty_runtime::fake::ProbePoint;
use chaldea_pty_runtime::{Guard, WriteOrigin, WriteUnit};
use serde_json::json;

use super::gate::set;
use super::{Live, SRig};
use crate::supervisor::send::{DISPATCH_WAIT, GATE_TIMEOUT, READ_WAIT, WRITE_STALL};
use crate::supervisor::tests::support::http::spawn_send;
use crate::supervisor::{Pause, lock};

/// The moves of input `id`, as `(from, to)`.
fn moves(rig: &SRig, live: &Live, id: InputId) -> Vec<(String, String)> {
    rig.of_kind(live.t, "input_state_changed")
        .into_iter()
        .filter(|p| p["input_id"] == json!(id))
        .map(|p| {
            (
                p["from"].as_str().unwrap().to_owned(),
                p["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Ticks at `at` (a stopped clock's reading or later) and runs a pass that sees it.
pub(crate) async fn tick_at(rig: &SRig, live: &Live, at: Instant) {
    rig.sup.tick(at).await;
    let sl = rig.sl(live);
    rig.until("the driver forwarded the tick", || {
        lock(&sl.tick).is_some_and(|t| t >= at)
    })
    .await;
    rig.pass(live).await;
}

/// From inside a test point (a blocking context): submits a raw key unit of `bytes` through
/// the launch's session, as `key` does.
fn key_from_point(rig: &SRig, live: &Live) -> impl FnOnce() + Send + 'static {
    let entry = rig.sup.entry(live.l).unwrap();
    let sl = rig.sl(live);
    let not_after = rig.sup.real(rig.sup.now() + Duration::from_secs(60));
    move || {
        let g = entry
            .ops
            .try_lock()
            .expect("the op lock is free before submit");
        let s = g.as_ref().unwrap();
        let unit = WriteUnit::bytes(WriteOrigin::Key, b"\x1b".to_vec());
        sl.submit_human(s, unit, Guard::until(not_after), None)
            .unwrap();
    }
}

/// PR10b-CL-6: `Written` → `sent`; a write that fails after part of the unit → `unconfirmed`
/// with `input_unconfirmed`; a submission the session could not take (`Busy`) → `cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn outcomes() {
    // Incomplete: the submit part fails.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let text = "partial";
    let paste_len = u64::try_from(b"\x1b[200~partial\x1b[201~".len()).unwrap();
    live.child
        .fail_writes_after(paste_len, std::io::ErrorKind::BrokenPipe);
    let id = rig.send(&live, text).await;
    rig.until_input(id, InputState::Unconfirmed).await;
    assert!(
        rig.thread(live.t)
            .state
            .attention
            .contains(&AttentionCause::InputUnconfirmed { input_id: id })
    );
    assert_eq!(moves(&rig, &live, id).last().unwrap().1, "unconfirmed");
    // The worker clears `running` after the completion job commits the input's move.
    rig.until("the record ended the unit", || {
        rig.sl(&live).state().running.is_none()
    })
    .await;
    rig.stop().await;

    // Busy: the op lock taken between the open job and the submit.
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let entry = rig.sup.entry(live.l).unwrap();
    let held = Arc::new(std::sync::Mutex::new(None));
    let h2 = held.clone();
    set(&rig.sl(&live).hooks.before_submit, move || {
        *h2.lock().unwrap() = Some(entry.ops.clone().try_lock_owned().unwrap());
    });
    let id = rig.send(&live, "busy").await;
    rig.until_input(id, InputState::Cancelled).await;
    assert!(live.child.input().is_empty());
    assert_eq!(
        moves(&rig, &live, id),
        vec![
            ("queued".into(), "sending".into()),
            ("sending".into(), "cancelled".into())
        ]
    );
    held.lock().unwrap().take();
    // The next input opens (a zero-byte attempt keeps the gate).
    let b = rig.send(&live, "next").await;
    rig.until_input(b, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-6: a permitted unit with no record `WRITE_STALL` after its `started_at` shows its
/// input `unconfirmed`; the unit keeps running: `ready` answers `unit_running` and the next
/// input stays `queued` until the record, and then until new evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_stall() {
    let rig = SRig::new().await;
    let live = rig.bound(Agent::Codex).await;
    let hold = live.child.hold_writes();
    let id = rig.send(&live, "stalled").await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    rig.clock.stop();
    let t0 = rig.sup.now();
    let started = rig.sl(&live).state().running.as_ref().and_then(|r| {
        r.ticket
            .as_ref()
            .and_then(chaldea_pty_runtime::WriteTicket::started_at)
    });
    assert!(started.is_some(), "the ticket shows the permission");
    // The first pass after the bound shows it; none before.
    tick_at(&rig, &live, t0).await;
    assert_eq!(rig.input(id).state, InputState::Sending);
    tick_at(&rig, &live, t0 + WRITE_STALL + Duration::from_secs(1)).await;
    assert_eq!(rig.input(id).state, InputState::Unconfirmed);
    // The clock catches up with the injected tick.
    rig.clock.advance(WRITE_STALL + Duration::from_secs(1));
    // The owner dismisses it; the unit still runs.
    rig.cmd_ok(
        "input_resolve",
        &json!({ "input_id": id, "delivered": false }),
        200,
    )
    .await;
    let v = rig
        .cmd_ok("ready", &json!({ "thread_id": live.t }), 409)
        .await;
    assert_eq!(v["code"], json!("unit_running"));
    let second = rig.send(&live, "second").await;
    rig.pass(&live).await;
    assert_eq!(rig.input(second).state, InputState::Queued);
    drop(hold);
    rig.until("the record ended the unit", || {
        rig.sl(&live).state().running.is_none()
    })
    .await;
    rig.pass(&live).await;
    assert_eq!(rig.input(id).state, InputState::Dismissed, "left as it was");
    assert_eq!(
        rig.input(second).state,
        InputState::Queued,
        "needs new evidence"
    );
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    rig.until_input(second, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-6, W2/W3: a unit the writer never reaches (queued behind a held key unit) is
/// cancelled `DISPATCH_WAIT` + 1 s after its submission, with zero bytes, by a `try_cancel`
/// that never waits for the queue lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_wait() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let hold = live.child.hold_writes();
    set(
        &rig.sl(&live).hooks.before_submit,
        key_from_point(&rig, &live),
    );
    rig.clock.stop();
    let t0 = rig.sup.now();
    let id = rig.send(&live, "behind").await;
    assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
    rig.until("the unit was submitted", || {
        rig.sl(&live)
            .state()
            .running
            .as_ref()
            .is_some_and(|r| r.ticket.is_some())
    })
    .await;
    assert_eq!(rig.input(id).state, InputState::Sending);
    let unpermitted = rig
        .sl(&live)
        .state()
        .running
        .as_ref()
        .unwrap()
        .ticket
        .as_ref()
        .unwrap()
        .started_at()
        .is_none();
    assert!(unpermitted);
    tick_at(
        &rig,
        &live,
        t0 + DISPATCH_WAIT + READ_WAIT - Duration::from_millis(1),
    )
    .await;
    assert_eq!(rig.input(id).state, InputState::Sending);
    tick_at(&rig, &live, t0 + DISPATCH_WAIT + READ_WAIT).await;
    rig.until_input(id, InputState::Cancelled).await;
    drop(hold);
    rig.until("the key was recorded", || {
        rig.of_kind(live.t, "human_write").len() == 1
    })
    .await;
    assert!(!String::from_utf8_lossy(&live.child.input()).contains("behind"));
    rig.stop().await;
}

/// PR10b-CL-6, W3: a unit parked at its dispatch boundary (the queue lock held) past the
/// stall bound: the worker's `try_cancel` answers `None` and its pass ends; no stall shows
/// (no `started_at`); released after its real deadline, the writer refuses it `Expired`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_before_permission() {
    let rig = SRig::new().await;
    rig.clock.resume();
    let live = rig.claude().await;
    let probe = rig.probe(&live).await;
    let park = probe.park(ProbePoint::Dispatching);
    let id = rig.send(&live, "parked").await;
    let p2 = probe.clone();
    assert!(
        tokio::task::spawn_blocking(move || p2.wait(
            ProbePoint::Dispatching,
            1,
            Instant::now() + Duration::from_secs(10)
        ))
        .await
        .unwrap()
    );
    // The unit's real deadline is at most `DISPATCH_WAIT` after this instant.
    let submitted_by = Instant::now();
    rig.clock.stop();
    let t0 = rig.sup.now();
    let sl = rig.sl(&live);
    let n = sl.passes.load(Ordering::SeqCst);
    tick_at(&rig, &live, t0 + WRITE_STALL + DISPATCH_WAIT).await;
    assert!(sl.passes.load(Ordering::SeqCst) > n, "the pass ended");
    assert_eq!(
        rig.input(id).state,
        InputState::Sending,
        "no stall, not cancelled"
    );
    // Released once the real deadline has passed.
    let until = submitted_by + DISPATCH_WAIT + Duration::from_millis(200);
    tokio::task::spawn_blocking(move || {
        // The writer's deadline is a real instant: only real time reaches it.
        while Instant::now() < until {
            std::thread::park_timeout(until - Instant::now());
        }
        drop(park);
    })
    .await
    .unwrap();
    rig.until_input(id, InputState::Cancelled).await;
    assert!(live.child.input().is_empty());
    rig.stop().await;
}

/// PR10b-CL-6: one launch's worker held before its completion job does not delay another's:
/// launch B's gate timeout is raised on time while A's worker waits, the store writer free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_retry() {
    let rig = SRig::new().await;
    let a = rig.claude().await;
    let (b, _) = rig.spawn(Agent::Codex, None).await;
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    set(&rig.sl(&a).hooks.record_job, move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    let ia = rig.send(&a, "held record").await;
    entered.await.unwrap();
    rig.clock.stop();
    let t0 = rig.sup.now();
    let ib = rig.send(&b, "waits").await;
    tick_at(&rig, &b, t0).await;
    tick_at(&rig, &b, t0 + GATE_TIMEOUT).await;
    rig.until("B's gate timeout", || {
        rig.thread(b.t)
            .state
            .attention
            .contains(&AttentionCause::InputGateTimeout { input_id: ib })
    })
    .await;
    assert_eq!(rig.input(ia).state, InputState::Sending, "A's record waits");
    go.send(()).unwrap();
    rig.until_input(ia, InputState::Sent).await;
    rig.stop().await;
}

/// PR10b-CL-6: a completion whose job comes after the exit job changes nothing (the sweep made
/// the input `unconfirmed`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_exit() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let (entered_tx, entered) = tokio::sync::oneshot::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    set(&rig.sl(&live).hooks.record_job, move || {
        let _ = entered_tx.send(());
        let _ = wait.recv_timeout(Duration::from_secs(30));
    });
    let id = rig.send(&live, "late record").await;
    entered.await.unwrap();
    assert_eq!(rig.input(id).state, InputState::Sending);
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the exit job committed", || {
        rig.launch(live.l).exit_reason.is_some()
    })
    .await;
    rig.until_input(id, InputState::Unconfirmed).await;
    let before = moves(&rig, &live, id);
    go.send(()).unwrap();
    let sl = rig.sl(&live);
    rig.until("the worker ended", || rig.sup.send.launch(live.l).is_none())
        .await;
    drop(sl);
    assert_eq!(moves(&rig, &live, id), before, "nothing changed");
    rig.stop().await;
}

/// PR10b-CL-6 (PR10a1-CL-29's last clause): a sealed `NotStarted` record is applied once,
/// whether the exit job's `SendExitStep` or the worker's completion job commits first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sealed_applied_once() {
    for worker_first in [false, true] {
        let rig = SRig::new().await;
        let live = rig.claude().await;
        // The input unit queued behind a held key unit, so the stop's seal settles it.
        let hold = live.child.hold_writes();
        set(
            &rig.sl(&live).hooks.before_submit,
            key_from_point(&rig, &live),
        );
        let id = rig.send(&live, "sealed").await;
        assert!(hold.wait_entered(Instant::now() + Duration::from_secs(10)));
        rig.until("the unit was submitted", || {
            rig.sl(&live)
                .state()
                .running
                .as_ref()
                .is_some_and(|r| r.ticket.is_some())
        })
        .await;
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let mut armed = None;
        if worker_first {
            armed = Some(rig.pauses.arm(Pause::StopAfterSeal));
        } else {
            set(&rig.sl(&live).hooks.record_job, move || {
                let _ = entered_tx.send(());
                let _ = wait.recv_timeout(Duration::from_secs(30));
            });
        }
        let stop = spawn_send(
            &rig.srv,
            &rig.srv.command(
                "stop",
                &rig.owner,
                crate::supervisor::tests::cid(),
                &json!({ "thread_id": live.t }).to_string(),
            ),
        );
        if worker_first {
            let a = armed.take().unwrap();
            let a = tokio::task::spawn_blocking(move || {
                a.wait();
                a
            })
            .await
            .unwrap();
            rig.until_input(id, InputState::Cancelled).await;
            drop(hold);
            a.go();
        } else {
            entered.await.unwrap();
            drop(hold);
            rig.until_input(id, InputState::Cancelled).await;
            go.send(()).unwrap();
        }
        assert_eq!(stop.await.unwrap().status, 200);
        rig.until("the worker ended", || rig.sup.send.launch(live.l).is_none())
            .await;
        let m = moves(&rig, &live, id);
        assert_eq!(
            m.iter()
                .filter(|(f, t)| f == "sending" && t == "cancelled")
                .count(),
            1,
            "worker first: {worker_first}: {m:?}"
        );
        rig.stop().await;
    }
}

/// PR10b-CL-6: the session gone from its slot between the open job and the submission
/// (`with_session` → `Gone`): the input is `cancelled` with zero bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn submit_gone() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let sl = rig.sl(&live);
    let entry = rig.sup.entry(live.l).unwrap();
    let taken = Arc::new(std::sync::Mutex::new(None));
    let t2 = taken.clone();
    set(&sl.hooks.before_submit, move || {
        let mut slot = entry.ops.try_lock().expect("the op lock is free");
        *t2.lock().unwrap() = slot.take();
    });
    let n = live.child.input().len();
    let id = rig.send(&live, "gone").await;
    rig.until_input(id, InputState::Cancelled).await;
    assert!(taken.lock().unwrap().is_some(), "the session was taken");
    assert_eq!(live.child.input().len(), n, "no byte");
    assert_eq!(
        moves(&rig, &live, id),
        [
            ("queued".to_owned(), "sending".to_owned()),
            ("sending".to_owned(), "cancelled".to_owned())
        ]
    );
    rig.stop().await;
}
