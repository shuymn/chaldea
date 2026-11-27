//! The session handle: a close bounded while a resize holds the control, the handoff of the
//! control's cleanup, the seal refusing resizes and subscriptions, and the budgeted
//! subscribe (`docs/design/pty-runtime.md` §2.2, §2.7; `docs/design/t1-attach.md` I-1, D12).

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use chaldea_pty_runtime::fake::{FakeChild, FakeEvent, FakeScript, KillBehavior, ProbePoint};
use chaldea_pty_runtime::{
    ChildExit, CloseBounds, Fence, Limits, PtySize, ResizeError, Resized, SessionEvent,
    SessionHandle, SubscribeError, grid_rows,
};
use common::*;

/// The slack a bounded close may take past its budgets on a loaded runner.
const SLACK: Duration = Duration::from_secs(1);

fn to() -> PtySize {
    PtySize::new(100, 30).unwrap()
}

fn control_closes(c: &FakeChild) -> usize {
    c.log()
        .iter()
        .filter(|e| **e == FakeEvent::ControlClosed)
        .count()
}

fn resizes(c: &FakeChild) -> usize {
    c.log()
        .iter()
        .filter(|e| matches!(e, FakeEvent::Resize(_)))
        .count()
}

fn resize_in_thread(h: &SessionHandle) -> JoinHandle<Result<Resized, ResizeError>> {
    let h = h.clone();
    std::thread::spawn(move || h.resize(to(), None))
}

#[derive(Debug, Clone, Copy)]
enum CloseKind {
    Close,
    /// The child exited before the close.
    Exit,
    /// The shutdown's and the exit op's `close_by`.
    Shutdown,
    /// A close step panics (the kill).
    Unwind,
}

/// I-1: a close returns within its budgets while a resize holds the control in the backend
/// call, reports the control unreleased, and once the resize returns the control is closed
/// exactly once (by the resize).
#[test]
fn close_with_resize_held() {
    for kind in [
        CloseKind::Close,
        CloseKind::Exit,
        CloseKind::Shutdown,
        CloseKind::Unwind,
    ] {
        let script = FakeScript {
            kill: if matches!(kind, CloseKind::Unwind) {
                KillBehavior::Panic
            } else {
                KillBehavior::ExitOnKill
            },
            ..FakeScript::default()
        };
        let Rig {
            session,
            child,
            events,
            ..
        } = rig_with(script, Limits::default(), 80, 24);
        let h = session.handle();
        let hold = child.hold_resizes();
        let resize = resize_in_thread(&h);
        assert!(hold.wait_entered(soon()), "{kind:?}");
        if matches!(kind, CloseKind::Exit) {
            child.exit(ChildExit {
                code: Some(0),
                signal: None,
            });
            assert!(events.wait(|e| matches!(e, SessionEvent::Exited(_))));
        }
        let bounds = short_bounds();
        let t = Instant::now();
        let out = match kind {
            CloseKind::Close | CloseKind::Exit => Some(session.close(&[], bounds)),
            CloseKind::Shutdown => {
                Some(session.close_by(&[], Instant::now() + bounds.kill, bounds))
            }
            CloseKind::Unwind => {
                let res = catch_unwind(AssertUnwindSafe(move || session.close(&[], bounds)));
                assert!(res.is_err(), "the kill panics");
                None
            }
        };
        let took = t.elapsed();
        assert!(took <= bounds.total() + SLACK, "{kind:?}: {took:?}");
        if let Some(out) = &out {
            assert!(!out.released.control, "{kind:?}: {out:?}");
            assert_eq!(out.close.elapsed, None, "{kind:?}");
        }
        // The hold is still in place: nothing has closed the control yet.
        assert_eq!(control_closes(&child), 0, "{kind:?}");
        drop(hold);
        let resized = resize.join().unwrap();
        assert!(
            resized.is_ok(),
            "{kind:?}: permitted before the seal: {resized:?}"
        );
        assert_eq!(control_closes(&child), 1, "{kind:?}");
        child.close_output();
    }
}

/// I-1: a resize after the seal is refused before the backend call; unsubscribe still works.
#[test]
fn resize_after_seal_refused() {
    let r = rig();
    let h = r.session.handle();
    let (sink, _rx) = chaldea_pty_runtime::channel_sink(1024);
    let sub = h.subscribe(sink, soon(), |_| usize::MAX).unwrap();
    let _ = r.session.seal();
    assert!(matches!(h.resize(to(), None), Err(ResizeError::Closed)));
    assert_eq!(resizes(&r.child), 0);
    let (sink, _rx2) = chaldea_pty_runtime::channel_sink(1024);
    assert_eq!(
        h.subscribe(sink, soon(), |_| usize::MAX).map(|s| s.id),
        Err(SubscribeError::Closed)
    );
    h.unsubscribe(sub.id);
    assert_eq!(h.size(), PtySize::new(80, 24).unwrap());
}

/// A resize whose fence moved is refused before the backend call.
#[test]
fn resize_fence_checked_before_backend() {
    let r = rig();
    let h = r.session.handle();
    let word = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
    let fence = Fence {
        word: word.clone(),
        expect: 1,
    };
    let probe = r.session.probe();
    let park = probe.park(ProbePoint::ResizeLocked);
    let h2 = h.clone();
    let f2 = fence.clone();
    let t = std::thread::spawn(move || h2.resize(to(), Some(&f2)));
    assert!(probe.wait(ProbePoint::ResizeLocked, 1, soon()));
    word.store(2, std::sync::atomic::Ordering::Release);
    drop(park);
    assert!(matches!(t.join().unwrap(), Err(ResizeError::FenceMoved)));
    assert_eq!(resizes(&r.child), 0);
    assert_eq!(
        h.resize(to(), Some(&Fence { word, expect: 2 })).unwrap(),
        Resized::PtyAndVt
    );
    assert_eq!(resizes(&r.child), 1);
}

/// I-1: a resize paused after its final check and before it releases the control, while
/// the close times out on the control lock: exactly one of them closes the control. Also
/// with the resize paused before its seal check (it then sees the seal and is refused).
#[test]
fn resize_handoff_closes_once() {
    for point in [ProbePoint::ResizeDone, ProbePoint::ResizeLocked] {
        let Rig { session, child, .. } = rig();
        let h = session.handle();
        let probe = session.probe();
        let park = probe.park(point);
        let resize = resize_in_thread(&h);
        assert!(probe.wait(point, 1, soon()), "{point:?}");
        let bounds = short_bounds();
        let t = Instant::now();
        let out = session.close(&[], bounds);
        assert!(t.elapsed() <= bounds.total() + SLACK);
        assert!(!out.released.control, "{point:?}: {out:?}");
        assert_eq!(control_closes(&child), 0);
        drop(park);
        let res = resize.join().unwrap();
        match point {
            ProbePoint::ResizeDone => assert!(res.is_ok(), "{res:?}"),
            _ => assert!(matches!(res, Err(ResizeError::Closed)), "{res:?}"),
        }
        assert_eq!(control_closes(&child), 1, "{point:?}");
        child.close_output();
    }
}

/// A close that gets the control after the resize released it closes it itself.
#[test]
fn close_after_resize_closes_once() {
    let Rig { session, child, .. } = rig();
    let h = session.handle();
    assert_eq!(h.resize(to(), None).unwrap(), Resized::PtyAndVt);
    let out = session.close(&[], CloseBounds::default());
    assert!(out.released.control);
    assert_eq!(control_closes(&child), 1);
}

/// D12: the copy keeps the grid exactly up to its encoded length; one byte less and the
/// subscription is registered with no grid at the same offset.
#[test]
fn subscribe_grid_budget_is_exact() {
    let r = rig();
    emit_parsed(&r, "ab\u{3042}\"\\\x07 c\r\n  x".as_bytes());
    let snap = r.session.snapshot(soon()).unwrap();
    let exact = serde_json::to_string(&grid_rows(&snap.lines))
        .unwrap()
        .len();
    let h = r.session.handle();
    let (sink, _rx) = chaldea_pty_runtime::channel_sink(1024);
    let full = h.subscribe(sink, soon(), |_| exact).unwrap();
    assert_eq!(full.lines.as_deref(), Some(&snap.lines[..]));
    assert_eq!(full.meta, snap.meta());
    let (sink, _rx) = chaldea_pty_runtime::channel_sink(1024);
    let cut = h.subscribe(sink, soon(), |_| exact - 1).unwrap();
    assert_eq!(cut.lines, None);
    assert_eq!(cut.meta, snap.meta());
    assert_ne!(cut.id, full.id);
}

/// D12: a cell with a long zero-width run stops the copy at the budget.
#[test]
fn zero_width_cell_budget() {
    let r = rig();
    let mut out = String::from("e");
    for _ in 0..20_000 {
        out.push('\u{301}');
    }
    emit_parsed(&r, out.as_bytes());
    let h = r.session.handle();
    let (sink, _rx) = chaldea_pty_runtime::channel_sink(1024);
    let s = h.subscribe(sink, soon(), |_| 4096).unwrap();
    assert_eq!(s.lines, None);
    let (sink, _rx) = chaldea_pty_runtime::channel_sink(1024);
    let s = h.subscribe(sink, soon(), |_| usize::MAX).unwrap();
    let cell = &s.lines.unwrap()[0].cells[0];
    assert_eq!(cell.text.chars().count(), 20_001);
}
