//! The pseudoconsole's control (`docs/design/pty-windows.md` §2.3): resize, and a close that
//! runs on a worker thread started before the pseudoconsole exists, measured against the
//! caller's deadline.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::sys::Pcon;
use crate::backend::{CloseReport, PtyControl, PtySize, TEARDOWN_CLOSE};

enum Msg {
    Arm(Box<dyn FnOnce() + Send>),
    Close,
}

/// A thread that runs one close when told to. It exists before the resource it closes, so
/// no path holds that resource without a thread able to close it.
pub(crate) struct CloseWorker {
    tx: Sender<Msg>,
    /// When the close operation returned.
    done: Receiver<Instant>,
    join: JoinHandle<()>,
}

#[cfg(test)]
type AfterSend = Box<dyn FnOnce(&JoinHandle<()>)>;

#[cfg(test)]
thread_local! {
    /// Runs in `close` between handing over the close and receiving its result (tests).
    static AFTER_SEND: std::cell::RefCell<Option<AfterSend>> = const { std::cell::RefCell::new(None) };
}

impl CloseWorker {
    /// Starts the thread; it waits for a message.
    pub(crate) fn start(name: &'static str) -> io::Result<CloseWorker> {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (done_tx, done) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut armed: Option<Box<dyn FnOnce() + Send>> = None;
                loop {
                    match rx.recv() {
                        Ok(Msg::Arm(f)) => armed = Some(f),
                        // `close`, or dropped without it: an armed worker closes (on its
                        // own thread when dropped); an unarmed one exits.
                        msg => {
                            if let Some(f) = armed.take() {
                                f();
                            }
                            if msg.is_ok() {
                                let _ = done_tx.send(Instant::now());
                            }
                            return;
                        }
                    }
                }
            })?;
        Ok(CloseWorker { tx, done, join })
    }

    /// Hands over the close. The worker is blocked in its receive while it lives, so the
    /// send cannot fail.
    pub(crate) fn arm(&mut self, close: Box<dyn FnOnce() + Send>) {
        let _ = self.tx.send(Msg::Arm(close));
    }

    /// Disconnects without `close` and returns the thread (tests: its exit is observable).
    #[cfg(test)]
    fn disconnect(self) -> JoinHandle<()> {
        self.join
    }

    /// Runs the close and waits for it until `deadline`: `Some` when the close operation
    /// returned by then (its completion time compared with the deadline, so a completion
    /// queued late is not on time); `None` abandons the thread with the close recorded as
    /// hung. The thread itself is not joined (`docs/design/pty-windows.md` §2.3).
    pub(crate) fn close(self, deadline: Instant) -> Option<Duration> {
        let t = Instant::now();
        let CloseWorker { tx, done, join } = self;
        if tx.send(Msg::Close).is_err() {
            return None;
        }
        #[cfg(test)]
        if let Some(f) = AFTER_SEND.with(|h| h.borrow_mut().take()) {
            f(&join);
        }
        match done.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(at) if at <= deadline => {
                #[cfg(any(test, feature = "fake"))]
                super::hooks::register_close_thread(join);
                #[cfg(not(any(test, feature = "fake")))]
                drop(join);
                Some(at.saturating_duration_since(t))
            }
            Ok(_) | Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => None,
        }
    }
}

/// What the control closes and resizes: the pseudoconsole, or a test double.
pub(crate) trait Pseudo: Send + Sync + 'static {
    fn close(self);
    fn resize(&self, size: PtySize) -> io::Result<()>;
}

impl Pseudo for Pcon {
    fn close(self) {
        Pcon::close(self);
    }

    fn resize(&self, size: PtySize) -> io::Result<()> {
        Pcon::resize(self, size)
    }
}

/// The session's `PtyControl`. The armed closure holds the only other reference to the
/// pseudoconsole; `close` and `Drop` release this one first, so the closure's `try_unwrap`
/// succeeds by construction.
pub(crate) struct WindowsControl<P: Pseudo = Pcon> {
    pcon: Option<Arc<P>>,
    worker: Option<CloseWorker>,
    /// A drain hold released when the close begins (`HoldOutput`).
    #[cfg(any(test, feature = "fake"))]
    hold: Option<Arc<super::hooks::Marker>>,
}

impl<P: Pseudo> WindowsControl<P> {
    /// Arms `worker` with the close of `pcon` at once (`arm` cannot fail).
    pub(crate) fn new(pcon: P, mut worker: CloseWorker) -> WindowsControl<P> {
        let pcon = Arc::new(pcon);
        let theirs = pcon.clone();
        worker.arm(Box::new(move || {
            if let Ok(p) = Arc::try_unwrap(theirs) {
                p.close();
            }
        }));
        WindowsControl {
            pcon: Some(pcon),
            worker: Some(worker),
            #[cfg(any(test, feature = "fake"))]
            hold: None,
        }
    }

    /// Keeps the drain's reader parked until this control's close begins.
    #[cfg(any(test, feature = "fake"))]
    pub(crate) fn hold_until_close(&mut self, m: Arc<super::hooks::Marker>) {
        self.hold = Some(m);
    }

    /// The pseudoconsole, while the control is open (stage 5's attribute lists borrow it).
    pub(crate) fn pseudo(&self) -> Option<&P> {
        self.pcon.as_deref()
    }

    fn release(&mut self, deadline: Instant) -> CloseReport {
        #[cfg(any(test, feature = "fake"))]
        if let Some(m) = self.hold.take() {
            m.release_at_close();
        }
        drop(self.pcon.take());
        let elapsed = self.worker.take().and_then(|w| w.close(deadline));
        CloseReport { elapsed }
    }
}

impl<P: Pseudo> PtyControl for WindowsControl<P> {
    fn resize(&self, size: PtySize) -> io::Result<()> {
        match &self.pcon {
            Some(p) => p.resize(size),
            None => Err(io::Error::other("the pseudoconsole is closed")),
        }
    }

    fn close(mut self: Box<Self>, deadline: Instant) -> CloseReport {
        super::tr("close", true, 0, Ok(0));
        self.release(deadline)
    }
}

impl<P: Pseudo> Drop for WindowsControl<P> {
    fn drop(&mut self) {
        let _ = self.release(Instant::now() + TEARDOWN_CLOSE);
    }
}

#[cfg(test)]
mod tests {
    use std::os::windows::io::AsHandle;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Waits (bounded) for the thread behind `join` to exit, on its handle.
    fn exited(join: &JoinHandle<()>) -> bool {
        super::super::sys::wait_floor(join.as_handle(), WAIT).unwrap_or(false)
    }

    const WAIT: Duration = Duration::from_secs(10);

    /// A pseudoconsole double: `close` records that the closure got sole ownership.
    struct Double {
        closed: Arc<AtomicBool>,
    }

    impl Pseudo for Double {
        fn close(self) {
            self.closed.store(true, Ordering::Release);
        }

        fn resize(&self, _size: PtySize) -> io::Result<()> {
            Ok(())
        }
    }

    /// PR4-CL-20: a close that does not return by the deadline is `None`, also when its
    /// late completion is already queued as the caller reads it.
    #[test]
    fn hung_close_worker_is_bounded() {
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let mut w = CloseWorker::start("conpty-close-test").unwrap();
        w.arm(Box::new(move || {
            let _ = release_rx.recv_timeout(WAIT);
        }));
        let t = Instant::now();
        assert_eq!(w.close(Instant::now() + Duration::from_millis(100)), None);
        assert!(t.elapsed() < WAIT, "watchdog: {:?}", t.elapsed());
        release_tx.send(()).unwrap();
        // The caller parks between handing over the close and reading its result; the close
        // returns only after the deadline, and its completion is queued before the read.
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut w = CloseWorker::start("conpty-close-test").unwrap();
        w.arm(Box::new(move || {
            let _ = go_rx.recv_timeout(WAIT);
        }));
        let deadline = Instant::now() + Duration::from_millis(1);
        AFTER_SEND.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |join: &JoinHandle<()>| {
                while Instant::now() <= deadline {
                    std::thread::yield_now();
                }
                go_tx.send(()).unwrap();
                // The worker sends its completion before it exits.
                assert!(exited(join), "watchdog: the worker did not finish");
            }));
        });
        assert_eq!(w.close(deadline), None, "a late completion is not on time");
        // A close that returns is `Some`.
        let mut w = CloseWorker::start("conpty-close-test").unwrap();
        w.arm(Box::new(|| {}));
        assert!(w.close(Instant::now() + WAIT).is_some());
    }

    /// An unarmed worker told to close acknowledges and exits.
    #[test]
    fn unarmed_worker_exits() {
        let w = CloseWorker::start("conpty-close-test").unwrap();
        assert!(w.close(Instant::now() + WAIT).is_some());
        // Dropped unarmed: the thread exits.
        let join = CloseWorker::start("conpty-close-test")
            .unwrap()
            .disconnect();
        assert!(exited(&join), "the unarmed worker did not exit");
        join.join().unwrap();
    }

    /// A worker dropped while armed still runs the close on its own thread.
    #[test]
    fn dropped_armed_worker_runs_the_close() {
        let (tx, rx) = mpsc::channel();
        let mut w = CloseWorker::start("conpty-close-test").unwrap();
        w.arm(Box::new(move || {
            let _ = tx.send(());
        }));
        drop(w);
        assert!(rx.recv_timeout(WAIT).is_ok(), "the close ran");
    }

    /// The control's reference is released before the close is dispatched, on `close` and
    /// on drop, so the closure's `try_unwrap` succeeds on both paths.
    #[test]
    fn close_and_drop_release_the_control_reference_first() {
        let closed = Arc::new(AtomicBool::new(false));
        let c = WindowsControl::new(
            Double {
                closed: closed.clone(),
            },
            CloseWorker::start("conpty-close-test").unwrap(),
        );
        let r = Box::new(c).close(Instant::now() + WAIT);
        assert!(r.elapsed.is_some());
        assert!(closed.load(Ordering::Acquire), "close path");

        let closed = Arc::new(AtomicBool::new(false));
        let c = WindowsControl::new(
            Double {
                closed: closed.clone(),
            },
            CloseWorker::start("conpty-close-test").unwrap(),
        );
        drop(c);
        assert!(closed.load(Ordering::Acquire), "drop path");

        // After `close` the control refuses a resize (unreachable through the trait).
        let mut c = WindowsControl::new(
            Double {
                closed: Arc::new(AtomicBool::new(false)),
            },
            CloseWorker::start("conpty-close-test").unwrap(),
        );
        let _ = c.release(Instant::now() + WAIT);
        assert!(c.resize(PtySize::new(80, 24).unwrap()).is_err());
    }
}
