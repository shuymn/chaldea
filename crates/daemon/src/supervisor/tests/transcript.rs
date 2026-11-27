//! The transcript tailer in the supervisor (C1 D10): one per Claude launch, reading from its
//! binding; the final read after the exit, also at daemon shutdown (with a binding that came
//! after admission closed, a poll begun across the close, and short retries); joined by the
//! shutdown participant before the store shuts down, or reported as left running.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use chaldea_core::adapter::HostOs;
use chaldea_core::ids::{LaunchId, ThreadId};
use serde_json::json;

use super::{Rig, json};
use crate::lifecycle::ParticipantEnd;
use crate::store::tests::support;
use crate::supervisor::Pause;
use crate::supervisor::components::shutdown;
use crate::transcript::{BATCH_LINES, DROPPED_OFF_BLOCKING, FINAL_TRIES, Mode, read_probe};

const LINE: &str =
    "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n";

async fn rig() -> Rig {
    let rig = Rig::with(HostOs::Macos, |c| {
        c.tail_poll = Duration::from_millis(20);
    })
    .await;
    rig.clock.resume();
    rig
}

async fn spawn_claude(rig: &Rig) -> (ThreadId, LaunchId) {
    let cwd = rig.cwd.path().to_string_lossy().into_owned();
    let r = rig
        .cmd("spawn", &json!({ "agent": "claude", "cwd": cwd }))
        .await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let v = json(&r);
    (
        serde_json::from_value(v["thread_id"].clone()).unwrap(),
        serde_json::from_value(v["launch_id"].clone()).unwrap(),
    )
}

fn texts(rig: &Rig, t: ThreadId) -> Vec<String> {
    rig.events(t)
        .into_iter()
        .filter_map(|(k, p)| match k.as_str() {
            "assistant_message" => Some(p["text"]["inline"].as_str().unwrap().to_owned()),
            "raw_unknown" => Some(format!("raw:{}", p["reason"].as_str().unwrap())),
            _ => None,
        })
        .collect()
}

/// A Claude launch's tailer waits from the reservation and reads from the binding; the
/// shutdown participant stops and joins it within its budget, and the store then shuts down
/// with every reader returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starts_at_binding_and_stops_at_shutdown() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    assert_eq!(rig.sup.tailers.launches(), [l]);
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, LINE).unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("the line is stored", || texts(&rig, t) == ["ok"])
        .await;
    let end = rig.sup.now() + Duration::from_secs(2);
    let started = Instant::now();
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    assert!(started.elapsed() < Duration::from_secs(10));
    assert!(rig.sup.tailers.launches().is_empty());
    assert!(!DROPPED_OFF_BLOCKING.lock().unwrap().contains(&l));
    let (_, mut store) = rig.srv.into_store(Duration::from_secs(10)).await;
    let report = store.shutdown();
    assert!(report.readers_returned, "{report:?}");
    assert_eq!(report.readers_abandoned, 0);
}

/// After the launch exits, the final read stores the trailing fragment as `partial_line` and
/// the tailer ends; a Codex launch has none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_final_read_follows_the_exit() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, [LINE, "{\"type\""].concat()).unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("the line is stored", || texts(&rig, t) == ["ok"])
        .await;
    let r = rig.cmd("stop", &json!({ "thread_id": t })).await;
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    rig.until("the fragment is stored", || {
        texts(&rig, t) == ["ok", "raw:partial_line"]
    })
    .await;
    rig.until("the tailer ended", || rig.sup.tailers.launches().is_empty())
        .await;
    let (_t2, _l2) = rig.spawn_codex(None).await;
    assert!(rig.sup.tailers.launches().is_empty());
    rig.srv.stop().await;
}

/// Bind `l` of `t` to a new transcript holding `body`, in a temp directory the caller keeps.
async fn bind(rig: &Rig, t: ThreadId, l: LaunchId, body: &str) -> tempfile::TempDir {
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, body).unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    files
}

/// At daemon shutdown the participant records the exit, then the tailer makes its final
/// read within the budget: the trailing fragment is stored as `partial_line`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_shutdown_makes_the_final_read() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let _files = bind(&rig, t, l, &[LINE, "{\"type\""].concat()).await;
    rig.until("the line is stored", || texts(&rig, t) == ["ok"])
        .await;
    let end = rig.sup.now() + Duration::from_secs(5);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    assert!(rig.launch(l).exit_reason.is_some());
    assert_eq!(texts(&rig, t), ["ok", "raw:partial_line"]);
    assert!(rig.sup.tailers.launches().is_empty());
    rig.srv.stop().await;
}

/// A tailer holding a store reader past the participant's end: the participant answers
/// `LeftRunning` and the tailer is still there; once the read returns, the tailer ends and
/// the store shuts down with every reader returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tailer_holding_a_reader_at_the_end_is_left_running() {
    let rig = rig().await;
    let read = rig.pauses.arm(Pause::TailerRead);
    let (_t, l) = spawn_claude(&rig).await;
    let read = tokio::task::spawn_blocking(move || {
        read.wait();
        read
    })
    .await
    .unwrap();
    // A budget the launch's close fits in, so only the tailer is left.
    let end = rig.sup.now() + Duration::from_secs(2);
    assert_eq!(
        shutdown(rig.sup.clone(), end).await,
        ParticipantEnd::LeftRunning
    );
    assert!(
        rig.sup
            .tracker
            .empty_before(rig.sup.now() + Duration::from_secs(1))
            .await
    );
    assert_eq!(rig.sup.tailers.launches(), [l]);
    read.go();
    rig.until("the tailer ended", || rig.sup.tailers.launches().is_empty())
        .await;
    let (_, mut store) = rig.srv.into_store(Duration::from_secs(10)).await;
    let report = store.shutdown();
    assert!(report.readers_returned, "{report:?}");
    assert_eq!(report.readers_abandoned, 0);
}

/// The binding committed after admission closed and before the final read: the tailer,
/// still waiting for its binding, reads it at the final read and stores the fragment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_binding_after_hold_is_read_at_the_final_read() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, [LINE, "{\"type\""].concat()).unwrap();
    let close = rig.pauses.arm(Pause::ShutdownCloseEnd);
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    let close = tokio::task::spawn_blocking(move || {
        close.wait();
        close
    })
    .await
    .unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    close.go();
    assert_eq!(participant.await.unwrap(), ParticipantEnd::Clean);
    assert_eq!(texts(&rig, t), ["ok", "raw:partial_line"]);
    rig.srv.stop().await;
}

/// A final read that keeps failing (the file is missing) under the default poll period
/// ends within the participant's default 2 s after `FINAL_TRIES` short tries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_final_read_ends_within_the_default_budget() {
    let rig = Rig::with(HostOs::Macos, |_| {}).await;
    rig.clock.resume();
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("missing.jsonl");
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("a poll ran", || polls(&rig) > 0).await;
    let before = polls(&rig);
    let end = rig.sup.now() + Duration::from_secs(2);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    assert!(rig.sup.tailers.launches().is_empty());
    assert_eq!(polls(&rig) - before, FINAL_TRIES);
    rig.srv.stop().await;
}

fn polls(rig: &Rig) -> u32 {
    rig.sup.tailers.polls.load(Ordering::Relaxed)
}

/// A budget too short for a wait and another try: the failing final read (a missing file)
/// ends after its first try instead of waiting past the participant's end (three tries 100 ms
/// apart would need about 200 ms).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_final_retry_is_not_waited_past_the_budget() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("missing.jsonl");
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("a poll ran", || polls(&rig) > 0).await;
    let before = polls(&rig);
    let end = rig.sup.now() + Duration::from_millis(180);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    assert!(rig.sup.tailers.launches().is_empty());
    assert_eq!(polls(&rig) - before, 1);
    rig.srv.stop().await;
}

/// At the final read, failed binding reads and a failed view are counted apart: two failed
/// binding reads, then the binding, then a failed view that is still tried again, so the
/// fragment is stored.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_and_view_failures_are_counted_apart() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, [LINE, "{\"type\""].concat()).unwrap();
    let close = rig.pauses.arm(Pause::ShutdownCloseEnd);
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    let close = tokio::task::spawn_blocking(move || {
        close.wait();
        close
    })
    .await
    .unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.sup
        .tailers
        .read_faults
        .lock()
        .unwrap()
        .extend([true, true, false, true]);
    close.go();
    assert_eq!(participant.await.unwrap(), ParticipantEnd::Clean);
    assert!(rig.sup.tailers.read_faults.lock().unwrap().is_empty());
    assert_eq!(texts(&rig, t), ["ok", "raw:partial_line"]);
    rig.srv.stop().await;
}

/// A poll that read `Run` before admission closed and starts its blocking task after: it
/// reads nothing then, and the line is stored by the final read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_poll_starting_after_hold_waits_for_the_final_read() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, "").unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("a poll ran", || rig.pauses.passed(Pause::TailerPoll))
        .await;
    let poll = rig.pauses.arm(Pause::TailerPoll);
    std::fs::write(&path, LINE).unwrap();
    let poll = tokio::task::spawn_blocking(move || {
        poll.wait();
        poll
    })
    .await
    .unwrap();
    rig.sup.tailers.set(Mode::Hold);
    poll.go();
    tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(300)))
        .await
        .unwrap();
    assert!(texts(&rig, t).is_empty());
    let end = rig.sup.now() + Duration::from_secs(5);
    assert_eq!(shutdown(rig.sup.clone(), end).await, ParticipantEnd::Clean);
    assert_eq!(texts(&rig, t), ["ok"]);
    rig.srv.stop().await;
}

/// At the final read a launch whose exit is not stored ends the tailer after the poll, even
/// when its batch closed with more to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_final_read_without_a_stored_exit_ends_after_one_poll() {
    let rig = rig().await;
    let (t, l) = spawn_claude(&rig).await;
    let files = tempfile::tempdir().unwrap();
    let path = files.path().join("session.jsonl");
    std::fs::write(&path, "").unwrap();
    rig.srv
        .append(vec![support::bound(
            t,
            l,
            "s1",
            Some(&path.to_string_lossy()),
        )])
        .await;
    rig.until("a poll ran", || rig.pauses.passed(Pause::TailerPoll))
        .await;
    let poll = rig.pauses.arm(Pause::TailerPoll);
    // The held poll read `Run`; the next one reads `Final`.
    std::fs::write(&path, LINE.repeat(2 * BATCH_LINES + 10)).unwrap();
    let poll = tokio::task::spawn_blocking(move || {
        poll.wait();
        poll
    })
    .await
    .unwrap();
    rig.sup.tailers.set(Mode::Final);
    poll.go();
    rig.until("the tailer ended", || rig.sup.tailers.launches().is_empty())
        .await;
    assert_eq!(texts(&rig, t).len(), 2 * BATCH_LINES);
    rig.srv.stop().await;
}

/// At the final read a tailer still waiting for its binding whose binding reads all fail
/// records the continuity as unverified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spent_binding_reads_at_the_final_read_are_unverified() {
    let rig = rig().await;
    let (_t, _l) = spawn_claude(&rig).await;
    let close = rig.pauses.arm(Pause::ShutdownCloseEnd);
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    let close = tokio::task::spawn_blocking(move || {
        close.wait();
        close
    })
    .await
    .unwrap();
    rig.sup
        .tailers
        .read_faults
        .lock()
        .unwrap()
        .extend([true; FINAL_TRIES as usize]);
    close.go();
    assert_eq!(participant.await.unwrap(), ParticipantEnd::Clean);
    assert_eq!(
        rig.sup.tailers.unbound_unverified.load(Ordering::Relaxed),
        1
    );
    rig.srv.stop().await;
}

/// Past the participant's end a tailer read borrows no reader: it fails before the pool,
/// which lends a reader free at once without checking the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_reader_is_borrowed_past_the_participants_end() {
    let rig = rig().await;
    let past = rig.sup.now() - Duration::from_millis(1);
    let _ = rig.sup.window.set(crate::supervisor::Window {
        close_end: past,
        end: past,
    });
    let before = rig.sup.tailers.reads.load(Ordering::Relaxed);
    let sup = rig.sup.clone();
    let r = tokio::task::spawn_blocking(move || read_probe(&sup))
        .await
        .unwrap();
    assert!(r.is_err(), "{r:?}");
    assert_eq!(rig.sup.tailers.reads.load(Ordering::Relaxed), before);
    rig.srv.stop().await;
}

/// The participant cut at its end (dropped, as the outer deadline does) before it joined the
/// tailers: the launches whose tailers are left are logged as unverified.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_participant_cut_before_the_final_reads_logs_them_unverified() {
    let rig = rig().await;
    let (_t, _l) = spawn_claude(&rig).await;
    let close = rig.pauses.arm(Pause::ShutdownCloseEnd);
    let end = rig.sup.now() + Duration::from_secs(5);
    let participant = tokio::spawn(shutdown(rig.sup.clone(), end));
    let close = tokio::task::spawn_blocking(move || {
        close.wait();
        close
    })
    .await
    .unwrap();
    participant.abort();
    assert!(participant.await.unwrap_err().is_cancelled());
    assert_eq!(
        rig.sup.tailers.abandoned_unverified.load(Ordering::Relaxed),
        1
    );
    close.go();
    rig.srv.stop().await;
}
