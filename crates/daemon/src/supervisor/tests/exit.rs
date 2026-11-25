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
