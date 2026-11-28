//! The tailer's lifecycle: started for a Claude launch at its reservation, it waits for the
//! binding, then polls every [`TAIL_POLL`] until the launch exits (one final read), the
//! binding moves on (`StaleBinding`), or the supervisor's shutdown participant ends it. At
//! shutdown no poll starts once admission closes ([`Mode::Hold`]); after the participant has
//! recorded the launches' exits each tailer reads its binding again if it had none and makes
//! its final read ([`Mode::Final`]), a failed one retried every [`FINAL_RETRY`] within the
//! participant's budget, and at the budget's end a read in progress stops at its next chunk
//! or before its next write ([`Mode::Stop`]). All file I/O and every store read run on a blocking task that the
//! tailer's task awaits to its end, never on the store writer or under a launch's op lock;
//! the tailer's state is dropped on such a task too. Batches reach the store as write jobs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, LaunchId, StreamId, ThreadId};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::acc::SpoolDir;
use super::read::{Backend, Binding, Commit, FINAL_TRIES, Step, Tail, View};
use crate::store::{JobError, ReadTx, Store, StoreError, TranscriptBatch, WriteError};
use crate::supervisor::Supervisor;

/// The poll period.
pub(crate) const TAIL_POLL: Duration = Duration::from_secs(1);
/// A store read's wait for a connection, and a batch's `not_after`.
const READ_WAIT: Duration = Duration::from_secs(1);
const WRITE_WAIT: Duration = Duration::from_secs(10);
/// Immediate polls in a row before the tailer waits a period anyway.
const MAX_AGAIN: u32 = 64;
/// At shutdown, the wait before a failed final read (or binding read) is tried again,
/// within the participant's budget.
const FINAL_RETRY: Duration = Duration::from_millis(100);

/// What every tailer does, set by the shutdown participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Poll every period.
    Run,
    /// Admission closed: no poll starts (one in progress finishes).
    Hold,
    /// The launches' exits are recorded: each tailer makes its final read and ends.
    Final,
    /// The budget ran out: a read in progress stops at its next chunk.
    Stop,
}

/// The supervisor's tailers: their tasks by launch, and their mode.
pub(crate) struct Tailers {
    /// A launch stays here until its tailer's task ends; its handle is taken by the
    /// participant.
    map: Mutex<HashMap<LaunchId, Option<JoinHandle<()>>>>,
    mode: watch::Sender<Mode>,
    stop: Arc<AtomicBool>,
    /// Store reads to fail, in order (`true`: fail), before the rest succeed (tests).
    #[cfg(test)]
    pub(crate) read_faults: Mutex<std::collections::VecDeque<bool>>,
    /// Polls that ran (tests).
    #[cfg(test)]
    pub(crate) polls: std::sync::atomic::AtomicU32,
    /// Polls whose blocking task found `Hold` and read nothing (tests).
    #[cfg(test)]
    pub(crate) skipped: std::sync::atomic::AtomicU32,
    /// Store reads that borrowed a reader (tests).
    #[cfg(test)]
    pub(crate) reads: std::sync::atomic::AtomicU32,
    /// Final reads abandoned before the tailer had its binding (tests).
    #[cfg(test)]
    pub(crate) unbound_unverified: std::sync::atomic::AtomicU32,
    /// Shutdown participants cut before they joined the tailers (tests).
    #[cfg(test)]
    pub(crate) abandoned_unverified: std::sync::atomic::AtomicU32,
}

impl Default for Tailers {
    fn default() -> Self {
        Tailers {
            map: Mutex::default(),
            mode: watch::Sender::new(Mode::Run),
            stop: Arc::default(),
            #[cfg(test)]
            read_faults: Mutex::default(),
            #[cfg(test)]
            polls: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            skipped: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            reads: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            unbound_unverified: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            abandoned_unverified: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl Tailers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<LaunchId, Option<JoinHandle<()>>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The launches whose tailer has not ended.
    pub(crate) fn launches(&self) -> Vec<LaunchId> {
        self.lock().keys().copied().collect()
    }

    /// Whether the tailers were stopped (tests).
    #[cfg(test)]
    pub(crate) fn stopped(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Sets every tailer's mode.
    pub(crate) fn set(&self, mode: Mode) {
        if mode == Mode::Stop {
            self.stop.store(true, Ordering::Release);
        }
        self.mode.send_replace(mode);
    }

    /// The running tailers' tasks.
    pub(crate) fn tasks(&self) -> Vec<JoinHandle<()>> {
        self.lock().values_mut().filter_map(Option::take).collect()
    }
}

/// The store side of a running tailer.
struct StoreBackend {
    sup: Arc<Supervisor>,
}

pub(super) fn binding_view(r: &ReadTx<'_>, b: &Binding) -> Result<View, StoreError> {
    let (Some(t), Some(l)) = (r.thread(b.thread_id)?, r.launch(b.launch_id)?) else {
        return Ok(View {
            current: false,
            exited: false,
            cursor: None,
        });
    };
    let current = t.current_launch_id == Some(b.launch_id)
        && l.session_id.as_deref() == Some(b.session_id.as_str())
        && t.session_id.as_deref() == Some(b.session_id.as_str())
        && t.transcript_path.as_deref() == Some(b.path.as_str());
    Ok(View {
        current,
        exited: l.exit_reason.is_some(),
        cursor: r.transcript_cursor(b.thread_id)?,
    })
}

/// A tailer's store read (on the blocking task it runs on); at shutdown its wait for a
/// reader ends with the participant's budget.
fn read<R>(
    sup: &Supervisor,
    f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>,
) -> Result<R, String> {
    #[cfg(test)]
    if sup
        .tailers
        .read_faults
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop_front()
        == Some(true)
    {
        return Err("no reader (test)".into());
    }
    let mut until = sup.now() + READ_WAIT;
    if let Some(w) = sup.window.get() {
        // The pool lends a reader free at once without checking the deadline.
        if sup.now() >= w.end {
            return Err("the participant's end has passed".into());
        }
        until = until.min(w.end);
    }
    sup.store
        .read(sup.real(until), |r| {
            #[cfg(test)]
            sup.tailers
                .reads
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[cfg(test)]
            sup.pauses().blocking(crate::supervisor::Pause::TailerRead);
            f(r)
        })
        .map_err(|e| format!("{e:?}"))
}

/// One tailer store read that does nothing (tests).
#[cfg(test)]
pub(crate) fn read_probe(sup: &Supervisor) -> Result<(), String> {
    read(sup, |_| Ok(()))
}

impl Backend for StoreBackend {
    fn view(&self, b: &Binding) -> Result<View, String> {
        read(&self.sup, |r| binding_view(r, b))
    }

    fn commit(&self, batch: TranscriptBatch) -> Commit {
        commit(&self.sup.store, batch)
    }

    #[cfg(test)]
    fn after_read(&self) {
        self.sup
            .pauses()
            .blocking(crate::supervisor::Pause::TailerAfterRead);
    }
}

/// Submits `batch` as one write job and waits for it.
pub(crate) fn commit(store: &Store, batch: TranscriptBatch) -> Commit {
    let ticket = match store.submit(Instant::now() + WRITE_WAIT, move |tx| {
        tx.append_transcript(batch)?;
        Ok(())
    }) {
        Ok(t) => t,
        Err(e) => return Commit::Failed(format!("submit: {e}")),
    };
    match ticket.blocking_wait() {
        Ok(()) => Commit::Done,
        Err(
            WriteError::RolledBack(StoreError::StaleCursor { .. })
            | WriteError::Rejected(JobError::Store(StoreError::StaleCursor { .. })),
        ) => Commit::StaleCursor,
        Err(
            WriteError::RolledBack(StoreError::StaleBinding { .. })
            | WriteError::Rejected(JobError::Store(StoreError::StaleBinding { .. })),
        ) => Commit::StaleBinding,
        Err(e) => Commit::Failed(e.to_string()),
    }
}

/// What a waiting tailer reads of its launch.
enum Bound {
    Yes(Binding),
    NotYet,
    /// Never to be bound with a transcript path: another launch is current, the launch
    /// exited unbound, or it bound without a path.
    Never,
}

fn bound(r: &ReadTx<'_>, thread: ThreadId, launch: LaunchId) -> Result<Bound, StoreError> {
    let (Some(t), Some(l)) = (r.thread(thread)?, r.launch(launch)?) else {
        return Ok(Bound::NotYet);
    };
    if t.current_launch_id != Some(launch) {
        return Ok(Bound::Never);
    }
    let Some(session) = l.session_id.clone() else {
        return Ok(if l.exit_reason.is_some() {
            Bound::Never
        } else {
            Bound::NotYet
        });
    };
    let Some(path) = t.transcript_path.clone() else {
        return Ok(Bound::Never);
    };
    Ok(Bound::Yes(Binding {
        thread_id: thread,
        launch_id: launch,
        session_id: session,
        path,
        agent: l.agent,
        cli_version: l.cli_version,
    }))
}

impl Supervisor {
    /// Starts the tailer of a Claude launch (from its reservation job; never blocks).
    pub(crate) fn tail_launch(self: &Arc<Self>, thread: ThreadId, launch: LaunchId, agent: Agent) {
        if agent != Agent::Claude {
            return;
        }
        let mut map = self.tailers.lock();
        if map.contains_key(&launch)
            || !self.tracker.accepting()
            || *self.tailers.mode.borrow() != Mode::Run
        {
            return;
        }
        let task = self.rt.spawn(run(self.clone(), thread, launch));
        map.insert(launch, Some(task));
    }

    fn spool_dir(&self) -> SpoolDir {
        let db = self.store.paths().db;
        SpoolDir::new(
            db.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf),
        )
    }

    /// Waits [`FINAL_RETRY`] or a mode change before a final read is tried again; `false`,
    /// at once, when the participant's budget leaves no room for the wait and another try.
    async fn final_wait(&self, mode: &mut watch::Receiver<Mode>) -> bool {
        if let Some(w) = self.window.get()
            && w.end.saturating_duration_since(self.now()) < 2 * FINAL_RETRY
        {
            return false;
        }
        let timer = crate::supervisor::tick_timer(self.timer(self.now() + FINAL_RETRY));
        tokio::select! {
            () = timer => {}
            _ = mode.changed() => {}
        }
        true
    }

    /// Waits a period, a commit naming `thread` when `commits` is given, or a mode change.
    async fn tail_wait(
        &self,
        thread: ThreadId,
        commits: Option<&mut tokio::sync::broadcast::Receiver<crate::store::Committed>>,
        mode: &mut watch::Receiver<Mode>,
    ) {
        let timer = crate::supervisor::tick_timer(self.timer(self.now() + self.cfg.tail_poll));
        tokio::pin!(timer);
        let stream = StreamId::Thread(thread);
        match commits {
            Some(c) => loop {
                tokio::select! {
                    () = &mut timer => return,
                    _ = mode.changed() => return,
                    r = c.recv() => match r {
                        Ok(c) if c.heads.iter().any(|h| h.stream_id == stream) => return,
                        Ok(_) => {}
                        Err(RecvError::Lagged(_) | RecvError::Closed) => return,
                    },
                }
            },
            None => {
                tokio::select! {
                    () = &mut timer => {}
                    _ = mode.changed() => {}
                }
            }
        }
    }
}

/// Runs `f` on a blocking task and waits for its end.
async fn blocking<T: Send + 'static>(
    sup: &Supervisor,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    sup.rt
        .spawn_blocking(move || {
            #[cfg(test)]
            let _on = super::read::OnBlocking::enter();
            f()
        })
        .await
        .ok()
}

/// A tailer's task.
async fn run(sup: Arc<Supervisor>, thread: ThreadId, launch: LaunchId) {
    let mut mode = sup.tailers.mode.subscribe();
    // Wait for the binding; at shutdown it is read once more when the exits are recorded.
    let mut commits = sup.store.subscribe();
    let mut bind_tries = 0;
    let b = loop {
        let m = *mode.borrow_and_update();
        match m {
            Mode::Run | Mode::Final => {}
            Mode::Hold => {
                if mode.changed().await.is_err() {
                    return finish(&sup, launch);
                }
                continue;
            }
            Mode::Stop => return finish(&sup, launch),
        }
        let s = sup.clone();
        match blocking(&sup, move || read(&s, |r| bound(r, thread, launch))).await {
            Some(Ok(Bound::Yes(b))) => break b,
            Some(Ok(Bound::Never)) => return finish(&sup, launch),
            Some(Ok(Bound::NotYet)) if m == Mode::Final => return finish(&sup, launch),
            Some(Ok(Bound::NotYet)) if sup.entry(launch).is_none() => {
                return finish(&sup, launch);
            }
            Some(Ok(Bound::NotYet) | Err(_)) | None => {}
        }
        if m == Mode::Final {
            // A failed read at shutdown is retried a few times, shortly, within the budget.
            bind_tries += 1;
            if bind_tries >= FINAL_TRIES || !sup.final_wait(&mut mode).await {
                // Whether the launch had a transcript to read is unknown.
                tracing::warn!(%thread, %launch, "the transcript's continuity is unverified: the binding could not be read for the final read");
                #[cfg(test)]
                sup.tailers
                    .unbound_unverified
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return finish(&sup, launch);
            }
        } else {
            sup.tail_wait(thread, Some(&mut commits), &mut mode).await;
        }
    };
    drop(commits);
    tracing::info!(%thread, %launch, path = %b.path, "the transcript tailer starts");
    let backend = Arc::new(StoreBackend { sup: sup.clone() });
    let mut tail = Some(Tail::new(b, sup.spool_dir(), sup.tailers.stop.clone()));
    let mut again = 0;
    // Failed views at shutdown, counted apart from the binding's reads.
    let mut final_tries = 0;
    let mut final_seen = false;
    while let Some(mut t) = tail.take() {
        let m = *mode.borrow_and_update();
        final_seen |= m == Mode::Final;
        match m {
            Mode::Run | Mode::Final => {}
            Mode::Hold => {
                tail = Some(t);
                if mode.changed().await.is_err() {
                    break;
                }
                continue;
            }
            Mode::Stop => {
                tail = Some(t);
                break;
            }
        }
        #[cfg(test)]
        sup.pauses().at(crate::supervisor::Pause::TailerPoll).await;
        let be = backend.clone();
        let at_start = mode.clone();
        // A panic outside the line parser has already requested the daemon's stop.
        let Some((t, step)) = blocking(&sup, move || {
            // A poll that would start once admission is closed waits for the final read.
            if *at_start.borrow() == Mode::Hold {
                #[cfg(test)]
                be.sup
                    .tailers
                    .skipped
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                return (t, None);
            }
            #[cfg(test)]
            be.sup
                .tailers
                .polls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let step = t.poll(&*be);
            (t, Some(step))
        })
        .await
        else {
            break;
        };
        let exited = t.exited();
        tail = Some(t);
        let Some(step) = step else { continue };
        match step {
            Step::End(why) => {
                tracing::info!(%thread, %launch, why = %why, "the transcript tailer ends");
                break;
            }
            // At shutdown only a final read goes on: with the exit stored, or after a view
            // that failed (a few times).
            _ if m == Mode::Final && exited == Some(false) => break,
            Step::Again if m == Mode::Final => {}
            Step::Wait if m == Mode::Final => {
                if exited.is_none() {
                    final_tries += 1;
                    if final_tries >= FINAL_TRIES {
                        break;
                    }
                }
                if !sup.final_wait(&mut mode).await {
                    break;
                }
            }
            Step::Again if again < MAX_AGAIN => {
                again += 1;
            }
            Step::Again | Step::Wait => {
                again = 0;
                sup.tail_wait(thread, None, &mut mode).await;
            }
        }
    }
    if let Some(mut t) = tail {
        // A final read that ended (retries spent, the budget, a stop) before its prefix check.
        if final_seen && !t.prefix_checked() && t.stats.unverified == 0 && t.exited() != Some(false)
        {
            t.unverified("the tailer ended before its final read's prefix check");
        }
        blocking(&sup, move || drop(t)).await;
    }
    finish(&sup, launch);
}

fn finish(sup: &Supervisor, launch: LaunchId) {
    sup.tailers.lock().remove(&launch);
}
