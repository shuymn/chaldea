//! The HTTP fault points (design §7): each is inert until a test arms it.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use crate::store::tests::support::waits;
use tokio::sync::oneshot;

type Hook = Box<dyn FnOnce() + Send>;
/// Tells the test the gate was reached; admission waits for the release.
type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Default)]
pub(crate) struct Points {
    /// Polls of request bodies by the application, since the server started.
    pub(crate) body_polls: AtomicUsize,
    /// Large blob response bodies that ended with an error (`blobs::PartsBody`).
    pub(crate) blob_body_errors: AtomicUsize,
    /// Run once, on the blocking thread, before a store read starts: after skipping the
    /// given number of reads.
    pub(crate) before_read: Mutex<Option<(usize, Hook)>>,
    /// Run once, on the blocking thread, after a store read returned: after skipping the
    /// given number of reads.
    pub(crate) after_read: Mutex<Option<(usize, Hook)>>,
    /// Returned by the next accepts instead of accepting.
    pub(crate) accept_errors: Mutex<VecDeque<io::ErrorKind>>,
    /// Write jobs `run_write` submitted to the store.
    pub(crate) submits: AtomicUsize,
    /// The `not_after` of each job `run_write` submitted, in order.
    pub(crate) submitted_not_afters: Mutex<Vec<std::time::Instant>>,
    /// Write jobs whose ticket answered `run_write` with `WriteError::Expired`.
    pub(crate) expired_writes: AtomicUsize,
    /// The server-clock instant of each timer set through `Shared::timer`, in order.
    pub(crate) timers: Mutex<Vec<std::time::Instant>>,
    /// The deadline of each hook pid wait, in order.
    pub(crate) pid_wait_deadlines: Mutex<Vec<std::time::Instant>>,
    /// Held once by an upgrade admission between its final read and the ticket removal.
    pub(crate) removal_gate: Mutex<Option<Gate>>,
    /// Held once by a read's or a write reply's waiter before it first polls its result
    /// against the cutoff.
    pub(crate) cutoff_gate: Mutex<Option<Gate>>,
    /// Held once by a read's waiter between its cutoff sample and the classification of its
    /// result.
    pub(crate) sample_gate: Mutex<Option<Gate>>,
    /// Told the first time the next registered socket task parks.
    pub(crate) first_park: Mutex<Option<oneshot::Sender<()>>>,
    /// Told the first time the next write observer's wait for its ticket parks.
    pub(crate) ticket_park: Mutex<Option<oneshot::Sender<()>>>,
    /// Held once by the next write observer after its ticket answered, before the observer
    /// reads the answer.
    pub(crate) ticket_answer_gate: Mutex<Option<Gate>>,
    /// The upgrade steps recorded, in order.
    pub(crate) steps: Mutex<Vec<&'static str>>,
    /// Held once by a socket's next send before it writes.
    pub(crate) socket_send_gate: Mutex<Option<Gate>>,
    /// Held once by a socket's next send after its frames were fed to the sink, before the
    /// flush that hands them to the transport.
    pub(crate) socket_flush_gate: Mutex<Option<Gate>>,
    /// Held once by a socket's next close frame before it is sent (inside the close's
    /// grace).
    pub(crate) close_frame_gate: Mutex<Option<Gate>>,
    /// While set, sockets drop the store's commit notifications.
    pub(crate) drop_notifications: AtomicBool,
    /// Held once by a command runner between its registration and its first submit.
    pub(crate) runner_gate: Mutex<Option<Gate>>,
    /// Held once by a command's reply wait at the top of its loop, before it reads the
    /// runner's progress.
    pub(crate) progress_gate: Mutex<Option<Gate>>,
    /// The server's clock at each receipt completion attempt's admission, in order.
    pub(crate) completion_attempts: Mutex<Vec<std::time::Instant>>,
    /// Each pause between completion attempts, in order: the server's clock when it was
    /// scheduled and the instant it wakes at.
    pub(crate) completion_pauses: Mutex<Vec<(std::time::Instant, std::time::Instant)>>,
    /// Connections whose first byte was read (the request origin recorded).
    pub(crate) first_bytes: AtomicUsize,
    /// Taken by the next receipt completion attempt, which holds it inside its job on the
    /// writer thread.
    pub(crate) completion_hold: Mutex<Option<WriterHold>>,
    /// Held once by a registered write observer before it submits its job.
    pub(crate) submit_gate: Mutex<Option<Gate>>,
}

/// A hold inside a write job, on the writer thread: tells the test it was reached, then
/// waits (at most 20 s) for the release.
pub(crate) struct WriterHold {
    reached: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

impl WriterHold {
    pub(crate) fn hold(self) {
        let _ = self.reached.send(());
        let _ = self
            .release
            .recv_timeout(std::time::Duration::from_secs(20));
    }
}

/// Arms a gate: the returned receiver fires when the gate is reached, and the waiter goes
/// on once the returned sender is used or dropped, or after 20 s.
fn arm(slot: &Mutex<Option<Gate>>) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((reached_tx, release_rx));
    (reached_rx, release_tx)
}

/// Passes a gate: if armed, tells the test and waits for the release; true if it was.
async fn pass(slot: &Mutex<Option<Gate>>) -> bool {
    let gate = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    let Some((reached, release)) = gate else {
        return false;
    };
    let _ = reached.send(());
    let _ = waits::bounded(std::time::Duration::from_secs(20), release).await;
    true
}

/// Passes a gate as [`pass`], but waits until the release is used or dropped, never on a
/// timer: for gates whose test result depends on the waiter not going on by itself.
async fn pass_held(slot: &Mutex<Option<Gate>>) -> bool {
    let gate = slot.lock().unwrap_or_else(PoisonError::into_inner).take();
    let Some((reached, release)) = gate else {
        return false;
    };
    let _ = reached.send(());
    let _ = release.await;
    true
}

/// The armed hook if its skip count is used up; otherwise counts one skip.
fn take(slot: &Mutex<Option<(usize, Hook)>>) -> Option<Hook> {
    let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
    match s.as_mut() {
        Some((0, _)) => s.take().map(|(_, f)| f),
        Some((n, _)) => {
            *n -= 1;
            None
        }
        None => None,
    }
}

impl Points {
    pub(crate) fn body_polled(&self) {
        self.body_polls.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn polls(&self) -> usize {
        self.body_polls.load(Ordering::SeqCst)
    }

    pub(crate) fn before_read(&self) {
        if let Some(f) = take(&self.before_read) {
            f();
        }
    }

    pub(crate) fn after_read(&self) {
        if let Some(f) = take(&self.after_read) {
            f();
        }
    }

    /// Runs `f` before the read that follows `skip` other reads.
    pub(crate) fn arm_before_read(&self, skip: usize, f: impl FnOnce() + Send + 'static) {
        *self
            .before_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((skip, Box::new(f)));
    }

    /// Runs `f` after the read that follows `skip` other reads.
    pub(crate) fn arm_after_read(&self, skip: usize, f: impl FnOnce() + Send + 'static) {
        *self
            .after_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((skip, Box::new(f)));
    }

    pub(crate) fn arm_accept_errors(&self, kinds: &[io::ErrorKind]) {
        self.accept_errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(kinds.iter().copied());
    }

    pub(crate) fn accept_error(&self) -> Option<io::Error> {
        self.accept_errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .map(io::Error::from)
    }

    pub(crate) fn write_submitted(&self, not_after: std::time::Instant) {
        self.submits.fetch_add(1, Ordering::SeqCst);
        self.submitted_not_afters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(not_after);
    }

    pub(crate) fn write_expired(&self) {
        self.expired_writes.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn expired_writes(&self) -> usize {
        self.expired_writes.load(Ordering::SeqCst)
    }

    pub(crate) fn timer_set(&self, at: std::time::Instant) {
        self.timers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(at);
    }

    pub(crate) fn timers(&self) -> Vec<std::time::Instant> {
        self.timers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn pid_wait_started(&self, deadline: std::time::Instant) {
        self.pid_wait_deadlines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(deadline);
    }

    pub(crate) fn pid_wait_deadlines(&self) -> Vec<std::time::Instant> {
        self.pid_wait_deadlines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn submitted_not_afters(&self) -> Vec<std::time::Instant> {
        self.submitted_not_afters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Arms the submit gate, as [`Points::arm_removal_gate`], except that the submission
    /// goes on only when the returned sender is used or dropped (no 20 s release).
    pub(crate) fn arm_submit_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.submit_gate)
    }

    pub(crate) async fn before_submit(&self) {
        pass_held(&self.submit_gate).await;
    }

    pub(crate) fn submits(&self) -> usize {
        self.submits.load(Ordering::SeqCst)
    }

    /// Arms the gate: the returned receiver fires when an admission reaches it, and the
    /// admission goes on once the returned sender is used or dropped.
    pub(crate) fn arm_removal_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.removal_gate)
    }

    pub(crate) async fn before_ticket_removal(&self) {
        pass(&self.removal_gate).await;
    }

    /// Arms the cutoff gate, as [`Points::arm_removal_gate`]; after the release the waiter
    /// also waits (at most 10 s) until its result is in, so it then polls a ready result.
    pub(crate) fn arm_cutoff_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.cutoff_gate)
    }

    pub(crate) async fn before_cutoff_wait(&self, ready: impl Fn() -> bool) {
        if !pass(&self.cutoff_gate).await {
            return;
        }
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ready() {
            assert!(
                std::time::Instant::now() < until,
                "the result never came in"
            );
            tokio::task::yield_now().await;
        }
    }

    /// Arms the sample gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_sample_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.sample_gate)
    }

    pub(crate) async fn after_cutoff_sample(&self) {
        pass(&self.sample_gate).await;
    }

    /// Arms the socket send gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_socket_send_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.socket_send_gate)
    }

    pub(crate) async fn before_socket_send(&self) {
        pass_held(&self.socket_send_gate).await;
    }

    /// Arms the socket flush gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_socket_flush_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.socket_flush_gate)
    }

    pub(crate) async fn before_socket_flush(&self) {
        pass_held(&self.socket_flush_gate).await;
    }

    /// Arms the close frame gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_close_frame_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.close_frame_gate)
    }

    pub(crate) async fn before_close_frame(&self) {
        pass_held(&self.close_frame_gate).await;
    }

    /// Arms the runner gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_runner_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.runner_gate)
    }

    pub(crate) async fn before_runner_submit(&self) {
        pass(&self.runner_gate).await;
    }

    /// Arms the progress gate, as [`Points::arm_removal_gate`].
    pub(crate) fn arm_progress_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.progress_gate)
    }

    pub(crate) async fn before_progress_read(&self) {
        pass(&self.progress_gate).await;
    }

    pub(crate) fn completion_attempt(&self, at: std::time::Instant) {
        self.completion_attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(at);
    }

    pub(crate) fn completion_attempts(&self) -> Vec<std::time::Instant> {
        self.completion_attempts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn completion_pause(&self, from: std::time::Instant, wake: std::time::Instant) {
        self.completion_pauses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((from, wake));
    }

    pub(crate) fn completion_pauses(&self) -> Vec<(std::time::Instant, std::time::Instant)> {
        self.completion_pauses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn first_byte_read(&self) {
        self.first_bytes.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn first_bytes(&self) -> usize {
        self.first_bytes.load(Ordering::SeqCst)
    }

    /// Arms the completion hold: the returned receiver gets a message when the next
    /// completion job reaches it, and the job goes on once the returned sender is used or
    /// dropped.
    pub(crate) fn arm_completion_hold(
        &self,
    ) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self
            .completion_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(WriterHold {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }

    pub(crate) fn take_completion_hold(&self) -> Option<WriterHold> {
        self.completion_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    pub(crate) fn drop_notifications(&self, on: bool) {
        self.drop_notifications.store(on, Ordering::SeqCst);
    }

    pub(crate) fn notifications_dropped(&self) -> bool {
        self.drop_notifications.load(Ordering::SeqCst)
    }

    pub(crate) fn step(&self, step: &'static str) {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(step);
    }

    pub(crate) fn steps(&self) -> Vec<&'static str> {
        self.steps
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The returned receiver fires the first time the next write observer's wait for its
    /// ticket parks (`run_write`): its job was submitted and has not answered yet.
    pub(crate) fn arm_ticket_park(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self
            .ticket_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    pub(crate) async fn signal_ticket_park<F: Future>(&self, fut: F) -> F::Output {
        let mut signal = self
            .ticket_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut fut = std::pin::pin!(fut);
        std::future::poll_fn(move |cx| {
            let r = fut.as_mut().poll(cx);
            if r.is_pending()
                && let Some(s) = signal.take()
            {
                let _ = s.send(());
            }
            r
        })
        .await
    }

    pub(crate) fn arm_ticket_answer_gate(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.ticket_answer_gate)
    }

    /// Awaits `fut`, the observer's ticket, then passes the answer gate if it was armed when
    /// this observer began its wait.
    pub(crate) async fn hold_ticket_answer<F: Future>(&self, fut: F) -> F::Output {
        let gate = self
            .ticket_answer_gate
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let out = fut.await;
        if let Some((reached, release)) = gate {
            let _ = reached.send(());
            let _ = waits::bounded(std::time::Duration::from_secs(20), release).await;
        }
        out
    }

    /// The returned receiver fires the first time the next registered socket task parks.
    pub(crate) fn arm_first_park(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        *self
            .first_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tx);
        rx
    }

    pub(crate) fn signal_first_park<F>(&self, fut: F) -> impl Future<Output = ()> + Send + 'static
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let mut signal = self
            .first_park
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let mut fut = Box::pin(fut);
        std::future::poll_fn(move |cx| {
            let r = fut.as_mut().poll(cx);
            if r.is_pending()
                && let Some(s) = signal.take()
            {
                let _ = s.send(());
            }
            r
        })
    }
}
