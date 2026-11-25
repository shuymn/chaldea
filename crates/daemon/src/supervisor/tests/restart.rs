//! The restart reconcilers (design §2.5): their order, the open launches and in-flight
//! inputs they record, and that startup signals and creates nothing.

use chaldea_core::payload::ExitReason;
use chaldea_pty_runtime::fake::FakeEvent;
use serde_json::json;

use super::support::TestStore;
use super::{Rig, json};
use crate::lifecycle::{Components, Reconciler};
use crate::store::TokenKind;
use crate::supervisor::{SupervisorComponents, SupervisorConfig, restart};

/// Runs `r` on the store as startup runs it: one job.
fn run(store: &TestStore, r: Reconciler) -> u64 {
    tokio::task::block_in_place(|| store.run(r.job).unwrap())
}

/// Every reconciler of the supervisor's components, in order, as startup step 9 runs them.
fn restart_all(store: &TestStore) -> u64 {
    let mut c = SupervisorComponents::new(
        chaldea_core::adapter::HostOs::Macos,
        SupervisorConfig::default(),
    );
    c.reconcilers(store.store())
        .into_iter()
        .map(|r| run(store, r))
        .sum()
}

/// PR10a1-CL-16: receipts first, then launches, then buffers.
#[test]
fn reconciler_order() {
    let store = TestStore::new();
    let mut c = SupervisorComponents::new(
        chaldea_core::adapter::HostOs::Macos,
        SupervisorConfig::default(),
    );
    let names: Vec<_> = c
        .reconcilers(store.store())
        .iter()
        .map(|r| r.name)
        .collect();
    assert_eq!(
        names,
        [
            "receipts_mark_unknown_after_crash",
            "supervisor_launches",
            "supervisor_buffers"
        ]
    );
    let _ = restart::buffers();
}

/// PR10a1-CL-8: a crash between `launch_reserved` and `launch_started` is recorded
/// `unknown_after_crash`, death unconfirmed, and `stop`, `resume`, and `close` answer
/// `death_unconfirmed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_before_started() {
    let rig = Rig::new().await;
    // The op held after the reservation committed, before `Session::spawn`: the store as a
    // crash there leaves it.
    let held = rig.pauses.arm(crate::supervisor::Pause::BeforeSpawn);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let reply = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd, "prompt": "hi" }).to_string(),
        ),
    );
    held.wait();
    let (t, l) = rig.srv.store.read(|r| {
        let t = r.threads()?.pop().unwrap();
        Ok((t.thread_id, t.current_launch_id.unwrap()))
    });
    assert!(restart_all(&rig.srv.store) > 0);
    let row = rig.launch(l);
    assert_eq!(row.exit_reason, Some(ExitReason::UnknownAfterCrash));
    assert!(!row.death_confirmed);
    assert!(
        rig.events(t)
            .iter()
            .any(|(k, p)| k == "input_state_changed" && p["to"] == "unconfirmed"),
        "{:?}",
        rig.kinds(t)
    );
    for (name, args) in [
        ("stop", json!({ "thread_id": t })),
        ("resume", json!({ "thread_id": t, "expected_launch_id": l })),
        ("close", json!({ "thread_id": t, "expected_launch_id": l })),
    ] {
        let r = rig.cmd(name, &args).await;
        assert_eq!(
            (r.status, json(&r)["code"].clone()),
            (409, json!("death_unconfirmed")),
            "{name}"
        );
    }
    held.go();
    let _ = reply.await;
    rig.srv.stop().await;
}

/// PR10a1-CL-14, CL-16: a running launch with a recorded pid is `daemon_restart`, its `sent`
/// positional input `unconfirmed`, its hook token revoked; a second restart writes nothing;
/// the restarted supervisor spawned and signalled nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inputs_recovered() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(Some("hello")).await;
    let child = rig.pty.take_child().unwrap();
    let hook = rig.srv.token(TokenKind::Hook, Some(t), Some(l)).await;
    let owner = rig.owner.clone();
    let (_, store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    drop(rig.sup);
    let signalled = || {
        child
            .log()
            .iter()
            .filter(|e| matches!(e, FakeEvent::Kill { .. } | FakeEvent::Signal))
            .count()
    };
    let old = signalled();
    assert!(restart_all(&store) > 0);
    let before = store.read(move |r| {
        Ok(
            r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                .events
                .len(),
        )
    });
    // A second restart writes nothing.
    assert_eq!(restart_all(&store), 0);
    let after = store.read(move |r| {
        Ok(
            r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                .events
                .len(),
        )
    });
    assert_eq!(before, after);
    let rig = Rig::on(store, chaldea_core::adapter::HostOs::Macos, |_| {}).await;
    let row = rig.launch(l);
    assert_eq!(row.exit_reason, Some(ExitReason::DaemonRestart));
    assert!(!row.death_confirmed);
    assert!(
        rig.events(t)
            .iter()
            .any(|(k, p)| k == "input_state_changed" && p["to"] == "unconfirmed"),
        "{:?}",
        rig.kinds(t)
    );
    assert!(rig.srv.token_row(&hook).unwrap().revoked_at.is_some());
    assert!(rig.srv.token_row(&owner).unwrap().revoked_at.is_none());
    // No signal and no spawn from the restart, the build, or a tick.
    rig.sup.tick(rig.sup.now()).await;
    assert!(rig.pty.spawns().is_empty());
    assert_eq!(signalled(), old, "the restart signalled nothing");
    rig.srv.stop().await;
}
