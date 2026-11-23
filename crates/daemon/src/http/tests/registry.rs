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

// ---- command runners and the closing transition (PR7-CL-20) ---------------------------------

mod runners {
    use std::time::{Duration, Instant};

    use chaldea_core::auth::TokenKind;
    use chaldea_core::command::ReceiptStatus;
    use chaldea_core::ids::CommandId;
    use tokio::io::AsyncWriteExt;

    use super::super::support::http::{Reply, TestServer, connect, parse, read_to_end};
    use super::super::{eventually, shared};
    use crate::http::StoreFatal;
    use crate::store::test_faults::CommitFault;

    fn cid(n: u128) -> CommandId {
        CommandId::from_u128(0xC0DE_2000 + n)
    }

    /// Sends `bytes` and reads to the end; a connection ended without a reply is empty.
    fn spawn_raw(port: u16, bytes: Vec<u8>) -> tokio::task::JoinHandle<Vec<u8>> {
        tokio::spawn(async move {
            let mut s = connect(port).await;
            s.write_all(&bytes).await.unwrap();
            read_to_end(&mut s).await.0
        })
    }

    async fn joined<T>(h: tokio::task::JoinHandle<T>) -> T {
        tokio::time::timeout(Duration::from_secs(20), h)
            .await
            .expect("done within 20 s")
            .unwrap()
    }

    fn reply(bytes: &[u8]) -> Reply {
        parse(bytes, None)
    }

    /// A runner registered just before closing is in the snapshot: it submits after the
    /// registry closed and completes, counted under `exited`. A request on a connection
    /// admitted before closing that reaches its runner's registration after it gets 503
    /// `shutting_down`, with nothing submitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_between_register_and_submit() {
        let (mut srv, probe) = TestServer::with_commands(|_| {}).await;
        let owner = srv.token(TokenKind::Owner, None, None).await;
        let s = shared(&srv);
        let (gate_reached, gate_release) = s.faults.arm_runner_gate();
        let first = spawn_raw(
            srv.port,
            srv.command("test_counter", &owner, cid(1), r#"{"n":1}"#)
                .bytes(),
        );
        tokio::time::timeout(Duration::from_secs(10), gate_reached)
            .await
            .expect("the runner reached its gate")
            .unwrap();
        // The second request is dispatched and waits for its body.
        let second = srv.command("test_counter", &owner, cid(2), r#"{"n":2}"#);
        let polls = s.faults.polls();
        let mut conn = connect(srv.port).await;
        conn.write_all(&second.head()).await.unwrap();
        eventually(Duration::from_secs(10), "the body is awaited", || {
            s.faults.polls() > polls
        })
        .await;
        let before = s.faults.submits();
        let after_closing = async {
            eventually(Duration::from_secs(10), "the registry is closing", || {
                s.registry.is_closing()
            })
            .await;
            let body = &second.bytes()[second.head().len()..];
            conn.write_all(body).await.unwrap();
            let (bytes, _) = read_to_end(&mut conn).await;
            let r = reply(&bytes);
            assert_eq!(r.outcome(), (503, Some("shutting_down".to_owned())));
            assert_eq!(s.faults.submits(), before, "the refused request submitted");
            gate_release.send(()).unwrap();
        };
        let (report, ()) = tokio::join!(
            srv.shutdown_http(Instant::now() + Duration::from_secs(10)),
            after_closing
        );
        assert_eq!(report.exited.runner, 1, "{report:?}");
        assert_eq!(report.abort_requested.total(), 0, "{report:?}");
        assert_eq!(reply(&joined(first).await).status, 200);
        assert!(matches!(
            srv.receipt(cid(1)).unwrap().status,
            ReceiptStatus::Completed(_)
        ));
        assert_eq!(srv.receipt(cid(2)), None);
        assert_eq!(probe.intents(), 1);
        srv.store.shutdown();
    }

    /// The intent job is held in the writer past the drain deadline, so the runner gets an
    /// abort request; the commit then completes with an unknown outcome. Nobody reports it,
    /// and after reopening and the restart's marking the receipt is exactly
    /// `unknown_after_crash`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn commit_fault_after_the_drain_deadline() {
        let (mut srv, probe) = TestServer::with_commands(|_| {}).await;
        let owner = srv.token(TokenKind::Owner, None, None).await;
        let (intent_reached, intent_release) = probe.hold_intent();
        *srv.store.store().faults().commit.lock().unwrap() = Some(CommitFault::IoErrAfterCommit);
        let first = spawn_raw(
            srv.port,
            srv.command("test_slow_effect", &owner, cid(1), r#"{"tag":"x"}"#)
                .bytes(),
        );
        tokio::task::spawn_blocking(move || intent_reached.recv_timeout(Duration::from_secs(10)))
            .await
            .unwrap()
            .expect("the intent is held");
        let report = srv
            .shutdown_http(Instant::now() + Duration::from_millis(300))
            .await;
        assert_eq!(report.abort_requested.runner, 1, "{report:?}");
        assert!(joined(first).await.is_empty(), "a reply after the abort");
        intent_release.send(()).unwrap();
        let mut store = srv.store;
        let store = tokio::task::spawn_blocking(move || {
            // The writer runs the held job to its commit, then exits.
            store.shutdown();
            store.reopen();
            store
                .run(|tx| Ok(tx.receipts_mark_unknown_after_crash()?))
                .unwrap();
            store
        })
        .await
        .unwrap();
        assert_eq!(*srv.sink.0.lock().unwrap(), Vec::<StoreFatal>::new());
        assert_eq!(
            store.read(move |tx| tx.receipt(cid(1))).unwrap().status,
            ReceiptStatus::UnknownAfterCrash
        );
        assert_eq!(probe.effects(), 0);
    }
}
