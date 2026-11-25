//! The shutdown participant and the tracker (design §2.9; PR10a1-CL-33, CL-36).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::payload::ExitReason;
use chaldea_pty_runtime::fake::{FakeScript, KillBehavior};

use super::Rig;
use crate::lifecycle::ParticipantEnd;
use crate::supervisor::components::shutdown;
use crate::supervisor::tracker::Tracker;

/// PR10a1-CL-33, CL-36: each live session closed and recorded against `end`: a proven kill
/// is `stopped` confirmed and the participant `Clean`; a tree not proven dead is `stopped`
/// unconfirmed, its token stays counted, and the participant is `LeftRunning`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owned_close() {
    for (kill, confirmed, end_as) in [
        (KillBehavior::ExitOnKill, true, ParticipantEnd::Clean),
        (KillBehavior::Ignore, false, ParticipantEnd::LeftRunning),
    ] {
        let rig = Rig::new().await;
        rig.pty.script_next(FakeScript {
            kill,
            ..FakeScript::default()
        });
        let (_t, l) = rig.spawn_codex(None).await;
        let end = rig.sup.now() + Duration::from_secs(2);
        let started = Instant::now();
        let got = shutdown(rig.sup.clone(), end).await;
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(got, end_as, "{kill:?}");
        let row = rig.launch(l);
        assert_eq!(row.exit_reason, Some(ExitReason::Stopped), "{kill:?}");
        assert_eq!(row.death_confirmed, confirmed, "{kill:?}");
        assert!(!rig.sup.tracker.accepting());
        rig.srv.stop().await;
    }
}

/// PR10a1-CL-36 `zero_wakeup`: the last token released before the wait, between its
/// wake-up's enable and its count read, and after a nonzero read before its first await:
/// true by the wake-up each time, long before `end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_wakeup() {
    let end = || Instant::now() + Duration::from_secs(20);
    // Before.
    let t = Arc::new(Tracker::default());
    t.admit().unwrap().release();
    assert!(t.empty_before(end()).await);
    // Between the enable and the count read.
    let t = Arc::new(Tracker::default());
    let tok = t.admit().unwrap();
    *t.hooks.after_enable.lock().unwrap() = Some(Box::new(move || tok.release()));
    let started = Instant::now();
    assert!(t.empty_before(end()).await);
    assert!(started.elapsed() < Duration::from_secs(10));
    // After a nonzero read, before the first await.
    let t = Arc::new(Tracker::default());
    let tok = t.admit().unwrap();
    *t.hooks.after_count.lock().unwrap() = Some(Box::new(move || tok.release()));
    let started = Instant::now();
    assert!(t.empty_before(end()).await);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "woken, not ended"
    );
    // A token never released: false at `end`.
    let t = Arc::new(Tracker::default());
    let _held = t.admit().unwrap();
    assert!(
        !t.empty_before(Instant::now() + Duration::from_millis(50))
            .await
    );
}

/// PR10a1-CL-22 `close_stop_after_admission`: a `close --stop` whose stop committed and
/// whose closing job is refused `Closed` once the store's admission closed: 503
/// `outcome_unknown`, nothing reported to the sink; after a restart the same `command_id`
/// is 409 `unknown_after_crash`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_stop_after_admission() {
    use serde_json::json;
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(None).await;
    let id = super::cid();
    let args = json!({ "thread_id": t, "expected_launch_id": l, "stop": true });
    let held = rig.pauses.arm(crate::supervisor::Pause::BeforeClosingJob);
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command("close", &rig.owner, id, &args.to_string()),
    );
    held.wait();
    assert_eq!(rig.launch(l).exit_reason, Some(ExitReason::Stopped));
    let store = rig.srv.store.store().clone();
    let closing =
        std::thread::spawn(move || store.shutdown(Instant::now() + Duration::from_secs(20)));
    rig.until("admission closed", || {
        let job: crate::http::Job<()> = Box::new(|_| Ok(()));
        rig.srv
            .store
            .store()
            .submit(Instant::now() + Duration::from_secs(5), job)
            .is_err()
    })
    .await;
    held.go();
    let r = reply.await.unwrap();
    assert_eq!(
        (r.status, super::json(&r)["code"].clone()),
        (503, json!("outcome_unknown")),
        "{}",
        String::from_utf8_lossy(&r.body)
    );
    assert!(rig.sink.0.lock().unwrap().is_empty(), "{:?}", rig.sink.0);
    assert!(closing.join().unwrap().writer_exited);
    let owner = rig.owner.clone();
    drop(rig.sup);
    let (_, mut store) = rig.srv.into_store(Duration::from_secs(10)).await;
    store.reopen();
    super::restart::restart_all(&store);
    let rig = Rig::on(store, chaldea_core::adapter::HostOs::Macos, |_| {}).await;
    let r = rig.cmd_as(&owner, "close", id, &args).await;
    assert_eq!(
        (r.status, super::json(&r)["code"].clone()),
        (409, json!("unknown_after_crash"))
    );
    rig.srv.stop().await;
}
