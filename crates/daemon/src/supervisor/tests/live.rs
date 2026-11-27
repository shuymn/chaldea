//! The observation the hook intake reads (design §2.11 "Observation"; PR10a1-CL-30).

use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::FakeScript;
use chaldea_pty_runtime::{Cleanup, NeverStartedKind, OriginClass, SpawnError};

use super::Rig;
use crate::supervisor::live::{LaunchObservation, LiveLaunch};
use crate::supervisor::{Pause, lock};

/// PR10a1-CL-30 `LiveLaunch::observe`: synchronous calls while the spawn op holds the op
/// lock: right after the reservation `Reserved`; once creation began (before the spawn, after
/// it, and after the reference was sampled, before publication) `Unobservable`; published,
/// with the start job held, `Started` with the sampled `reference`, and a terminal reply the
/// responder then writes has its class position above it; with the live map held, the same
/// at once (the observation takes no lock).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observe() {
    let rig = Rig::new().await;
    let points = [
        (Pause::AfterReserve, Some(LaunchObservation::Reserved)),
        (Pause::BeforeSpawn, Some(LaunchObservation::Unobservable)),
        (Pause::AfterSpawn, Some(LaunchObservation::Unobservable)),
        (Pause::BeforePublish, Some(LaunchObservation::Unobservable)),
        (Pause::BeforeStartJob, None),
    ];
    let armed: Vec<_> = points.iter().map(|(p, _)| rig.pauses.arm(*p)).collect();
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &serde_json::json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    let mut l = None;
    for ((p, want), held) in points.into_iter().zip(armed) {
        held.wait();
        let launch = *l.get_or_insert_with(|| {
            rig.srv
                .store
                .read(|r| Ok(r.threads()?.pop().unwrap().current_launch_id.unwrap()))
        });
        let entry = rig.sup.entry(launch).unwrap();
        let seen = entry.observe(None);
        if let Some(want) = want {
            assert_eq!(seen, want, "{p:?}");
            held.go();
            continue;
        }
        // Published, the start job not yet run.
        let LaunchObservation::Started {
            reference,
            last_started,
        } = seen
        else {
            panic!("{seen:?}");
        };
        let reply_class = OriginClass::Reply.index();
        assert!(last_started[reply_class] <= reference, "{seen:?}");
        let child = rig.pty.take_child().unwrap();
        child.emit(b"\x1b[5n");
        let mut same = None;
        rig.until("the reply's dispatch observed", || {
            match entry.observe(None) {
                LaunchObservation::Started {
                    reference: r,
                    last_started,
                } if last_started[reply_class] > reference => {
                    same = Some(r);
                    true
                }
                _ => false,
            }
        })
        .await;
        assert_eq!(
            same,
            Some(reference),
            "the reference sampled before publication"
        );
        let map = rig.sup.live.lock();
        assert!(matches!(
            entry.observe(None),
            LaunchObservation::Started { reference: r, .. } if r == reference
        ));
        drop(map);
        held.go();
    }
    assert_eq!(reply.await.unwrap().status, 200);
    rig.srv.stop().await;
}

/// Starts a spawn of `agent` held at `pause` and returns the held point, the reply, and the
/// launch's entry.
async fn held_spawn(
    rig: &Rig,
    pause: Pause,
    agent: &str,
) -> (
    super::Armed,
    tokio::task::JoinHandle<super::support::http::Reply>,
    Arc<LiveLaunch>,
) {
    let held = rig.pauses.arm(pause);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &serde_json::json!({ "agent": agent, "cwd": cwd }).to_string(),
        ),
    );
    tokio::task::block_in_place(|| held.wait());
    let launch = rig
        .srv
        .store
        .read(|r| Ok(r.threads()?.pop().unwrap().current_launch_id.unwrap()));
    let entry = rig.sup.entry(launch).unwrap();
    (held, reply, entry)
}

/// Runs `entry.await_published` with a facts deadline 10 s away on its own thread, once it
/// reported its first wait; returns how long the wait took.
fn waiting(
    entry: &Arc<LiveLaunch>,
) -> (
    std::sync::mpsc::Receiver<()>,
    std::thread::JoinHandle<Duration>,
) {
    let (waiting_tx, waiting) = std::sync::mpsc::channel();
    *lock(&entry.hooks.publication_waiting) = Some(Box::new(move || {
        let _ = waiting_tx.send(());
    }));
    let e = entry.clone();
    let h = std::thread::spawn(move || {
        let t0 = Instant::now();
        e.await_published(t0 + Duration::from_secs(10));
        t0.elapsed()
    });
    (waiting, h)
}

/// SF-8 `observe` and the removal: a launch removed before its creation began is
/// `Unobservable` with no wait; one whose spawn fails after creation began ends a
/// publication wait at the removal, not at the facts deadline; a published launch detached
/// and still in the map is `Started`; one removed between the positions' read and the return
/// is `Unobservable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observe_removed() {
    // Removed before creation began.
    let rig = Rig::new().await;
    let (held, reply, entry) = held_spawn(&rig, Pause::AfterReserve, "codex").await;
    rig.sup.remove(entry.launch_id);
    assert!(entry.removed());
    assert_eq!(entry.observe(None), LaunchObservation::Unobservable);
    let t0 = Instant::now();
    entry.await_published(t0 + Duration::from_secs(10));
    assert!(t0.elapsed() < Duration::from_secs(1), "no wait");
    held.go();
    let _ = reply.await.unwrap();
    rig.srv.stop().await;

    // The spawn fails after creation began: the wait ends on the removal.
    let rig = Rig::new().await;
    rig.pty.script_next(FakeScript {
        fail: Some(SpawnError::NeverStarted {
            kind: NeverStartedKind::Setup,
            error: std::io::Error::other("x"),
            cleanup: Cleanup::Confirmed,
        }),
        ..FakeScript::default()
    });
    let (held, reply, entry) = held_spawn(&rig, Pause::BeforeSpawn, "codex").await;
    assert!(entry.creation_started());
    let (reported, h) = waiting(&entry);
    reported.recv_timeout(Duration::from_secs(10)).unwrap();
    held.go();
    assert_eq!(
        super::json(&reply.await.unwrap())["outcome"],
        "spawn_failed"
    );
    assert!(
        h.join().unwrap() < Duration::from_secs(5),
        "no stall to the deadline"
    );
    assert!(entry.removed());
    assert_eq!(entry.observe(None), LaunchObservation::Unobservable);
    rig.srv.stop().await;

    // Detached and still in the map; then removed between the positions and the return.
    let rig = Rig::new().await;
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd(
            "spawn",
            &serde_json::json!({ "agent": "codex", "cwd": cwd }),
        )
        .await;
    assert_eq!(r.status, 200);
    let launch = serde_json::from_value(super::json(&r)["launch_id"].clone()).unwrap();
    let entry = rig.sup.entry(launch).unwrap();
    let mut g = entry
        .lock_before(&rig.sup, rig.sup.now() + Duration::from_secs(5))
        .await
        .unwrap();
    let session = entry.take_session(&mut g);
    assert!(
        matches!(entry.observe(None), LaunchObservation::Started { .. }),
        "detached"
    );
    *g = session;
    drop(g);
    let (sup, l) = (rig.sup.clone(), entry.launch_id);
    *lock(&entry.hooks.after_positions) = Some(Box::new(move || sup.remove(l)));
    assert_eq!(entry.observe(None), LaunchObservation::Unobservable);
    assert!(entry.removed());
    rig.srv.stop().await;
}

/// SF-8: a publication or a removal made after the publication wait began is seen promptly,
/// well before the facts deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn poll_sees_later_change() {
    // Published.
    let rig = Rig::new().await;
    let (held, reply, entry) = held_spawn(&rig, Pause::BeforePublish, "codex").await;
    let (reported, h) = waiting(&entry);
    reported.recv_timeout(Duration::from_secs(10)).unwrap();
    held.go();
    assert!(
        h.join().unwrap() < Duration::from_secs(2),
        "the publication was seen"
    );
    assert!(entry.published().is_some());
    assert_eq!(reply.await.unwrap().status, 200);
    rig.srv.stop().await;
    // Removed.
    let rig = Rig::new().await;
    let (held, reply, entry) = held_spawn(&rig, Pause::BeforePublish, "codex").await;
    let (reported, h) = waiting(&entry);
    reported.recv_timeout(Duration::from_secs(10)).unwrap();
    rig.sup.remove(entry.launch_id);
    assert!(
        h.join().unwrap() < Duration::from_secs(2),
        "the removal was seen"
    );
    held.go();
    let _ = reply.await.unwrap();
    rig.srv.stop().await;
}
