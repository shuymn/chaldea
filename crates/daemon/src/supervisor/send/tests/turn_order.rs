//! Turn boundaries in receipt order (send §2.4): a `UserPromptSubmit` or `Stop` whose
//! receipt ordinal is below a committed turn boundary of its launch changes neither the turn
//! nor the cards, and its prompt does not move the turn-start basis; it still records its
//! `user_message` and acks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, ApprovalId, InputId, StreamId};
use chaldea_core::input::InputState;
use chaldea_core::payload::ApprovalOutcome;
use chaldea_core::state::TurnState;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::evidence::{advance, count, settle_rig};
use super::gate::set;
use super::human::{card_hook, key, outcome};
use super::{Live, SRig};
use crate::hook::TurnMark;
use crate::store::test_faults::CommitFault;
use crate::supervisor::live::LaunchObservation;
use crate::supervisor::send::SETTLE;
use crate::supervisor::tests::support::hook::Hook;
use crate::supervisor::tests::support::http::{Reply, reached, spawn_send};
use crate::supervisor::tests::support::waits;

/// A request's reply handle, carried out of a future that must not await it.
struct Pending(JoinHandle<Reply>);

/// A hook held before its job, its intake ended.
pub(super) struct Held {
    ordinal: u64,
    hook: Hook,
    reply: JoinHandle<Reply>,
    release: oneshot::Sender<()>,
}

impl Held {
    /// Releases the job and returns the hook's reply.
    pub(super) async fn finish(self) -> Reply {
        self.release.send(()).unwrap();
        self.reply.await.unwrap()
    }
}

/// Posts `hook` and holds it before its job, once its intake ended. With `park`, the send
/// worker is parked through the intake so the intake reads the send side's facts.
pub(super) async fn hold(rig: &SRig, live: &Live, hook: Hook, park: bool) -> Held {
    let ordinal = rig.next_ordinal(live);
    let (got, release) = rig.ingest.points.arm_job(ordinal);
    let req = hook.req(&rig.srv);
    // The reply's handle is not awaited here: the request waits in its job's hold.
    let taken_in = async {
        let reply = spawn_send(&rig.srv, &req);
        reached(got).await;
        Pending(reply)
    };
    let Pending(reply) = if park {
        rig.worker_parked(live, taken_in).await
    } else {
        taken_in.await
    };
    Held {
        ordinal,
        hook,
        reply,
        release,
    }
}

fn prompt(text: &str) -> Value {
    json!({ "prompt": text })
}

pub(super) fn turn(rig: &SRig, live: &Live) -> TurnState {
    rig.thread(live.t).state.turn
}

/// The seqs of the thread's events of `kind`, in order.
fn seqs(rig: &SRig, live: &Live, kind: &str) -> Vec<u64> {
    let (t, kind) = (live.t, json!(kind));
    rig.srv.store.read(move |r| {
        Ok(r.events_after(StreamId::Thread(t), 0, 10_000)?
            .events
            .into_iter()
            .filter(|e| serde_json::to_value(&e.kind).unwrap() == kind)
            .map(|e| e.seq)
            .collect())
    })
}

/// The seq of the primary event of `live`'s delivery with receipt ordinal `ordinal`.
fn delivery_seq(rig: &SRig, live: &Live, ordinal: u64) -> Option<u64> {
    let l = live.l;
    rig.srv.store.read(move |r| r.delivery_seq(l, ordinal))
}

/// Blocks a test point until released, off the runtime's threads; the receiver hears it
/// entered.
fn gate_point() -> (
    oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
    impl FnOnce() + Send + 'static,
) {
    let (entered_tx, entered) = oneshot::channel();
    let (go, wait) = std::sync::mpsc::channel::<()>();
    (entered, go, move || {
        let _ = entered_tx.send(());
        tokio::task::block_in_place(|| {
            let _ = wait.recv_timeout(Duration::from_secs(30));
        });
    })
}

/// After `ready`, sends `text` and waits until it is `sent` and its unit ended.
async fn sent(rig: &SRig, live: &Live, text: &str) -> InputId {
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let id = rig.send(live, text).await;
    rig.until_input(id, InputState::Sent).await;
    let sl = rig.sl(live);
    rig.until("the unit ended", || sl.state().running.is_none())
        .await;
    id
}

/// Requires the next send to stay `queued` through two passes, with no byte written.
async fn stays_queued(rig: &SRig, live: &Live) -> InputId {
    let id = rig.send(live, "queued-text").await;
    rig.pass(live).await;
    rig.pass(live).await;
    assert_eq!(rig.input(id).state, InputState::Queued);
    assert!(
        !live
            .child
            .input()
            .windows(b"queued-text".len())
            .any(|w| w == b"queued-text"),
        "no byte"
    );
    id
}

/// Requires `p` acked by the thread's last `user_message`, its `ack_seq` that event's seq.
fn acked_by_last_message(rig: &SRig, live: &Live, p: InputId) {
    let msgs = rig.of_kind(live.t, "user_message");
    assert_eq!(msgs.last().unwrap()["input_id"], json!(p));
    let row = rig.input(p);
    assert_eq!(row.state, InputState::Acked);
    assert_eq!(row.ack_seq, seqs(rig, live, "user_message").last().copied());
}

/// Control: the start commits before its `Stop`; one settle, and the next send is written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_commits_first() {
    let (rig, live) = settle_rig().await;
    let p = sent(&rig, &live, "P").await;
    let start = rig.next_ordinal(&live);
    rig.hook(&live, "UserPromptSubmit", &prompt("P")).await;
    assert_eq!(turn(&rig, &live), TurnState::Running);
    acked_by_last_message(&rig, &live, p);
    assert_eq!(
        rig.launch(live.l).last_turn_start_seq,
        delivery_seq(&rig, &live, start)
    );
    rig.hook(&live, "Stop", &json!({})).await;
    settles(&rig, &live).await;
    let next = rig.send(&live, "next").await;
    rig.until_input(next, InputState::Sent).await;
    rig.stop().await;
}

/// The `Stop` commits before its turn's start. With `observer_held`, the `Stop`'s observer
/// is held once its ticket answered, before it reads the answer, until the start's job ran:
/// the watermark is the writer's, not the observer's. Returns the input and the turn-start
/// basis before the start.
async fn stop_before_start(rig: &SRig, live: &Live, observer_held: bool) -> (InputId, Option<u64>) {
    let p = sent(rig, live, "P").await;
    let before = rig.launch(live.l).last_turn_start_seq;
    let start = hold(
        rig,
        live,
        rig.hook_of(live, "UserPromptSubmit", &prompt("P")),
        true,
    )
    .await;
    if observer_held {
        let faults = crate::http::tests::shared(&rig.srv);
        let req = rig.hook_of(live, "Stop", &json!({})).req(&rig.srv);
        let (stop, go) = rig
            .worker_parked(live, async {
                let (answered, go) = faults.faults.arm_ticket_answer_gate();
                let reply = spawn_send(&rig.srv, &req);
                answered.await.unwrap();
                (Pending(reply), go)
            })
            .await;
        assert_eq!(start.finish().await.status, 200);
        assert_eq!(turn(rig, live), TurnState::Unknown, "observer held");
        go.send(()).unwrap();
        assert_eq!(stop.0.await.unwrap().status, 200);
    } else {
        rig.hook(live, "Stop", &json!({})).await;
        assert_eq!(start.finish().await.status, 200);
    }
    assert_eq!(turn(rig, live), TurnState::Unknown);
    acked_by_last_message(rig, live, p);
    assert_eq!(rig.launch(live.l).last_turn_start_seq, before);
    (p, before)
}

/// TO-1: the stale start neither restarts the turn nor moves the basis; the `Stop`
/// settles and the next send is written without `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_commits_first() {
    for observer_held in [false, true] {
        let (rig, live) = settle_rig().await;
        stop_before_start(&rig, &live, observer_held).await;
        settles(&rig, &live).await;
        let next = rig.send(&live, "next").await;
        rig.until_input(next, InputState::Sent).await;
        rig.stop().await;
    }
}

/// A `Stop` taken in while its input's unit was recorded as `sending` keeps
/// `settle: false` when its start commits after it: no settle, the next send waits for
/// `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_first_without_settle() {
    let (rig, live) = settle_rig().await;
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.record_job, point);
    let p = rig.send(&live, "P").await;
    reached(entered).await;
    assert_eq!(rig.input(p).state, InputState::Sending);
    // The worker is held in the record job, outside the state lock: no parking.
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("P")),
        false,
    )
    .await;
    let stop = rig.next_ordinal(&live);
    let r = rig.post(&rig.hook_of(&live, "Stop", &json!({}))).await;
    assert_eq!(r.status, 200);
    assert_eq!(start.finish().await.status, 200);
    end_mark(&rig, &live, stop, false);
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    assert_eq!(rig.input(p).state, InputState::Acked);
    go.send(()).unwrap();
    rig.pass(&live).await;
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    let q = stays_queued(&rig, &live).await;
    rig.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
        .await;
    rig.until_input(q, InputState::Sent).await;
    rig.stop().await;
}

/// Requires the mark of the delivery with receipt ordinal `o` to be `End { settle }`.
fn end_mark(rig: &SRig, live: &Live, o: u64, settle: bool) {
    let sl = rig.sl(live);
    let log = &sl.state().log.0;
    assert!(
        matches!(log.get(&o), Some((TurnMark::End { settle: s }, _)) if *s == settle),
        "{log:?}"
    );
}

/// Moves the stopped clock `SETTLE` forward once, then wakes passes until one `stop_settled`
/// is recorded: a pass that could not observe the launch keeps the candidate for the next.
async fn settles(rig: &SRig, live: &Live) {
    rig.clock.advance(SETTLE);
    waits::bounded(Duration::from_secs(10), async {
        while count(rig, live, "stop_settled") == 0 {
            rig.pass(live).await;
        }
    })
    .await
    .expect("the Stop settled within 10 s");
    assert_eq!(count(rig, live, "stop_settled"), 1);
}

/// A `Stop` taken in after a `Written` unit's record committed, while the worker has not
/// yet applied the record's reply, sees the unit ended: it settles once its start acks the
/// input, though that start commits after it, and the next send is written without `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_after_the_record_settles() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.record_job, point);
    let p = rig.send(&live, "P").await;
    reached(entered).await;
    // The worker is held in the record job, outside the state lock: no parking.
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("P")),
        false,
    )
    .await;
    let (recorded, go_on, point) = gate_point();
    set(&sl.hooks.after_record, point);
    go.send(()).unwrap();
    reached(recorded).await;
    assert_eq!(rig.input(p).state, InputState::Sent);
    let stop = rig.next_ordinal(&live);
    assert_eq!(
        rig.post(&rig.hook_of(&live, "Stop", &json!({})))
            .await
            .status,
        200
    );
    assert_eq!(start.finish().await.status, 200);
    assert_eq!(rig.input(p).state, InputState::Acked);
    end_mark(&rig, &live, stop, true);
    go_on.send(()).unwrap();
    settles(&rig, &live).await;
    let next = rig.send(&live, "next").await;
    rig.until_input(next, InputState::Sent).await;
    rig.stop().await;
}

/// A `Stop` taken in after an `Incomplete` unit's record committed, while the worker has not
/// yet applied the record's reply, sees the unit ended and settles: the turn is `idle`
/// again without `ready`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_after_an_incomplete_record_settles() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.clock.stop();
    let sl = rig.sl(&live);
    let (recorded, go, point) = gate_point();
    set(&sl.hooks.after_record, point);
    let paste_len = u64::try_from(b"\x1b[200~partial\x1b[201~".len()).unwrap();
    live.child
        .fail_writes_after(paste_len, std::io::ErrorKind::BrokenPipe);
    let p = rig.send(&live, "partial").await;
    reached(recorded).await;
    assert_eq!(rig.input(p).state, InputState::Unconfirmed);
    rig.hook(&live, "UserPromptSubmit", &prompt("typed")).await;
    let stop = rig.next_ordinal(&live);
    rig.hook(&live, "Stop", &json!({})).await;
    end_mark(&rig, &live, stop, true);
    go.send(()).unwrap();
    settles(&rig, &live).await;
    assert_eq!(turn(&rig, &live), TurnState::Idle);
    rig.stop().await;
}

/// A `Stop` whose intake read its facts before a `Written` unit's record committed keeps the
/// unit running, though the record commits before the intake's observation ends: its facts
/// are read before the fence advance, so it never settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_during_intake_keeps_running() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.record_job, point);
    let p = rig.send(&live, "P").await;
    reached(entered).await;
    let recorded = sl.state().running.as_ref().unwrap().recorded.clone();
    // Runs in place of the `Stop`'s observation, after its fence advance: the record
    // commits there, and the launch is observed, so the facts are present.
    let observed = Arc::new(AtomicBool::new(false));
    let o = observed.clone();
    *crate::supervisor::send::lock(&sl.hooks.observe) = Some(Box::new(move |e, _| {
        go.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while !recorded.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        loop {
            let seen = e.observe(None);
            if matches!(seen, LaunchObservation::Started { .. }) {
                o.store(recorded.load(Ordering::Acquire), Ordering::SeqCst);
                return seen;
            }
            if Instant::now() >= end {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }));
    let stop = rig.next_ordinal(&live);
    assert_eq!(
        rig.post(&rig.hook_of(&live, "Stop", &json!({})))
            .await
            .status,
        200
    );
    assert!(
        observed.load(Ordering::SeqCst),
        "the record committed and the launch was observed during the intake"
    );
    end_mark(&rig, &live, stop, false);
    rig.until_input(p, InputState::Sent).await;
    rig.stop().await;
}

/// TO-2b: a human write before a stale start stays in the turn the `Stop` ended. The
/// settle's evidence is of that turn, so it does not reopen the gate after the write.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_start_keeps_the_basis() {
    let (rig, live) = settle_rig().await;
    let before = rig.launch(live.l).last_turn_start_seq;
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("p")),
        true,
    )
    .await;
    rig.attach(&live, b"x").await.await.unwrap();
    let sl = rig.sl(&live);
    rig.until("the human write was recorded", || {
        !seqs(&rig, &live, "human_write").is_empty() && sl.state().human_unsettled == 0
    })
    .await;
    let human = seqs(&rig, &live, "human_write")[0];
    rig.hook(&live, "Stop", &json!({})).await;
    let ordinal = start.ordinal;
    assert_eq!(delivery_seq(&rig, &live, ordinal), None, "not yet appended");
    assert_eq!(start.finish().await.status, 200);
    assert!(human < delivery_seq(&rig, &live, ordinal).unwrap());
    assert_eq!(rig.launch(live.l).last_turn_start_seq, before);
    assert!(before.unwrap() < human);
    settles(&rig, &live).await;
    stays_queued(&rig, &live).await;
    rig.stop().await;
}

/// TO-2: a `Stop` committing after a newer start and a card opened in that turn ends
/// neither: the turn stays `awaiting_approval`, the card open, and nothing settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_stop_keeps_the_turn_and_cards() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    rig.clock.stop();
    rig.hook(&live, "UserPromptSubmit", &prompt("t0")).await;
    let stop = hold(&rig, &live, rig.hook_of(&live, "Stop", &json!({})), true).await;
    rig.hook(&live, "UserPromptSubmit", &prompt("t1")).await;
    let card = card_hook(&rig, &live, "t1");
    assert_eq!(rig.post_for(&live, &card).await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::AwaitingApproval);
    assert_eq!(stop.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::AwaitingApproval);
    assert_eq!(outcome(&rig, ApprovalId(card.emit)), None, "open");
    advance(&rig, &live, SETTLE).await;
    assert_eq!(count(&rig, &live, "stop_settled"), 0);
    rig.stop().await;
}

/// L-13 case f, C1-7: `Start(1)`, `Card(2)`, `Stop(3)` committed 3, 2, 1. The card was taken
/// in below the committed `Stop`, so it is closed at open (`approval_requested` and
/// `approval_resolved` `superseded` in one job, no `ApprovalOpened`): the turn never shows
/// `awaiting_approval`, the stale start changes nothing, and the card's key answers
/// `card_closed` with no byte.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn card_between_stale_start_and_stop() {
    let rig = SRig::new().await;
    let live = rig.claude().await;
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("t")),
        true,
    )
    .await;
    let card = hold(&rig, &live, card_hook(&rig, &live, "t1"), true).await;
    rig.hook(&live, "Stop", &json!({})).await;
    let after_stop = turn(&rig, &live);
    let id = ApprovalId(card.hook.emit);
    assert_eq!(card.finish().await.status, 200);
    assert_eq!(outcome(&rig, id), Some(ApprovalOutcome::Superseded));
    assert_eq!(turn(&rig, &live), after_stop);
    assert_eq!(start.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), after_stop);
    assert!(!rig.sl(&live).state().cards.contains_key(&id));
    let n = live.child.input().len();
    let r = key(&rig, &live, &["enter"], Some(id)).await;
    assert_eq!(
        (r.0, r.1["code"].clone()),
        (409, json!("card_closed")),
        "{r:?}"
    );
    assert_eq!(live.child.input().len(), n, "no byte");
    rig.stop().await;
}

/// Boundaries promoted by the binding `SessionStart` raise the watermark: a start held
/// across the binding, below a promoted `Stop`, is stale. The control promotes the start
/// and its `Stop` in one job in receipt order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promoted_boundaries() {
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    rig.paste(&live, true).await;
    let binding = hold(
        &rig,
        &live,
        rig.hook_of(&live, "SessionStart", &json!({})),
        true,
    )
    .await;
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("p")),
        true,
    )
    .await;
    rig.hook(&live, "Stop", &json!({})).await;
    assert_eq!(binding.finish().await.status, 200);
    assert!(rig.launch(live.l).session_id.is_some(), "bound");
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    assert_eq!(start.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    rig.stop().await;

    // Ordered control: both buffered, promoted together.
    let rig = SRig::new().await;
    let (live, _) = rig.spawn(Agent::Codex, None).await;
    rig.paste(&live, true).await;
    let binding = hold(
        &rig,
        &live,
        rig.hook_of(&live, "SessionStart", &json!({})),
        true,
    )
    .await;
    let start = rig.next_ordinal(&live);
    rig.hook(&live, "UserPromptSubmit", &prompt("p")).await;
    rig.hook(&live, "Stop", &json!({})).await;
    assert_eq!(binding.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    assert_eq!(
        rig.launch(live.l).last_turn_start_seq,
        delivery_seq(&rig, &live, start)
    );
    rig.stop().await;
}

/// TO-3: a boundary whose job rolls back raises nothing; the lower start then applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rolled_back_boundary_raises_nothing() {
    let (rig, live) = settle_rig().await;
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("p")),
        true,
    )
    .await;
    let stop = rig.hook_of(&live, "Stop", &json!({}));
    let r = rig
        .worker_parked(&live, async {
            *rig.srv.store.store().faults().commit.lock().unwrap() =
                Some(CommitFault::BusyBeforeCommit);
            rig.post(&stop).await
        })
        .await;
    assert_eq!(
        (r.status, crate::supervisor::tests::json(&r)["code"].clone()),
        (503, json!("busy"))
    );
    let (l, emit) = (live.l, stop.emit);
    assert!(
        rig.srv
            .store
            .read(move |r| r.delivery_row(l, emit))
            .is_none()
    );
    assert_eq!(start.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::Running);
    rig.stop().await;
}

/// The stale start's projection survives a rebuild: the basis stays before the start, and
/// the input keeps its `ack_seq`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_start_rebuilds() {
    let (rig, live) = settle_rig().await;
    let (p, before) = stop_before_start(&rig, &live, false).await;
    let ack = rig.input(p).ack_seq;
    assert!(ack.is_some());
    rig.srv
        .store
        .store()
        .submit(Instant::now() + Duration::from_secs(5), |tx| {
            tx.rebuild_projections()?;
            Ok(())
        })
        .unwrap()
        .await
        .unwrap();
    assert_eq!(rig.launch(live.l).last_turn_start_seq, before);
    assert_eq!(rig.input(p).ack_seq, ack);
    assert_eq!(rig.input(p).state, InputState::Acked);
    rig.stop().await;
}

/// An exit step that refuses, so the exit job rolls back and the exit stays unrecorded.
pub(super) struct RefuseExit;

impl crate::supervisor::exit::ExitStep for RefuseExit {
    fn in_exit_job(
        &self,
        _tx: &mut crate::store::WriteTx<'_>,
        _exit: &crate::supervisor::exit::ExitFacts,
    ) -> Result<(), crate::store::StoreError> {
        Err(crate::store::StoreError::Constraint(
            "refused by the test".into(),
        ))
    }
}

/// A launch whose exit job rolled back leaves the live map, and its send side with it, while
/// the store still holds it live: a start held across that, below a committed `Stop`, still
/// changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_start_after_unrecorded_exit() {
    let rig = SRig::with(|c| c.exit_steps.push(std::sync::Arc::new(RefuseExit))).await;
    let live = rig.bound(Agent::Codex).await;
    rig.clock.stop();
    let before = rig.launch(live.l).last_turn_start_seq;
    let start = hold(
        &rig,
        &live,
        rig.hook_of(&live, "UserPromptSubmit", &prompt("p")),
        true,
    )
    .await;
    rig.hook(&live, "Stop", &json!({})).await;
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    // Taken before the exit: a pass after it may remove the send side.
    let sl = rig.sl(&live);
    live.child.exit(chaldea_pty_runtime::ChildExit {
        code: Some(0),
        signal: None,
    });
    rig.until("the launch left the live map", || {
        rig.sup.entry(live.l).is_none()
    })
    .await;
    // The worker leaves at its next pass.
    sl.wake.notify_one();
    rig.until("the send side is gone", || {
        rig.sup.send.launch(live.l).is_none()
    })
    .await;
    assert_eq!(
        rig.launch(live.l).exit_reason,
        None,
        "the exit is unrecorded"
    );
    assert_eq!(start.finish().await.status, 200);
    assert_eq!(turn(&rig, &live), TurnState::Unknown);
    assert_eq!(rig.launch(live.l).last_turn_start_seq, before);
    rig.stop().await;
}

/// SF-6: a `Stop` whose intake waits for the launch's held state while the input unit's
/// completion job commits counts the record before its intake point: the state released
/// after the commit, its mark settles.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_during_state_wait() {
    let (rig, live) = settle_rig().await;
    let sl = rig.sl(&live);
    let (entered, go, point) = gate_point();
    set(&sl.hooks.record_job, point);
    let p = rig.send(&live, "P").await;
    reached(entered).await;
    let recorded = sl.state().running.as_ref().unwrap().recorded.clone();
    let (held_tx, held) = std::sync::mpsc::channel();
    let (free, until) = std::sync::mpsc::channel::<()>();
    let s2 = sl.clone();
    let holder = std::thread::spawn(move || {
        let _st = s2.state();
        held_tx.send(()).unwrap();
        let _ = until.recv_timeout(Duration::from_secs(30));
    });
    held.recv().unwrap();
    let (contended_tx, contended) = std::sync::mpsc::channel();
    set(&sl.hooks.state_contended, move || {
        contended_tx.send(()).unwrap();
    });
    let stop = rig.next_ordinal(&live);
    let reply = spawn_send(
        &rig.srv,
        &rig.hook_of(&live, "Stop", &json!({})).req(&rig.srv),
    );
    crate::supervisor::tests::support::http::reached_sync(contended).await;
    go.send(()).unwrap();
    rig.until("the record committed", || recorded.load(Ordering::Acquire))
        .await;
    free.send(()).unwrap();
    holder.join().unwrap();
    assert_eq!(reply.await.unwrap().status, 200);
    end_mark(&rig, &live, stop, true);
    rig.until_input(p, InputState::Sent).await;
    rig.stop().await;
}
