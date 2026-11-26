//! The observation the hook intake reads (design §2.11 "Observation"; PR10a1-CL-30).

use chaldea_core::ids::LaunchId;
use chaldea_pty_runtime::OriginClass;

use super::Rig;
use crate::supervisor::Pause;
use crate::supervisor::live::LaunchObservation;

/// PR10a1-CL-30 `observe`: synchronous calls while the spawn op holds the op lock: right
/// after the reservation `Reserved`; once creation began (before the spawn, after it, and
/// after the reference was sampled, before publication) `Unobservable`; published, with the
/// start job held, `Started` with the sampled `reference`, and a terminal reply the
/// responder then writes has its class position above it; with the map mutex held
/// `Unobservable` at once; an unknown launch `Unobservable`. No send worker runs (the rig's
/// default): its pass takes the map mutex, which `observe` would answer with `Unobservable`.
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
        let seen = rig.sup.observe(launch);
        if let Some(want) = want {
            assert_eq!(seen, want, "{p:?}");
            held.go();
            continue;
        }
        // Published, the start job not yet run. The writer thread takes the queue lock when
        // it first runs (`Unobservable` while it holds it).
        let mut seen = seen;
        rig.until("the published launch observed", || {
            seen = rig.sup.observe(launch);
            seen != LaunchObservation::Unobservable
        })
        .await;
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
        // The writer may still hold the queue lock right after the write (`Unobservable`).
        let mut same = None;
        rig.until("the reply's dispatch observed", || {
            match rig.sup.observe(launch) {
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
        assert_eq!(rig.sup.observe(launch), LaunchObservation::Unobservable);
        drop(map);
        held.go();
    }
    assert_eq!(reply.await.unwrap().status, 200);
    assert_eq!(
        rig.sup.observe(LaunchId(uuid::Uuid::new_v4())),
        LaunchObservation::Unobservable
    );
    rig.srv.stop().await;
}
