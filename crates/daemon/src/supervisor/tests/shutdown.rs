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
        rig.clock.resume();
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
/// true by the wake-up each time, long before `end`. A token never released: false at `end`;
/// a zero read only at `end`: false.
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
    // A zero read after `end` (its last token released late): not adopted.
    let t = Arc::new(Tracker::default());
    t.admit().unwrap().release();
    assert!(!t.empty_before(Instant::now()).await);
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
    // Only `Closed` says admission closed: the polls themselves fill the writer queue
    // (`QueueFull`) while the shutdown thread has not yet begun.
    rig.until("admission closed", || {
        let job: crate::http::Job<()> = Box::new(|_| Ok(()));
        matches!(
            rig.srv
                .store
                .store()
                .submit(Instant::now() + Duration::from_secs(5), job),
            Err(crate::store::SubmitError::Closed)
        )
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

/// A graceful stop of `rig`'s supervisor with 1 s to go.
async fn graceful(rig: &Rig) -> ParticipantEnd {
    shutdown(rig.sup.clone(), rig.sup.now() + Duration::from_secs(1)).await
}

/// PR10a1-CL-36 `release_rule`: a token is released only by a close that released
/// everything. Each of these leaves it counted, so a later graceful stop is `LeftRunning`:
/// a close with `pty-wait` abandoned (the launch still `death_confirmed`, the tree proven
/// dead), a kill that panics, a `StartedThenFailed` with an unresolved cleanup, a close with
/// `tree_dead: false`. Controls, `Clean`: a close that released everything, a `NeverStarted`
/// with a confirmed cleanup, and a program not found, which takes no token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn release_rule() {
    use chaldea_pty_runtime::{Cleanup, NeverStartedKind, SpawnError};
    use serde_json::json;
    let stopped = |kill, hold_waiter| FakeScript {
        kill,
        hold_waiter,
        ..FakeScript::default()
    };
    // Stopped launches.
    for (name, script, confirmed, want) in [
        (
            "pty-wait abandoned",
            stopped(KillBehavior::ExitOnKill, true),
            true,
            ParticipantEnd::LeftRunning,
        ),
        (
            "kill panics",
            stopped(KillBehavior::Panic, false),
            false,
            ParticipantEnd::LeftRunning,
        ),
        (
            "tree_dead false",
            stopped(KillBehavior::Ignore, false),
            false,
            ParticipantEnd::LeftRunning,
        ),
        (
            "everything released",
            stopped(KillBehavior::ExitOnKill, false),
            true,
            ParticipantEnd::Clean,
        ),
    ] {
        let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
            c.kill_budget = Duration::from_millis(300);
            c.close_bounds.join = Duration::from_millis(300);
        })
        .await;
        rig.clock.resume();
        rig.pty.script_next(script);
        let (t, l) = rig.spawn_codex(None).await;
        let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
        assert_eq!(r.status, 200, "{name}");
        assert_eq!(rig.launch(l).death_confirmed, confirmed, "{name}");
        assert_eq!(graceful(&rig).await, want, "{name}");
        rig.srv.stop().await;
    }
    // Spawn failures.
    let unresolved = Cleanup::Unresolved {
        kill: None,
        close: None,
        drain_abandoned: false,
    };
    for (name, fail, want) in [
        (
            "StartedThenFailed, unresolved",
            SpawnError::StartedThenFailed {
                stage: "s",
                error: std::io::Error::other("x"),
                identity: None,
                cleanup: unresolved,
            },
            ParticipantEnd::LeftRunning,
        ),
        (
            "NeverStarted, confirmed",
            SpawnError::NeverStarted {
                kind: NeverStartedKind::Setup,
                error: std::io::Error::other("x"),
                cleanup: Cleanup::Confirmed,
            },
            ParticipantEnd::Clean,
        ),
    ] {
        let rig = Rig::new().await;
        rig.clock.resume();
        rig.pty.script_next(FakeScript {
            fail: Some(fail),
            ..FakeScript::default()
        });
        let cwd = rig.cwd.path().to_string_lossy().into_owned();
        let r = rig
            .cmd("spawn", &json!({ "agent": "codex", "cwd": cwd }))
            .await;
        assert_eq!(super::json(&r)["outcome"], "spawn_failed", "{name}");
        assert_eq!(graceful(&rig).await, want, "{name}");
        rig.srv.stop().await;
    }
    // A program not found: no token.
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.program = Some(Arc::new(|_| None));
    })
    .await;
    rig.clock.resume();
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "codex", "cwd": cwd }))
        .await;
    assert_eq!(super::json(&r)["outcome"], "spawn_failed");
    assert!(rig.pty.spawns().is_empty());
    assert_eq!(rig.sup.tracker.count(), 0);
    assert_eq!(graceful(&rig).await, ParticipantEnd::Clean);
    rig.srv.stop().await;
}

/// Starts a graceful stop with 1 s to go while `held` holds a token's owner, and releases
/// it before the end (once admission closed) or only after the stop returned.
async fn stop_while_held(
    rig: &Rig,
    held: super::Armed,
    release_before_end: bool,
) -> ParticipantEnd {
    let sd = tokio::spawn(shutdown(
        rig.sup.clone(),
        rig.sup.now() + Duration::from_secs(1),
    ));
    if release_before_end {
        rig.until("admission closed", || !rig.sup.tracker.accepting())
            .await;
        held.go();
        sd.await.unwrap()
    } else {
        let end = sd.await.unwrap();
        held.go();
        end
    }
}

fn stop_req(rig: &Rig, t: chaldea_core::ids::ThreadId) -> super::support::http::Req {
    rig.srv.command(
        "stop",
        &rig.owner,
        super::cid(),
        &serde_json::json!({ "thread_id": t }).to_string(),
    )
}

/// PR10a1-CL-36 `tracker`: a token held by each of these, past the stop's end →
/// `LeftRunning`, released before it → `Clean`: an op after its token and before
/// `Session::spawn` returns; a natural-exit op after taking the session out of its slot,
/// before its kill task; a sealed stop's kill task before its start.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tracker() {
    use crate::supervisor::Pause;
    use chaldea_pty_runtime::ChildExit;
    for release_before_end in [false, true] {
        let want = if release_before_end {
            ParticipantEnd::Clean
        } else {
            ParticipantEnd::LeftRunning
        };
        // Before `Session::spawn`.
        let rig = Rig::new().await;
        rig.clock.resume();
        let held = rig.pauses.arm(Pause::BeforeSpawn);
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
        held.wait();
        assert_eq!(
            stop_while_held(&rig, held, release_before_end).await,
            want,
            "before spawn"
        );
        let _ = reply.await;
        rig.srv.stop().await;
        // A natural exit's session out of its slot.
        let rig = Rig::new().await;
        rig.clock.resume();
        let (_t, _l) = rig.spawn_codex(None).await;
        let child = rig.pty.take_child().unwrap();
        let held = rig.pauses.arm(Pause::ExitTookSession);
        child.exit(ChildExit {
            code: Some(0),
            signal: None,
        });
        held.wait();
        assert_eq!(
            stop_while_held(&rig, held, release_before_end).await,
            want,
            "natural exit"
        );
        rig.srv.stop().await;
        // A sealed stop's kill task before its start.
        let rig = Rig::new().await;
        rig.clock.resume();
        let (t, _l) = rig.spawn_codex(None).await;
        let held = rig.pauses.arm(Pause::KillTaskStart);
        let reply = super::support::http::spawn_send(&rig.srv, &stop_req(&rig, t));
        held.wait();
        assert_eq!(
            stop_while_held(&rig, held, release_before_end).await,
            want,
            "kill task"
        );
        let _ = reply.await;
        rig.srv.stop().await;
    }
}

/// PR10a1-CL-36 `publish_vs_admission`: a publication either completes before admission
/// closes or does not happen. Held before `Tracker::while_accepting` while admission
/// closes: refused, the op closes its own session (its late-creation close) and records
/// `launch_started` and `stopped`, with the positional input moved `sending → sent` after
/// `launch_started`'s seq by the start's own records, as a normal start job does. Held
/// inside it: `close_admission` waits for it, and the participant finds the entry published
/// and closes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn publish_vs_admission() {
    use crate::supervisor::Pause;
    for (at, closer) in [
        (Pause::BeforePublish, Pause::LateCloseEnd),
        (Pause::InPublish, Pause::ShutdownCloseEnd),
    ] {
        let rig = Rig::new().await;
        rig.clock.resume();
        let held = rig.pauses.arm(at);
        let closed_by = rig.pauses.arm(closer);
        let cwd = rig.cwd.path().to_string_lossy().into_owned();
        let reply = super::support::http::spawn_send(
            &rig.srv,
            &rig.srv.command(
                "spawn",
                &rig.owner,
                super::cid(),
                &serde_json::json!({ "agent": "codex", "cwd": cwd, "prompt": "hi" }).to_string(),
            ),
        );
        held.wait();
        // Inside the publication, admission's mutex is held: `close_admission` waits.
        assert_eq!(
            rig.sup.tracker.mutex_held(),
            at == Pause::InPublish,
            "{at:?}"
        );
        let sd = tokio::spawn(shutdown(
            rig.sup.clone(),
            rig.sup.now() + Duration::from_secs(5),
        ));
        if at == Pause::BeforePublish {
            rig.until("admission closed", || !rig.sup.tracker.accepting())
                .await;
        }
        let mut commits = rig.srv.store.store().subscribe();
        held.go();
        closed_by.wait();
        closed_by.go();
        assert_eq!(sd.await.unwrap(), ParticipantEnd::Clean, "{at:?}");
        let _ = reply.await;
        let l = rig
            .srv
            .store
            .read(|r| Ok(r.threads()?.pop().unwrap().current_launch_id.unwrap()));
        let kinds = rig.kinds(rig.launch(l).thread_id);
        assert!(
            kinds.iter().any(|k| k == "launch_started"),
            "{at:?}: {kinds:?}"
        );
        assert_eq!(
            rig.launch(l).exit_reason,
            Some(ExitReason::Stopped),
            "{at:?}"
        );
        if at == Pause::BeforePublish {
            let t = rig.launch(l).thread_id;
            let events = rig.srv.store.read(move |r| {
                Ok(
                    r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                        .events,
                )
            });
            let kind =
                |e: &chaldea_core::event::EventEnvelope| serde_json::to_value(&e.kind).unwrap();
            let payload = |e: &chaldea_core::event::EventEnvelope| -> serde_json::Value {
                serde_json::from_str(e.payload.get()).unwrap()
            };
            let started = events.iter().find(|e| kind(e) == "launch_started").unwrap();
            let exited = events.iter().find(|e| kind(e) == "launch_exited").unwrap();
            let sent = events
                .iter()
                .find(|e| kind(e) == "input_state_changed" && payload(e)["to"] == "sent")
                .expect("the positional input moved to sent");
            assert_eq!(payload(sent)["sent_after_seq"], started.seq);
            assert!(started.seq < sent.seq && sent.seq < exited.seq);
            // The commit that first shows `launch_started` already shows `sent`.
            let stream = chaldea_core::ids::StreamId::Thread(t);
            let first = std::iter::from_fn(|| commits.try_recv().ok())
                .flat_map(|c| c.heads)
                .filter(|h| h.stream_id == stream)
                .map(|h| h.head_seq)
                .find(|&head| head >= started.seq)
                .expect("a commit shows launch_started");
            assert!(first >= sent.seq, "sent in a later commit: {first}");
            assert_eq!(
                sent.idempotency_key.0,
                format!(
                    "derived:launch:{l}:started:input:{}",
                    payload(sent)["input_id"].as_str().unwrap()
                ),
                "derived from the start's own record"
            );
        }
        rig.srv.stop().await;
    }
}

/// PR10a1-CL-19 `git_spawn_stalled`: a probe held at the OS spawn's entry with its token:
/// shutdown closes admission at once, a live PTY is closed and recorded, and the
/// participant is `LeftRunning` by its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_spawn_stalled() {
    use crate::supervisor::Pause;
    use crate::supervisor::workspace::common_dir;
    let rig = Rig::new().await;
    rig.clock.resume();
    let (_t, l) = rig.spawn_codex(None).await;
    let dir = tempfile::tempdir().unwrap();
    let held = rig.pauses.arm(Pause::GitAfterAdmit);
    let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
    let probe =
        std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(5)));
    held.wait();
    assert_eq!(
        rig.sup.tracker.count(),
        2,
        "the PTY's token and the probe's"
    );
    let end = rig.sup.now() + Duration::from_secs(1);
    assert_eq!(
        shutdown(rig.sup.clone(), end).await,
        ParticipantEnd::LeftRunning
    );
    assert_eq!(rig.launch(l).exit_reason, Some(ExitReason::Stopped));
    held.go();
    let _ = probe.join();
    rig.srv.stop().await;
}

/// PR10a1-CL-19 `git_reaped`: a sleeping git still held by its probe at shutdown is killed
/// at the probe's next poll and reaped by the end, `Clean`; a sleeping git in `left` is
/// killed and reaped by the participant, `Clean`; that reap held past `end`: `LeftRunning`
/// by `end`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_reaped() {
    use crate::supervisor::Pause;
    use crate::supervisor::workspace::common_dir;
    let dir = tempfile::tempdir().unwrap();
    for in_left in [false, true] {
        let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
            c.git = super::workspace::sleeping_git();
        })
        .await;
        rig.clock.resume();
        let created = if in_left {
            rig.pauses.arm_panic(Pause::GitAfterCreate)
        } else {
            rig.pauses.arm(Pause::GitAfterCreate)
        };
        let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
        let probe =
            std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(20)));
        created.wait();
        created.go();
        let mut probe = Some(probe);
        if in_left {
            let p = probe.take().unwrap();
            assert!(p.join().is_err(), "the probe panicked");
            assert!(!rig.sup.git.idle(), "the child is in `left`");
        }
        let started = Instant::now();
        let end = rig.sup.now() + Duration::from_secs(3);
        assert_eq!(
            shutdown(rig.sup.clone(), end).await,
            ParticipantEnd::Clean,
            "in_left {in_left}"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(rig.sup.git.idle());
        if let Some(p) = probe {
            assert!(p.join().unwrap().is_err(), "the probe's git was killed");
        }
        rig.srv.stop().await;
    }
    // The participant's reap of `left` held past `end`: the participant answers
    // `LeftRunning` by `end` without waiting for it, and a reap that then succeeds changes
    // nothing.
    let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
        c.git = super::workspace::sleeping_git();
    })
    .await;
    rig.clock.resume();
    let created = rig.pauses.arm_panic(Pause::GitAfterCreate);
    let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
    let probe =
        std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(20)));
    created.wait();
    created.go();
    assert!(probe.join().is_err(), "the probe panicked");
    let reap = rig.pauses.arm(Pause::GitShutdownReap);
    let end = rig.sup.now() + Duration::from_secs(1);
    let sd = tokio::spawn(shutdown(rig.sup.clone(), end));
    let reap = tokio::task::spawn_blocking(move || {
        reap.wait();
        reap
    })
    .await
    .unwrap();
    rig.until("the participant answered with the reap held", || {
        sd.is_finished()
    })
    .await;
    reap.go();
    assert_eq!(sd.await.unwrap(), ParticipantEnd::LeftRunning);
    rig.until("the held reap ran", || rig.sup.git.idle()).await;
    rig.srv.stop().await;
}

/// PR10a1-CL-19 `git_late_handoff`: the git step reads the slot while a probe holds the
/// lease with `left` empty. Released, the probe kills its git at its next poll (admission
/// closed) and reaps it; in a second run it panics and its lease hands the child to `left`.
/// Either way the step frees the slot before the end, `Clean`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_late_handoff() {
    use crate::supervisor::Pause;
    use crate::supervisor::workspace::common_dir;
    let dir = tempfile::tempdir().unwrap();
    for panics in [false, true] {
        let rig = Rig::with(chaldea_core::adapter::HostOs::Macos, |c| {
            c.git = super::workspace::sleeping_git();
        })
        .await;
        // Running: the probe ends only at its deadline, while git still sleeps.
        rig.clock.resume();
        let created = if panics {
            rig.pauses.arm_panic(Pause::GitAfterCreate)
        } else {
            rig.pauses.arm(Pause::GitAfterCreate)
        };
        let read = rig.pauses.arm(Pause::GitShutdownRead);
        let (sup, cwd) = (rig.sup.clone(), dir.path().to_path_buf());
        let probe =
            std::thread::spawn(move || common_dir(&sup, &cwd, sup.now() + Duration::from_secs(3)));
        created.wait();
        let end = rig.sup.now() + Duration::from_secs(5);
        let sd = tokio::spawn(shutdown(rig.sup.clone(), end));
        read.wait();
        assert!(!rig.sup.git.idle(), "the lease is busy");
        created.go();
        let _ = probe.join();
        read.go();
        let started = Instant::now();
        assert_eq!(sd.await.unwrap(), ParticipantEnd::Clean, "panics {panics}");
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(rig.sup.git.idle());
        assert_eq!(rig.sup.tracker.count(), 0);
        rig.srv.stop().await;
    }
}

/// An op that panics while it owns an open session, the session's kill panicking too: the
/// session's implicit close runs during the unwind and its panic stays inside the drop, so
/// the op's task fails, the token stays counted, and the daemon reaches its shutdown
/// verdict, `LeftRunning`. At the stop's kill submission (before the kill task exists), after
/// the stop's seal, and before a spawn's publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwind_with_an_open_session() {
    use super::support::http::spawn_send;
    use crate::supervisor::Pause;
    use chaldea_pty_runtime::fake::FakeEvent;
    use serde_json::json;
    for at in [
        Pause::KillSubmitted,
        Pause::StopAfterSeal,
        Pause::BeforePublish,
    ] {
        let rig = Rig::new().await;
        rig.clock.resume();
        rig.pty.script_next(FakeScript {
            kill: KillBehavior::Panic,
            ..FakeScript::default()
        });
        let fault = rig.pauses.arm_panic(at);
        let _reply = if at == Pause::BeforePublish {
            let args = json!({ "agent": "codex", "cwd": rig.cwd.path().to_string_lossy() });
            Some(spawn_send(
                &rig.srv,
                &rig.srv
                    .command("spawn", &rig.owner, super::cid(), &args.to_string()),
            ))
        } else {
            let (t, _l) = rig.spawn_codex(None).await;
            let args = json!({ "thread_id": t });
            Some(spawn_send(
                &rig.srv,
                &rig.srv
                    .command("stop", &rig.owner, super::cid(), &args.to_string()),
            ))
        };
        let reached = tokio::task::spawn_blocking(move || {
            fault.wait();
            fault
        })
        .await
        .unwrap();
        let child = rig.pty.take_child().unwrap();
        reached.go();
        rig.until("the session's implicit close ran", || {
            child.log().contains(&FakeEvent::Kill { extra: Vec::new() })
        })
        .await;
        assert!(rig.sup.tracker.count() > 0, "{at:?}");
        assert_eq!(graceful(&rig).await, ParticipantEnd::LeftRunning, "{at:?}");
        child.close_output();
        rig.srv.stop().await;
    }
}
