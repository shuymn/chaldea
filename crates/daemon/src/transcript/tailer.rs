//! The tailer's lifecycle: started for a Claude launch at its reservation, it waits for the
//! binding, then polls every [`TAIL_POLL`] until the launch exits (one final read), the
//! binding moves on (`StaleBinding`), or the supervisor's shutdown participant stops and
//! joins it. All file I/O runs on a blocking task, never on the store writer or under a
//! launch's op lock; batches reach the store as write jobs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chaldea_core::ids::{Agent, LaunchId, StreamId, ThreadId};
use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
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

/// A running tailer's stop signal and task.
struct Running {
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
    task: Option<JoinHandle<()>>,
}

/// The supervisor's tailers, by launch.
#[derive(Default)]
pub(crate) struct Tailers {
    map: Mutex<HashMap<LaunchId, Running>>,
}

impl Tailers {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<LaunchId, Running>> {
        self.map.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The launches with a tailer (tests).
    #[cfg(test)]
    pub(crate) fn launches(&self) -> Vec<LaunchId> {
        self.lock().keys().copied().collect()
    }

    /// Stops every tailer and returns their tasks.
    pub(crate) fn stop_all(&self) -> Vec<JoinHandle<()>> {
        let mut map = self.lock();
        map.drain()
            .filter_map(|(_, mut r)| {
                r.stop.store(true, Ordering::Release);
                r.wake.notify_one();
                r.task.take()
            })
            .collect()
    }
}

/// The store side of a running tailer.
struct StoreBackend {
    store: Store,
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

impl Backend for StoreBackend {
    fn view(&self, b: &Binding) -> Result<View, String> {
        let b = b.clone();
        self.store
            .read(Instant::now() + READ_WAIT, move |r| binding_view(r, &b))
            .map_err(|e| format!("{e:?}"))
    }

    fn commit(&self, batch: TranscriptBatch) -> Commit {
        commit(&self.store, batch)
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
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new(Notify::new());
        let mut map = self.tailers.lock();
        if map.contains_key(&launch) || !self.tracker.accepting() {
            return;
        }
        let task = self.rt.spawn(run(
            self.clone(),
            thread,
            launch,
            stop.clone(),
            wake.clone(),
        ));
        map.insert(
            launch,
            Running {
                stop,
                wake,
                task: Some(task),
            },
        );
    }

    fn spool_dir(&self) -> SpoolDir {
        let db = self.store.paths().db;
        SpoolDir(
            db.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf),
        )
    }

    /// Waits a period, a commit naming `thread` when `commits` is given, or the stop.
    async fn tail_wait(
        &self,
        thread: ThreadId,
        commits: Option<&mut tokio::sync::broadcast::Receiver<crate::store::Committed>>,
        wake: &Notify,
    ) {
        let timer = crate::supervisor::tick_timer(self.timer(self.now() + self.cfg.tail_poll));
        tokio::pin!(timer);
        let stream = StreamId::Thread(thread);
        match commits {
            Some(c) => loop {
                tokio::select! {
                    () = &mut timer => return,
                    () = wake.notified() => return,
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
                    () = wake.notified() => {}
                }
            }
        }
    }
}

/// A tailer's task.
async fn run(
    sup: Arc<Supervisor>,
    thread: ThreadId,
    launch: LaunchId,
    stop: Arc<AtomicBool>,
    wake: Arc<Notify>,
) {
    let stopped = || stop.load(Ordering::Acquire);
    // Wait for the binding.
    let mut commits = sup.store.subscribe();
    let b = loop {
        if stopped() {
            return finish(&sup, launch);
        }
        match crate::supervisor::read_bounded(&sup, move |r| bound(r, thread, launch)).await {
            Some(Bound::Yes(b)) => break b,
            Some(Bound::Never) => return finish(&sup, launch),
            Some(Bound::NotYet) | None => {
                if sup.entry(launch).is_none() {
                    return finish(&sup, launch);
                }
            }
        }
        sup.tail_wait(thread, Some(&mut commits), &wake).await;
    };
    drop(commits);
    tracing::info!(%thread, %launch, path = %b.path, "the transcript tailer starts");
    let backend: Arc<dyn Backend + Send + Sync> = Arc::new(StoreBackend {
        store: sup.store.clone(),
    });
    let mut tail = Tail::new(b, sup.spool_dir(), stop.clone());
    let mut again = 0;
    loop {
        if stopped() {
            break;
        }
        let be = backend.clone();
        let polled = sup
            .rt
            .spawn_blocking(move || {
                let step = tail.poll(&*be);
                (tail, step)
            })
            .await;
        // A panic outside the line parser has already requested the daemon's stop.
        let Ok((t, step)) = polled else { break };
        tail = t;
        match step {
            Step::Again if again < MAX_AGAIN => {
                again += 1;
                continue;
            }
            Step::End(why) => {
                tracing::info!(%thread, %launch, why = %why, "the transcript tailer ends");
                break;
            }
            Step::Again | Step::Wait => {
                again = 0;
                sup.tail_wait(thread, None, &wake).await;
            }
        }
    }
    finish(&sup, launch);
}

fn finish(sup: &Supervisor, launch: LaunchId) {
    sup.tailers.lock().remove(&launch);
}
