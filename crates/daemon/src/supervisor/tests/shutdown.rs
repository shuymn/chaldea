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
