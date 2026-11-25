//! Event builders for the supervisor's jobs: the idempotency keys of mvp §2.1 and a thread
//! job that applies each state input through the reducer and appends its `status_changed`.

use chaldea_core::event::{Source, TypedEvent};
use chaldea_core::ids::{
    Actor, Agent, CommandId, IdempotencyKey, InputId, LaunchId, Open, StreamId, ThreadId,
};
use chaldea_core::input::{InputInput, InputReason, InputState, step};
use chaldea_core::payload::{InputStateChanged, SourceKind};
use chaldea_core::state::{AttentionCause, StateInput, ThreadState, reduce};

use crate::store::{Appended, InputRow, JobError, NewEvent, StoreError, ThreadRow, WriteTx};

/// `launch:<launch_id>:<name>` (mvp §2.1 "Process lifecycle").
pub(crate) fn launch_key(l: LaunchId, name: &str) -> IdempotencyKey {
    IdempotencyKey(format!("launch:{l}:{name}"))
}

/// `command:<command_id>:<n>` (mvp §2.1 "Command").
pub(crate) fn command_key(c: CommandId, n: usize) -> IdempotencyKey {
    IdempotencyKey(format!("command:{c}:{n}"))
}

/// `derived:<cause key>:<name>` (mvp §2.1 "Derived event").
pub(crate) fn derived(cause: &IdempotencyKey, name: &str) -> IdempotencyKey {
    IdempotencyKey(format!("derived:{}:{name}", cause.0))
}

/// A job's events on one thread, with the thread's state as the job has changed it.
pub(crate) struct ThreadJob {
    thread: ThreadId,
    agent: Agent,
    pub(crate) state: ThreadState,
    kind: SourceKind,
    command: Option<CommandId>,
    pending: Vec<NewEvent>,
}

impl ThreadJob {
    pub(crate) fn new(
        thread: ThreadId,
        agent: Agent,
        state: ThreadState,
        kind: SourceKind,
        command: Option<CommandId>,
    ) -> ThreadJob {
        ThreadJob {
            thread,
            agent,
            state,
            kind,
            command,
            pending: Vec::new(),
        }
    }

    fn event(
        &self,
        stream: StreamId,
        key: IdempotencyKey,
        event: TypedEvent,
        launch: Option<LaunchId>,
    ) -> NewEvent {
        NewEvent {
            stream_id: stream,
            idempotency_key: key,
            event,
            thread_id: Some(self.thread),
            launch_id: launch,
            command_id: self.command,
            source: Source {
                kind: Open::Known(self.kind),
                agent: Some(Open::Known(self.agent)),
                cli_version: None,
                transcript: None,
            },
            occurred_at: None,
            annex: None,
            blobs: Vec::new(),
        }
    }

    /// An event on the thread's stream.
    pub(crate) fn push(
        &mut self,
        key: IdempotencyKey,
        event: TypedEvent,
        launch: Option<LaunchId>,
    ) {
        let e = self.event(StreamId::Thread(self.thread), key, event, launch);
        self.pending.push(e);
    }

    /// An event on the thread's stream with the blobs its `Content` references.
    pub(crate) fn push_blobs(
        &mut self,
        key: IdempotencyKey,
        event: TypedEvent,
        launch: Option<LaunchId>,
        blobs: Vec<crate::store::BlobInput>,
    ) {
        let mut e = self.event(StreamId::Thread(self.thread), key, event, launch);
        e.blobs = blobs;
        self.pending.push(e);
    }

    /// An event on the host stream (`thread_registered`, `thread_closed`).
    pub(crate) fn push_host(&mut self, key: IdempotencyKey, event: TypedEvent) {
        let e = self.event(StreamId::Host, key, event, None);
        self.pending.push(e);
    }

    /// Applies `input` through the reducer and queues its `status_changed` events, keyed
    /// `derived:<cause>:status:<name>:<i>`.
    ///
    /// # Errors
    ///
    /// The reducer's refusal, as a job refusal (the job rolls back).
    pub(crate) fn apply(
        &mut self,
        cause: &IdempotencyKey,
        name: &str,
        input: &StateInput,
        launch: Option<LaunchId>,
    ) -> Result<(), JobError> {
        let (next, changes) = reduce(&self.state, input).map_err(refusal)?;
        for (i, c) in changes.into_iter().enumerate() {
            let key = derived(cause, &format!("status:{name}:{i}"));
            self.push(key, TypedEvent::StatusChanged(c), launch);
        }
        self.state = next;
        Ok(())
    }

    /// Whether `cause` is raised.
    pub(crate) fn raised(&self, cause: AttentionCause) -> bool {
        self.state.attention.contains(&cause)
    }

    /// Moves `input` by `by`, keyed under `cause`; `unconfirmed` also raises
    /// `input_unconfirmed` for it.
    ///
    /// # Errors
    ///
    /// The input machine's refusal, as a job refusal.
    pub(crate) fn step_input(
        &mut self,
        cause: &IdempotencyKey,
        input: &InputRow,
        by: &InputInput,
        sent_after_seq: Option<u64>,
        launch: Option<LaunchId>,
    ) -> Result<(), JobError> {
        let to = step(input.state, by).map_err(refusal)?;
        let id = input.input_id;
        self.push(
            derived(cause, &format!("input:{id}")),
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: id,
                from: input.state,
                to,
                reason: InputReason::from(by),
                sent_after_seq,
                by: Some(Actor::Daemon),
            }),
            launch,
        );
        if to == InputState::Unconfirmed {
            self.raise_unconfirmed(cause, id, launch)?;
        }
        Ok(())
    }

    fn raise_unconfirmed(
        &mut self,
        cause: &IdempotencyKey,
        id: InputId,
        launch: Option<LaunchId>,
    ) -> Result<(), JobError> {
        let c = AttentionCause::InputUnconfirmed { input_id: id };
        if self.raised(c) {
            return Ok(());
        }
        let key = derived(cause, &format!("unconfirmed:{id}"));
        self.apply(&key, "raise", &StateInput::AttentionRaised(c), launch)
    }

    /// Appends every queued event, in order.
    ///
    /// # Errors
    ///
    /// The store's refusal.
    pub(crate) fn flush(&mut self, tx: &mut WriteTx<'_>) -> Result<Vec<Appended>, StoreError> {
        let events = std::mem::take(&mut self.pending);
        if events.is_empty() {
            return Ok(Vec::new());
        }
        tx.append(events)
    }
}

/// The thread's row, or a refusal when it has none.
pub(crate) fn thread_row(tx: &WriteTx<'_>, t: ThreadId) -> Result<ThreadRow, JobError> {
    tx.read()
        .thread(t)?
        .ok_or_else(|| refusal("the thread has no row"))
}

/// A rule refusal inside a job: the job rolls back (a programming error of its guards, or
/// a state another job changed in between).
pub(crate) fn refusal(e: impl std::fmt::Display) -> JobError {
    JobError::Refused(e.to_string().into())
}

/// The seq of the appended event keyed `key`.
///
/// # Errors
///
/// A refusal when no such event was appended.
pub(crate) fn seq_of(appended: &[Appended], key: &IdempotencyKey) -> Result<u64, JobError> {
    appended
        .iter()
        .map(Appended::envelope)
        .find(|e| &e.idempotency_key == key)
        .map(|e| e.seq)
        .ok_or_else(|| refusal(format!("{} was not appended", key.0)))
}
