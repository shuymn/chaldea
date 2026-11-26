//! The restart prober (`docs/design/supervisor-recovery.md` §2.3; PR10a2-CL-3 to CL-6, CL-8's
//! race): a scripted `SupervisorConfig.kill_recorded` over stores a crashed daemon left.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use chaldea_core::adapter::HostOs;
use chaldea_core::ids::{LaunchId, ThreadId};
use chaldea_core::payload::ExitReason;
use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::orphan::{Fate, OrphanReport};
use serde_json::{Value, json};

use super::support::TestStore;
use super::{Rig, json as body};
use crate::lifecycle::ParticipantEnd;
use crate::supervisor::components::shutdown;
use crate::supervisor::{
    KillRecordedFn, PROBE_BUDGET, PROBE_JOIN_GRACE, PROBE_SLOTS, Pause, prober,
};

/// A gate a blocking `kill_recorded` call parks on, after it recorded its call (its "first
/// signal").
#[derive(Default)]
struct Park {
    state: Mutex<(usize, bool)>,
    cv: Condvar,
}

impl Park {
    fn enter(&self) {
        let mut g = self.state.lock().unwrap();
        g.0 += 1;
        self.cv.notify_all();
        let _ = self
            .cv
            .wait_timeout_while(g, Duration::from_secs(60), |s| !s.1)
            .unwrap();
    }

    /// Waits (20 s at most) until `n` calls are parked.
    fn reached(&self, n: usize) {
        let g = self.state.lock().unwrap();
        let (g, t) = self
            .cv
            .wait_timeout_while(g, Duration::from_secs(20), |s| s.0 < n)
            .unwrap();
        assert!(!t.timed_out(), "{} of {n} calls parked", g.0);
    }

    fn open(&self) {
        self.state.lock().unwrap().1 = true;
        self.cv.notify_all();
    }
}

/// A scripted `kill_recorded`: records each call's identities, answers each with `fate`,
/// parks while `park` is armed, panics once when `panic` is set.
struct Script {
    calls: Mutex<Vec<Vec<ProcessIdentity>>>,
    fate: Mutex<Fate>,
    parking: AtomicBool,
    park: Park,
    panic: AtomicBool,
}

impl Script {
    fn new(fate: Fate) -> Arc<Script> {
        Arc::new(Script {
            calls: Mutex::new(Vec::new()),
            fate: Mutex::new(fate),
            parking: AtomicBool::new(false),
            park: Park::default(),
            panic: AtomicBool::new(false),
        })
    }

    fn parking(fate: Fate) -> Arc<Script> {
        let s = Script::new(fate);
        s.parking.store(true, Ordering::SeqCst);
        s
    }

    fn fun(self: &Arc<Self>) -> KillRecordedFn {
        let s = self.clone();
        Arc::new(move |ids, _deadline| {
            s.calls.lock().unwrap().push(ids.to_vec());
            if s.panic.swap(false, Ordering::SeqCst) {
                panic!("an injected kill_recorded panic");
            }
            if s.parking.load(Ordering::SeqCst) {
                s.park.enter();
            }
            let f = *s.fate.lock().unwrap();
            OrphanReport {
                fates: ids.iter().map(|id| (id.clone(), f)).collect(),
            }
        })
    }

    fn calls(&self) -> Vec<Vec<ProcessIdentity>> {
        self.calls.lock().unwrap().clone()
    }
}

/// A store whose `n` running launches (one thread each) a crashed daemon left, recorded
/// `daemon_restart` by the restart reconcilers.
async fn crashed(n: usize) -> (TestStore, Vec<(ThreadId, LaunchId)>) {
    let rig = Rig::new().await;
    let mut ids = Vec::new();
    let mut children = Vec::new();
    for _ in 0..n {
        ids.push(rig.spawn_codex(None).await);
        children.push(rig.pty.take_child().unwrap());
    }
    let (_, store) = rig.srv.into_store(Duration::from_secs(10)).await;
    drop(rig.sup);
    drop(children);
    super::restart::restart_all(&store);
    (store, ids)
}

/// A rig over `store` probing with `script`.
async fn probing(store: TestStore, host: HostOs, script: &Arc<Script>) -> Rig {
    let f = script.fun();
    Rig::on(store, host, move |c| c.kill_recorded = f).await
}

/// The `probe` evidence payloads of thread `t`.
fn evidence(rig: &Rig, t: ThreadId) -> Vec<Value> {
    rig.events(t)
        .into_iter()
        .filter(|(k, p)| k == "launch_evidence" && p["source"] == "probe")
        .map(|(_, p)| p)
        .collect()
}

/// The leader identity a launch row records.
fn leader(rig: &Rig, l: LaunchId) -> ProcessIdentity {
    let row = rig.launch(l);
    ProcessIdentity {
        pid: row.pid.unwrap(),
        start_time: row.start_time.unwrap_or_default(),
        group: row.pgid_or_job.unwrap_or_default(),
    }
}

/// PR10a2-CL-3 `candidates`: the launches exited `daemon_restart` by an earlier start and by
/// this one, death unconfirmed, are probed once each, with this run's `instance` as
/// `probe_run`; a confirmed `daemon_restart`, an `unknown_after_crash`, a `stopped`
/// unconfirmed launch, and a live launch of this run are not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn candidates() {
    use chaldea_pty_runtime::fake::{FakeScript, KillBehavior};
    // Run 0: A and K running, S stopped with its death unconfirmed.
    let rig = Rig::with(HostOs::Macos, |c| {
        c.kill_budget = Duration::from_millis(100);
    })
    .await;
    rig.clock.resume();
    let (ta, la) = rig.spawn_codex(None).await;
    let (tk, lk) = rig.spawn_codex(None).await;
    rig.pty.script_next(FakeScript {
        kill: KillBehavior::Unconfirmed,
        ..FakeScript::default()
    });
    let (ts, ls) = rig.spawn_codex(None).await;
    let r = rig.cmd("stop", &json!({ "thread_id": ts })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    assert!(!rig.launch(ls).death_confirmed);
    // F and G: failed starts with their cleanup unresolved; F's child has a usable recorded
    // identity, G's has no start time.
    let child = |start: &str| chaldea_pty_runtime::ProcessIdentity {
        pid: if start.is_empty() { 92 } else { 91 },
        start_time: start.to_owned(),
        group: "g".to_owned(),
    };
    let failed = |id: chaldea_pty_runtime::ProcessIdentity| FakeScript {
        fail: Some(chaldea_pty_runtime::SpawnError::StartedThenFailed {
            stage: "s",
            error: std::io::Error::other("x"),
            identity: Some(id),
            cleanup: chaldea_pty_runtime::Cleanup::Unresolved {
                kill: None,
                close: None,
                drain_abandoned: false,
            },
        }),
        ..FakeScript::default()
    };
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let mut spawn_failed = Vec::new();
    for start in ["s91", ""] {
        rig.pty.script_next(failed(child(start)));
        let r = rig
            .cmd("spawn", &json!({ "agent": "codex", "cwd": cwd }))
            .await;
        assert_eq!(body(&r)["outcome"], "spawn_failed");
        spawn_failed
            .push(serde_json::from_value::<ThreadId>(body(&r)["thread_id"].clone()).unwrap());
    }
    let (tf, tg) = (spawn_failed[0], spawn_failed[1]);
    let (_, store) = rig.srv.into_store(Duration::from_secs(10)).await;
    drop(rig.sup);
    // Start 1 reconciles A and K; its prober never runs (a crash). K is confirmed by the
    // owner; B runs; U is held between its reservation and its spawn.
    super::restart::restart_all(&store);
    let rig = Rig::on(store, HostOs::Macos, |_| {}).await;
    let r = rig
        .cmd(
            "stop",
            &json!({ "thread_id": tk, "expected_launch_id": lk, "assume_dead": true }),
        )
        .await;
    assert_eq!(body(&r)["outcome"], "asserted");
    let (tb, lb) = rig.spawn_codex(None).await;
    let held = rig.pauses.arm(Pause::BeforeSpawn);
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let pending = super::support::http::spawn_send(
        &rig.srv,
        &rig.srv.command(
            "spawn",
            &rig.owner,
            super::cid(),
            &json!({ "agent": "codex", "cwd": cwd }).to_string(),
        ),
    );
    held.wait();
    // Start 2's reconcilers: B `daemon_restart`, U `unknown_after_crash`.
    super::restart::restart_all(&rig.srv.store);
    held.go();
    let _ = pending.await;
    let tu = rig
        .srv
        .store
        .read(|r| {
            Ok(r.threads()?
                .into_iter()
                .map(|t| t.thread_id)
                .collect::<Vec<_>>())
        })
        .into_iter()
        .find(|t| ![ta, tk, ts, tb, tf, tg].contains(t))
        .unwrap();
    let lu = rig.thread(tu).current_launch_id.unwrap();
    assert_eq!(
        rig.launch(lu).exit_reason,
        Some(ExitReason::UnknownAfterCrash)
    );
    let (_, store) = rig.srv.into_store(Duration::from_secs(10)).await;
    drop(rig.sup);
    // Start 2 probes, with a live launch L of its own.
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    let (tl, _ll) = rig.spawn_codex(None).await;
    prober::run(rig.sup.clone()).await;
    rig.until("three probes recorded", || {
        [ta, tb, tf]
            .iter()
            .map(|t| evidence(&rig, *t).len())
            .sum::<usize>()
            == 3
    })
    .await;
    let mut calls = script.calls();
    let mut want = vec![
        vec![leader(&rig, la)],
        vec![leader(&rig, lb)],
        vec![child("s91")],
    ];
    calls.sort_by_key(|c| (c[0].pid, c[0].start_time.clone()));
    want.sort_by_key(|c| (c[0].pid, c[0].start_time.clone()));
    assert_eq!(calls, want);
    for t in [ta, tb, tf] {
        let ev = evidence(&rig, t);
        assert_eq!(ev.len(), 1, "{t}");
        assert_eq!(ev[0]["probe_run"], json!(rig.sup.instance), "{t}");
        assert_eq!(ev[0]["source"], "probe");
    }
    for t in [tk, ts, tu, tl, tg] {
        assert!(evidence(&rig, t).is_empty(), "{t}");
    }
    rig.srv.stop().await;
}

/// PR10a2-CL-6 `never_confirms`: every identity `gone`: the launch stays unconfirmed,
/// `resume` and `close` answer `death_unconfirmed`, and the rows are unchanged but for the
/// event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn never_confirms() {
    let (store, ids) = crashed(1).await;
    let (t, l) = ids[0];
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    let (thread0, launch0) = (rig.thread(t), rig.launch(l));
    prober::run(rig.sup.clone()).await;
    rig.until("the probe recorded", || evidence(&rig, t).len() == 1)
        .await;
    let ev = &evidence(&rig, t)[0];
    assert_eq!(ev["identities"][0]["observed"], "gone");
    assert_eq!(ev["unchecked"], json!(["unrecorded_session_members"]));
    assert_eq!(rig.launch(l), launch0);
    assert_eq!(rig.thread(t).state, thread0.state);
    for (name, args) in [
        ("resume", json!({ "thread_id": t, "expected_launch_id": l })),
        ("close", json!({ "thread_id": t, "expected_launch_id": l })),
    ] {
        let r = rig.cmd(name, &args).await;
        assert_eq!(
            (r.status, body(&r)["code"].clone()),
            (409, json!("death_unconfirmed")),
            "{name}"
        );
    }
    rig.srv.stop().await;
}

/// PR10a2-CL-6 `shutdown`: admission closed at `prober_start`: no call; closed while the
/// prober waits for a slot: no further admit and no call; a task held at `probe_queued` while
/// admission closes: no call, no evidence, the participant `Clean`; a task held inside its
/// start under admission's mutex while shutdown begins: it signals and records; a `kill_recorded` parked
/// inside its signal sequence past the participant's `end`: `LeftRunning`, and `Clean` when
/// released before `end`; a probe that panicked earlier: a later shutdown `Clean`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_boundary() {
    // At `prober_start`.
    let (store, ids) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    let held = rig.pauses.arm(Pause::ProberStart);
    let run = tokio::spawn(prober::run(rig.sup.clone()));
    held.wait();
    rig.sup.tracker.close_admission();
    held.go();
    assert!(run.await.unwrap().is_empty());
    assert!(script.calls().is_empty());
    assert_eq!(rig.sup.tracker.count(), 0);
    rig.srv.stop().await;

    // Waiting for a slot.
    let (store, ids2) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    let all = rig
        .sup
        .probe_slots
        .clone()
        .acquire_many_owned(u32::try_from(PROBE_SLOTS).unwrap())
        .await
        .unwrap();
    let run = tokio::spawn(prober::run(rig.sup.clone()));
    // The read has returned and the prober waits on the semaphore: no call yet.
    tokio::task::yield_now().await;
    rig.sup.tracker.close_admission();
    drop(all);
    assert!(run.await.unwrap().is_empty());
    assert!(script.calls().is_empty());
    assert_eq!(rig.sup.tracker.count(), 0);
    assert!(evidence(&rig, ids2[0].0).is_empty());
    rig.srv.stop().await;

    // Held at `probe_queued` while admission closes.
    let (store, ids3) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    let queued = rig.pauses.arm(Pause::ProbeQueued);
    let records = prober::run(rig.sup.clone()).await;
    queued.wait();
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    rig.until("admission closed", || !rig.sup.tracker.accepting())
        .await;
    queued.go();
    assert_eq!(participant.await.unwrap(), ParticipantEnd::Clean);
    for r in records {
        r.await.unwrap();
    }
    assert!(script.calls().is_empty());
    assert!(evidence(&rig, ids3[0].0).is_empty());
    rig.srv.stop().await;

    // Held at `probe_queued` while admission closes and past the join's cutoff, the store
    // still open: the record step reads that the probe never started and records nothing,
    // before and after the task is released.
    let (store, ids4) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    let queued = rig.pauses.arm(Pause::ProbeQueued);
    let records = prober::run(rig.sup.clone()).await;
    queued.wait();
    rig.sup.tracker.close_admission();
    // The record tasks end at the join's cutoff (`PROBE_BUDGET + PROBE_JOIN_GRACE`).
    for r in records {
        r.await.unwrap();
    }
    assert!(evidence(&rig, ids4[0].0).is_empty());
    queued.go();
    rig.until("the queued task returned", || rig.sup.tracker.count() == 0)
        .await;
    assert!(script.calls().is_empty());
    assert!(evidence(&rig, ids4[0].0).is_empty());
    rig.srv.stop().await;

    // Held at `probe_queued` through the participant's end: its token is still held.
    let (store, _) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    let queued = rig.pauses.arm(Pause::ProbeQueued);
    prober::run(rig.sup.clone()).await;
    queued.wait();
    let end = rig.sup.now() + Duration::from_millis(300);
    assert_eq!(
        shutdown(rig.sup.clone(), end).await,
        ParticipantEnd::LeftRunning
    );
    queued.go();
    assert!(script.calls().is_empty());
    rig.srv.stop().await;

    // Held inside its start, under admission's mutex: `close_admission` waits for it, so the
    // probe started before admission closed and signals, and records its evidence.
    let (store, ids6) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    let starting = rig.pauses.arm(Pause::ProbeStarting);
    let records = prober::run(rig.sup.clone()).await;
    starting.wait();
    assert!(
        rig.sup.tracker.mutex_held(),
        "the start mark is set under admission's mutex"
    );
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    starting.go();
    assert_eq!(participant.await.unwrap(), ParticipantEnd::Clean);
    for r in records {
        r.await.unwrap();
    }
    assert_eq!(script.calls().len(), 1);
    assert_eq!(evidence(&rig, ids6[0].0).len(), 1);
    rig.srv.stop().await;

    // Parked inside the signal sequence: past `end`, then released before `end`.
    for (release_first, want) in [
        (false, ParticipantEnd::LeftRunning),
        (true, ParticipantEnd::Clean),
    ] {
        let (store, _) = crashed(1).await;
        let script = Script::parking(Fate::Gone);
        let rig = probing(store, HostOs::Macos, &script).await;
        rig.clock.resume();
        prober::run(rig.sup.clone()).await;
        script.park.reached(1);
        let end = rig.sup.now() + Duration::from_millis(if release_first { 5_000 } else { 300 });
        let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
        if release_first {
            rig.until("admission closed", || !rig.sup.tracker.accepting())
                .await;
            script.park.open();
        }
        assert_eq!(participant.await.unwrap(), want, "{release_first}");
        script.park.open();
        rig.srv.stop().await;
    }

    // A probe that panicked earlier.
    let (store, ids5) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    script.panic.store(true, Ordering::SeqCst);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    prober::run(rig.sup.clone()).await;
    rig.until("the panicked probe recorded", || {
        evidence(&rig, ids5[0].0).len() == 1
    })
    .await;
    let end = rig.sup.now() + Duration::from_secs(2);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    let _ = ids;
    rig.srv.stop().await;
}

/// PR10a2-CL-5 `bounded`: six candidates, every probe parked: four run at once though all
/// four joins elapsed, each recorded `not_read` with `result_not_received`; the fifth starts
/// only once a parked task ends. A join made `Late` by the clock: its fates are recorded. A
/// `kill_recorded` that panics: `not_read`, `result_not_received`, and its slot returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn bounded() {
    let (store, ids) = crashed(6).await;
    let script = Script::parking(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    // Running: the joins' cutoff passes in real time; the parked tasks never return by it.
    // Every dispatch is after `dispatched`, so no join's cutoff is before `elapsed_by`.
    rig.clock.resume();
    let dispatched = std::time::Instant::now();
    let run = tokio::spawn(prober::run(rig.sup.clone()));
    script.park.reached(PROBE_SLOTS);
    let elapsed_by = dispatched + PROBE_BUDGET + PROBE_JOIN_GRACE;
    let recorded = || {
        ids.iter()
            .filter(|(t, _)| !evidence(&rig, *t).is_empty())
            .count()
    };
    support_wait(&rig, "four elapsed joins recorded", || recorded() == 4).await;
    assert!(std::time::Instant::now() >= elapsed_by);
    assert_eq!(script.calls().len(), PROBE_SLOTS, "no fifth probe");
    assert_eq!(rig.sup.probe_slots.available_permits(), 0);
    for (t, _) in &ids {
        for ev in evidence(&rig, *t) {
            assert_eq!(ev["identities"][0]["observed"], "not_read");
            assert_eq!(
                ev["unchecked"],
                json!(["unrecorded_session_members", "result_not_received"])
            );
        }
    }
    // One parked task ends: the fifth starts; then all.
    script.parking.store(false, Ordering::SeqCst);
    script.park.open();
    drop(run.await.unwrap());
    rig.until("six probes", || script.calls().len() == 6).await;
    rig.until("six recorded", || recorded() == 6).await;
    rig.until("every slot back", || {
        rig.sup.probe_slots.available_permits() == PROBE_SLOTS
    })
    .await;
    rig.srv.stop().await;

    // A late join: recorded as observed.
    let (store, ids) = crashed(1).await;
    let script = Script::parking(Fate::Replaced);
    let rig = probing(store, HostOs::Windows, &script).await;
    prober::run(rig.sup.clone()).await;
    script.park.reached(1);
    rig.clock.advance(PROBE_BUDGET + PROBE_JOIN_GRACE);
    script.park.open();
    rig.until("the late probe recorded", || {
        evidence(&rig, ids[0].0).len() == 1
    })
    .await;
    let ev = &evidence(&rig, ids[0].0)[0];
    assert_eq!(ev["identities"][0]["observed"], "replaced");
    assert_eq!(ev["unchecked"], json!(["unrecorded_job_and_outside_job"]));
    rig.srv.stop().await;

    // A panic.
    let (store, ids) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    script.panic.store(true, Ordering::SeqCst);
    let rig = probing(store, HostOs::Macos, &script).await;
    prober::run(rig.sup.clone()).await;
    rig.until("the panicked probe recorded", || {
        evidence(&rig, ids[0].0).len() == 1
    })
    .await;
    let ev = &evidence(&rig, ids[0].0)[0];
    assert_eq!(ev["identities"][0]["observed"], "not_read");
    assert_eq!(
        ev["unchecked"],
        json!(["unrecorded_session_members", "result_not_received"])
    );
    rig.until("the slot returned", || {
        rig.sup.probe_slots.available_permits() == PROBE_SLOTS
    })
    .await;
    assert_eq!(script.calls().len(), 1);
    rig.srv.stop().await;
}

/// [`Rig::until`] with a bound long enough for a real-time join cutoff.
async fn support_wait(_rig: &Rig, what: &str, cond: impl FnMut() -> bool) {
    super::support::http::eventually(Duration::from_secs(30), what, cond).await;
}

/// PR10a2-CL-5 `record_retry`: the first attempt rolled back `Busy`: one event with the
/// probe's key, one `kill_recorded` call. A writer paused on an admitted evidence job: the
/// slots are free and the tracker count excludes the waiter, so the participant answers
/// `Clean`, and the other probes run. `CommitUnknown` on the evidence job: reported to the
/// lifecycle sink, the record ends `Unrecorded`, and no second probe.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn record_retry() {
    use crate::store::test_faults::CommitFault;
    // Busy once.
    let (store, ids) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::BusyBeforeCommit);
    prober::run(rig.sup.clone()).await;
    rig.until("recorded after the retry", || {
        evidence(&rig, ids[0].0).len() == 1
    })
    .await;
    assert_eq!(script.calls().len(), 1);
    let t0 = ids[0].0;
    let keys: Vec<String> = rig.srv.store.read(move |r| {
        Ok(
            r.events_after(chaldea_core::ids::StreamId::Thread(t0), 0, 10_000)?
                .events
                .into_iter()
                .filter(|e| e.kind.as_str() == "launch_evidence")
                .map(|e| e.idempotency_key.0)
                .collect(),
        )
    });
    assert_eq!(keys.len(), 1);
    let attempts = prober::attempts::of(ids[0].1);
    assert_eq!(attempts.len(), 2, "the rolled-back attempt and its retry");
    assert_eq!(attempts[0], attempts[1], "equal keys and payloads");
    assert_eq!(attempts[1].0, keys[0]);
    let prefix = format!("launch:{}:evidence:probe:", ids[0].1);
    assert!(
        keys[0].starts_with(&prefix) && uuid::Uuid::parse_str(&keys[0][prefix.len()..]).is_ok(),
        "{keys:?}"
    );
    rig.srv.stop().await;

    // A paused writer.
    let (store, ids) = crashed(5).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    let writer = rig.srv.hold_writer().await;
    prober::run(rig.sup.clone()).await;
    rig.until("every probe ran", || script.calls().len() == 5)
        .await;
    rig.until("every slot free", || {
        rig.sup.probe_slots.available_permits() == PROBE_SLOTS
    })
    .await;
    assert_eq!(rig.sup.tracker.count(), 0);
    let end = rig.sup.now() + Duration::from_secs(2);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    drop(writer);
    rig.until("every evidence recorded", || {
        ids.iter().all(|(t, _)| evidence(&rig, *t).len() == 1)
    })
    .await;
    rig.srv.stop().await;

    // CommitUnknown.
    let (store, ids) = crashed(1).await;
    let script = Script::new(Fate::Gone);
    let rig = probing(store, HostOs::Macos, &script).await;
    rig.clock.resume();
    *rig.srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
    prober::run(rig.sup.clone()).await;
    rig.until("CommitUnknown reported", || {
        rig.sink
            .0
            .lock()
            .unwrap()
            .contains(&crate::http::StoreFatal::CommitUnknown)
    })
    .await;
    // The probe ended (its slot and token returned) with one call; nothing probes again.
    rig.until("the probe ended", || {
        rig.sup.probe_slots.available_permits() == PROBE_SLOTS && rig.sup.tracker.count() == 0
    })
    .await;
    assert_eq!(script.calls().len(), 1);
    let _ = ids;
    rig.srv.stop().await;
}

/// PR10a2-CL-4 `crash_points`: a supervisor over the same store started again after a crash
/// with `kill_recorded` parked inside its signal sequence, with the evidence job rolled back
/// before its commit, and after the commit: the new prober probes again each time; after the
/// commit there are two `probe` events with different keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crash_points() {
    use crate::store::test_faults::CommitFault;
    for point in ["in_signal_sequence", "before_commit", "after_commit"] {
        let (store, ids) = crashed(1).await;
        let (t, _) = ids[0];
        let first = if point == "in_signal_sequence" {
            Script::parking(Fate::Gone)
        } else {
            Script::new(Fate::Gone)
        };
        let rig = probing(store, HostOs::Macos, &first).await;
        if point == "before_commit" {
            // The commit fails `Busy` and the retry's backoff never ends on the stopped
            // clock: the store keeps no evidence, as after a crash before the commit.
            *rig.srv.store.store().faults().commit.lock().unwrap() =
                Some(CommitFault::BusyBeforeCommit);
        }
        prober::run(rig.sup.clone()).await;
        match point {
            "in_signal_sequence" => first.park.reached(1),
            "before_commit" => {
                rig.until("the first attempt rolled back", || {
                    rig.srv
                        .store
                        .store()
                        .faults()
                        .commit
                        .lock()
                        .unwrap()
                        .is_none()
                })
                .await;
            }
            _ => {
                rig.until("committed", || evidence(&rig, t).len() == 1)
                    .await;
            }
        }
        let before = evidence(&rig, t).len();
        // The crash: this daemon is abandoned; the next one starts over the same store.
        let (_, store) = rig.srv.into_store(Duration::from_secs(10)).await;
        let next = Script::new(Fate::Gone);
        let rig2 = probing(store, HostOs::Macos, &next).await;
        prober::run(rig2.sup.clone()).await;
        rig2.until("probed again", || evidence(&rig2, t).len() == before + 1)
            .await;
        assert_eq!(next.calls().len(), 1, "{point}");
        let ev = evidence(&rig2, t);
        assert_eq!(
            ev.len(),
            usize::from(point == "after_commit") + 1,
            "{point}"
        );
        if point == "after_commit" {
            assert_ne!(ev[0]["probe_run"], ev[1]["probe_run"]);
            let keys: Vec<String> = rig2.srv.store.read(move |r| {
                Ok(
                    r.events_after(chaldea_core::ids::StreamId::Thread(t), 0, 10_000)?
                        .events
                        .into_iter()
                        .filter(|e| e.kind.as_str() == "launch_evidence")
                        .map(|e| e.idempotency_key.0)
                        .collect(),
                )
            });
            assert_eq!(keys.len(), 2);
            assert_ne!(keys[0], keys[1]);
        }
        first.park.open();
        rig2.srv.stop().await;
    }
}

/// PR10a2-CL-8 `assertion_race`: the owner's assertion committed while the probe is held at
/// `probe_queued`, and while `kill_recorded` is parked inside its signal sequence: one
/// `launch_death_confirmed`, the probe's evidence after it, the launch still confirmed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn assertion_race() {
    for at in ["probe_queued", "in_signal_sequence"] {
        let (store, ids) = crashed(1).await;
        let (t, l) = ids[0];
        let script = if at == "probe_queued" {
            Script::new(Fate::Gone)
        } else {
            Script::parking(Fate::Gone)
        };
        let rig = probing(store, HostOs::Macos, &script).await;
        let queued = (at == "probe_queued").then(|| rig.pauses.arm(Pause::ProbeQueued));
        prober::run(rig.sup.clone()).await;
        match &queued {
            Some(q) => q.wait(),
            None => script.park.reached(1),
        }
        let r = rig
            .cmd(
                "stop",
                &json!({ "thread_id": t, "expected_launch_id": l, "assume_dead": true }),
            )
            .await;
        assert_eq!(body(&r)["outcome"], "asserted", "{at}");
        if let Some(q) = queued {
            q.go();
        }
        script.park.open();
        rig.until("the probe recorded", || evidence(&rig, t).len() == 1)
            .await;
        let kinds = rig.kinds(t);
        let confirmed: Vec<usize> = kinds
            .iter()
            .enumerate()
            .filter(|(_, k)| *k == "launch_death_confirmed")
            .map(|(i, _)| i)
            .collect();
        let probe = kinds.iter().position(|k| k == "launch_evidence").unwrap();
        assert_eq!(confirmed.len(), 1, "{at}: {kinds:?}");
        assert!(confirmed[0] < probe, "{at}: {kinds:?}");
        let row = rig.launch(l);
        assert!(row.death_confirmed, "{at}");
        assert_eq!(row.death_method.as_deref(), Some("owner_asserted"));
        rig.srv.stop().await;
    }
}
