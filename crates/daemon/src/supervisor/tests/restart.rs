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
pub(crate) fn restart_all(store: &TestStore) -> u64 {
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

/// PR10a1-CL-14: a running launch with a recorded pid is `daemon_restart`, its `sent`
/// positional input `unconfirmed`, its hook token revoked; a second restart writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inputs_recovered() {
    let rig = Rig::new().await;
    let (t, l) = rig.spawn_codex(Some("hello")).await;
    let child = rig.pty.take_child().unwrap();
    let hook = rig.srv.token(TokenKind::Hook, Some(t), Some(l)).await;
    let owner = rig.owner.clone();
    let (_, store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
    drop(rig.sup);
    drop(child);
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
    rig.srv.stop().await;
}

/// PR10a1-CL-15 `buffers_resolved`: a buffer left by a launch that exited in an earlier run
/// is resolved by the restart's `supervisor_buffers`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn buffers_resolved() {
    let rig = Rig::new().await;
    let (t, l) = rig.stopped_bound().await;
    super::exit::buffer_one(&rig.srv.store, l);
    assert_eq!(super::exit::buffered(&rig.srv.store), vec![l]);
    assert!(restart_all(&rig.srv.store) > 0);
    assert!(super::exit::buffered(&rig.srv.store).is_empty());
    assert!(
        rig.kinds(t).iter().any(|k| k == "raw_unknown"),
        "{:?}",
        rig.kinds(t)
    );
    rig.srv.stop().await;
}

/// A `git` that appends a line to `marker` and nothing else: any run of it is counted.
fn counting_git(marker: &std::path::Path) -> Vec<std::ffi::OsString> {
    let m = marker.to_string_lossy();
    let argv: Vec<String> = if cfg!(windows) {
        vec![
            "powershell".into(),
            "-NoProfile".into(),
            "-Command".into(),
            format!("Add-Content -LiteralPath '{m}' -Value x; #"),
        ]
    } else {
        vec![
            "sh".into(),
            "-c".into(),
            "echo x >> \"$0\"".into(),
            m.into_owned(),
        ]
    };
    argv.into_iter().map(std::ffi::OsString::from).collect()
}

/// PR10a1-CL-16 `no_signal`: with each host's adapters, a launch left open with a recorded
/// pid; the reconcilers, the build, and one tick: no spawn, no kill or signal to the old
/// launch's processes, no git run, and the launch recorded `daemon_restart`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_signal() {
    use chaldea_core::adapter::HostOs;
    for host in [HostOs::Macos, HostOs::Windows] {
        let rig = Rig::with(host, |_| {}).await;
        let (_t, l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
        let (_, store) = rig.srv.into_store(std::time::Duration::from_secs(10)).await;
        drop(rig.sup);
        let before = child.log().len();
        assert!(restart_all(&store) > 0);
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("git-ran");
        let git = counting_git(&marker);
        let rig = Rig::on(store, host, move |c| c.git = git).await;
        rig.sup.tick(rig.sup.now()).await;
        assert!(rig.pty.spawns().is_empty(), "{host:?}: no spawn");
        let after = &child.log()[before..];
        assert!(
            !after
                .iter()
                .any(|e| matches!(e, FakeEvent::Kill { .. } | FakeEvent::Signal)),
            "{host:?}: {after:?}"
        );
        assert!(!marker.exists(), "{host:?}: git ran");
        assert_eq!(rig.launch(l).exit_reason, Some(ExitReason::DaemonRestart));
        rig.srv.stop().await;
    }
}
