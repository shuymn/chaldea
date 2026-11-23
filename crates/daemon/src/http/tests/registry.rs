//! `TaskRegistry` (design §2.2; PR7-CL-28 registry cases).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use super::eventually;
use crate::http::registry::{Closing, KindCounts, TaskKind, TaskRegistry};

fn arm(slot: &Mutex<Option<Box<dyn FnOnce() + Send>>>, f: impl FnOnce() + Send + 'static) {
    *slot.lock().unwrap() = Some(Box::new(f));
}

/// The task's first action observes its own entry: the spawner is held right after the
/// spawn until the task has acted, so an entry inserted after the spawn would be missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn entry_precedes_first_action() {
    let reg = TaskRegistry::new();
    let (seen_tx, seen_rx) = mpsc::channel::<usize>();
    let (fwd_tx, fwd_rx) = mpsc::channel::<usize>();
    arm(&reg.hooks().after_spawn, move || {
        let n = seen_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the task ran");
        fwd_tx.send(n).unwrap();
    });
    let r = reg.clone();
    reg.spawn(TaskKind::Write, async move {
        seen_tx.send(r.len()).unwrap();
    })
    .unwrap();
    assert_eq!(
        fwd_rx.recv().unwrap(),
        1,
        "the task did not see its own entry"
    );
}

/// Shutdown aborts an entry between its insert and its spawn: the task ends at its first
/// poll without running its future, and its entry is removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_before_spawn_ends_at_first_poll() {
    let reg = TaskRegistry::new();
    let r = reg.clone();
    let counted: Arc<Mutex<Option<(KindCounts, KindCounts)>>> = Arc::default();
    let c = counted.clone();
    arm(&reg.hooks().between_insert_and_spawn, move || {
        let snapshot = r.close();
        let aborted = r.abort_remaining();
        *c.lock().unwrap() = Some((snapshot, aborted));
    });
    let ran = Arc::new(AtomicBool::new(false));
    let ran2 = ran.clone();
    reg.spawn(TaskKind::Runner, async move {
        ran2.store(true, Ordering::SeqCst);
    })
    .unwrap();
    assert!(
        reg.wait_empty_until(Instant::now() + Duration::from_secs(10))
            .await
    );
    assert!(
        !ran.load(Ordering::SeqCst),
        "the aborted task ran its future"
    );
    let (snapshot, aborted) = counted.lock().unwrap().take().unwrap();
    assert_eq!(snapshot.runner, 1);
    assert_eq!(aborted.runner, 1);
}

struct OnDrop(Arc<Mutex<Option<std::thread::ThreadId>>>);

impl Drop for OnDrop {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = Some(std::thread::current().id());
    }
}

/// On a runtime that has shut down, `tokio::spawn` drops the future inline: the entry is
/// removed on the registering thread and nothing blocks.
#[test]
fn inline_drop_removes_entry() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    rt.shutdown_background();
    let _ctx = handle.enter();
    let reg = TaskRegistry::new();
    let dropped = Arc::new(Mutex::new(None));
    let guard = OnDrop(dropped.clone());
    let polled = Arc::new(AtomicBool::new(false));
    let p = polled.clone();
    reg.spawn(TaskKind::Connection, async move {
        p.store(true, Ordering::SeqCst);
        drop(guard);
    })
    .unwrap();
    assert_eq!(reg.len(), 0, "the entry outlived the dropped future");
    assert_eq!(*dropped.lock().unwrap(), Some(std::thread::current().id()));
    assert!(!polled.load(Ordering::SeqCst));
}

/// The last task exits after shutdown's emptiness check and before its wait: the stored
/// permit makes the wait return at once, not at the deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn last_exit_between_check_and_wait() {
    let reg = TaskRegistry::new();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    reg.spawn(TaskKind::Write, async move {
        let _ = go_rx.await;
    })
    .unwrap();
    reg.close();
    let r = reg.clone();
    arm(&reg.hooks().after_empty_check, move || {
        go_tx.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        while !r.is_empty() {
            assert!(Instant::now() < until, "the task did not exit");
            std::thread::yield_now();
        }
    });
    let started = Instant::now();
    assert!(
        reg.wait_empty_until(Instant::now() + Duration::from_secs(20))
            .await
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait missed the last exit: {:?}",
        started.elapsed()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finished_tasks_leave() {
    let reg = TaskRegistry::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for i in 0..200 {
        let tx = tx.clone();
        reg.spawn(TaskKind::Write, async move {
            tx.send(i).unwrap();
        })
        .unwrap();
    }
    for _ in 0..200 {
        rx.recv().await.unwrap();
    }
    let r = reg.clone();
    eventually(Duration::from_secs(10), "finished tasks leave", move || {
        r.is_empty()
    })
    .await;
}

/// Registration fails from the moment the registry closes; the future is dropped unpolled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_refuses() {
    let reg = TaskRegistry::new();
    let (hold_tx, hold_rx) = tokio::sync::oneshot::channel::<()>();
    reg.spawn(TaskKind::Socket, async move {
        let _ = hold_rx.await;
    })
    .unwrap();
    let snapshot = reg.close();
    assert_eq!(snapshot.socket, 1);
    assert!(reg.is_closing());
    let polled = Arc::new(AtomicBool::new(false));
    let p = polled.clone();
    let r = reg.spawn(TaskKind::Connection, async move {
        p.store(true, Ordering::SeqCst);
    });
    assert_eq!(r, Err(Closing));
    assert_eq!(reg.len(), 1);
    drop(hold_tx);
    assert!(
        reg.wait_empty_until(Instant::now() + Duration::from_secs(10))
            .await
    );
    assert!(!polled.load(Ordering::SeqCst));
}
