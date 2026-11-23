//! Hook ingest's test-only sync points: each is inert until a test arms it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

use tokio::sync::oneshot;

/// Tells the test the point was reached; the request waits for the release.
type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

#[derive(Default)]
pub(crate) struct Points {
    pre_read: Mutex<Option<Gate>>,
    /// Held after intake by the request whose ordinal matches.
    intake: Mutex<Option<(u64, Gate)>>,
    subscribe: Mutex<Option<Gate>>,
    /// Held before the job by the request whose ordinal matches.
    job: Mutex<Option<(u64, Gate)>>,
    /// Walks submitted to the blocking pool, counted before the submission.
    walks: AtomicUsize,
}

fn arm(slot: &Mutex<Option<Gate>>) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some((reached_tx, release_rx));
    (reached_rx, release_tx)
}

fn arm_for(
    slot: &Mutex<Option<(u64, Gate)>>,
    ordinal: u64,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *slot.lock().unwrap_or_else(PoisonError::into_inner) =
        Some((ordinal, (reached_tx, release_rx)));
    (reached_rx, release_tx)
}

/// Tells the test and waits until the release is sent or dropped; the gate never releases
/// itself, so the test bounds its own waits.
async fn pass(gate: Option<Gate>) {
    if let Some((reached, release)) = gate {
        let _ = reached.send(());
        let _ = release.await;
    }
}

fn take_for(slot: &Mutex<Option<(u64, Gate)>>, ordinal: u64) -> Option<Gate> {
    let mut s = slot.lock().unwrap_or_else(PoisonError::into_inner);
    if s.as_ref().is_some_and(|(o, _)| *o == ordinal) {
        s.take().map(|(_, g)| g)
    } else {
        None
    }
}

impl Points {
    pub(crate) fn arm_pre_read(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.pre_read)
    }

    pub(crate) async fn before_pre_read(&self) {
        let g = self
            .pre_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        pass(g).await;
    }

    pub(crate) fn arm_intake(&self, ordinal: u64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm_for(&self.intake, ordinal)
    }

    pub(crate) async fn after_intake(&self, ordinal: u64) {
        pass(take_for(&self.intake, ordinal)).await;
    }

    pub(crate) fn arm_subscribe(&self) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm(&self.subscribe)
    }

    pub(crate) async fn after_subscribe(&self) {
        let g = self
            .subscribe
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        pass(g).await;
    }

    pub(crate) fn walk_submitted(&self) {
        self.walks.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn walks(&self) -> usize {
        self.walks.load(Ordering::SeqCst)
    }

    pub(crate) fn arm_job(&self, ordinal: u64) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        arm_for(&self.job, ordinal)
    }

    pub(crate) async fn before_job(&self, ordinal: u64) {
        pass(take_for(&self.job, ordinal)).await;
    }
}
