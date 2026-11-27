//! The tailer's lifecycle: started for a Claude launch at its reservation, it waits for the
//! binding, then polls every [`TAIL_POLL`] until the launch exits (one final read), the
//! binding moves on (`StaleBinding`), or the supervisor's shutdown participant ends it. At
//! shutdown no poll starts once admission closes ([`Mode::Hold`]); after the participant has
//! recorded the launches' exits each tailer makes its final read ([`Mode::Final`]) within the
//! participant's budget, and at the budget's end a read in progress stops at its next chunk
//! ([`Mode::Stop`]). All file I/O and every store read run on a blocking task that the
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
use super::read::{Backend, Binding, Commit, Step, Tail, View};
use crate::store::{JobError, ReadTx, Store, StoreError, TranscriptBatch, WriteError};
use crate::supervisor::Supervisor;

/// The poll period.
pub(crate) const TAIL_POLL: Duration = Duration::from_secs(1);
/// A store read's wait for a connection, and a batch's `not_after`.
const READ_WAIT: Duration = Duration::from_secs(1);
const WRITE_WAIT: Duration = Duration::from_secs(10);
/// Immediate polls in a row before the tailer waits a period anyway.
const MAX_AGAIN: u32 = 64;

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
}

impl Default for Tailers {
    fn default() -> Self {
        Tailers {
            map: Mutex::default(),
            mode: watch::Sender::new(Mode::Run),
            stop: Arc::default(),
        }
    }
}

impl Tailers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<LaunchId, Option<JoinHandle<()>>>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The launches whose tailer has not ended (tests).
    #[cfg(test)]
    pub(crate) fn launches(&self) -> Vec<LaunchId> {
        self.lock().keys().copied().collect()
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

/// A tailer's store read (on the blocking task it runs on).
fn read<R>(
    sup: &Supervisor,
    f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>,
) -> Result<R, String> {
    sup.store
        .read(Instant::now() + READ_WAIT, |r| {
            #[cfg(test)]
            sup.pauses().blocking(crate::supervisor::Pause::TailerRead);
            f(r)
        })
        .map_err(|e| format!("{e:?}"))
}

impl Backend for StoreBackend {
    fn view(&self, b: &Binding) -> Result<View, String> {
        read(&self.sup, |r| binding_view(r, b))
    }

    fn commit(&self, batch: TranscriptBatch) -> Commit {
        commit(&self.sup.store, batch)
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
    // Wait for the binding.
    let mut commits = sup.store.subscribe();
    let b = loop {
        if *mode.borrow_and_update() != Mode::Run {
            return finish(&sup, launch);
        }
        let s = sup.clone();
        match blocking(&sup, move || read(&s, |r| bound(r, thread, launch))).await {
            Some(Ok(Bound::Yes(b))) => break b,
            Some(Ok(Bound::Never)) => return finish(&sup, launch),
            Some(Ok(Bound::NotYet) | Err(_)) | None => {
                if sup.entry(launch).is_none() {
                    return finish(&sup, launch);
                }
            }
        }
        sup.tail_wait(thread, Some(&mut commits), &mut mode).await;
    };
    drop(commits);
    tracing::info!(%thread, %launch, path = %b.path, "the transcript tailer starts");
    let backend = Arc::new(StoreBackend { sup: sup.clone() });
    let mut tail = Some(Tail::new(b, sup.spool_dir(), sup.tailers.stop.clone()));
    let mut again = 0;
    while let Some(mut t) = tail.take() {
        let m = *mode.borrow_and_update();
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
        let be = backend.clone();
        // A panic outside the line parser has already requested the daemon's stop.
        let Some((t, step)) = blocking(&sup, move || {
            let step = t.poll(&*be);
            (t, step)
        })
        .await
        else {
            break;
        };
        let last = t.exited();
        tail = Some(t);
        match step {
            Step::Again if again < MAX_AGAIN => {
                again += 1;
            }
            Step::End(why) => {
                tracing::info!(%thread, %launch, why = %why, "the transcript tailer ends");
                break;
            }
            // At shutdown only a final read is retried.
            Step::Again | Step::Wait if m == Mode::Final && !last => break,
            Step::Again | Step::Wait => {
                again = 0;
                sup.tail_wait(thread, None, &mut mode).await;
            }
        }
    }
    if let Some(t) = tail {
        blocking(&sup, move || drop(t)).await;
    }
    finish(&sup, launch);
}

fn finish(sup: &Supervisor, launch: LaunchId) {
    sup.tailers.lock().remove(&launch);
}
