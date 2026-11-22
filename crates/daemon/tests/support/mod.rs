//! Shared test support (`docs/design/event-store.md` §7): a temp store with a fixed clock
//! and counter ids, event builders, and a history model that emits valid jobs and computes
//! the expected projection values itself (independently of `project::apply`).
//!
//! Used by the integration tests and, through `#[path]`, by the crate's unit tests; it names
//! the crate as `chaldea_daemon` in both.

#![allow(dead_code, reason = "each test binary uses a subset")]
#![allow(clippy::missing_panics_doc, reason = "test support")]
#![allow(
    clippy::result_large_err,
    reason = "test support returns the store's WriteError"
)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chaldea_core::event::{Source, TypedEvent};
use chaldea_core::ids::{
    Actor, Agent, ApprovalId, Epoch, EventId, IdempotencyKey, InputId, LaunchId, Open, StreamId,
    SurfaceId, ThreadId, Timestamp, WorkspaceId, WorktreeId,
};
use chaldea_core::input::{InputInput, InputState, step};
use chaldea_core::payload::{
    ApprovalOutcome, ApprovalRequested, ApprovalResolved, AssistantMessage, Component, Content,
    ExitReason, HumanWrite, HumanWriteOrigin, InputCreated, InputEncoding, InputInitialState,
    InputOrigin, InputStateChanged, LaunchDeathConfirmed, LaunchExited, LaunchReserved,
    LaunchStarted, RawUnknown, RawUnknownReason, ReadinessEvidence, ReadinessObserved,
    SessionBound, SourceKind, SurfaceScope, SurfaceUpdate, TaskSet, ThreadClosed, ThreadRegistered,
    ThreadRole, UserMessage, WorktreeChanged, WorktreeState,
};
use chaldea_core::state::{
    AttentionCause, ProcessState, StateInput, StatusChanged, TaskState, ThreadState, reduce,
};
use chaldea_daemon::store::{
    Appended, BlobInput, Clock, Continuity, CountedClass, CountedClasses, CursorPos, EventAnnex,
    IdSource, JobError, NewEvent, NewGeneration, ReadPos, ReadTx, ShutdownReport, Store,
    StoreConfig, StoreError, TranscriptBatch, TranscriptLine, WriteBasis, WriteError, WriteTx,
    prepare_content,
};
use proptest::prelude::*;
use tempfile::TempDir;

// ---- clock and ids ----------------------------------------------------------------------

pub const T0: &str = "2026-11-22T00:00:00.000Z";

/// A clock whose wall time is fixed (settable) and whose instant is real.
pub struct FixedClock(Mutex<Timestamp>);

impl FixedClock {
    #[must_use]
    pub fn new(ts: &str) -> Arc<Self> {
        Arc::new(FixedClock(Mutex::new(Timestamp(ts.to_owned()))))
    }

    pub fn set(&self, ts: &str) {
        *self.0.lock().unwrap() = Timestamp(ts.to_owned());
    }
}

impl Clock for FixedClock {
    fn now(&self) -> (Instant, Timestamp) {
        (Instant::now(), self.0.lock().unwrap().clone())
    }
}

/// Event ids from a counter; each source has its own range so two stores never collide.
pub struct CounterIds {
    base: u128,
    next: AtomicU64,
}

static SOURCES: AtomicU64 = AtomicU64::new(1);

impl CounterIds {
    #[must_use]
    pub fn new() -> Arc<Self> {
        let n = SOURCES.fetch_add(1, Ordering::SeqCst);
        Arc::new(CounterIds {
            base: u128::from(n) << 64,
            next: AtomicU64::new(1),
        })
    }
}

impl IdSource for CounterIds {
    fn event_id(&self) -> EventId {
        EventId::from_u128(
            0xE000_0000_0000_0000_0000_0000_0000_0000
                | self.base
                | u128::from(self.next.fetch_add(1, Ordering::SeqCst)),
        )
    }

    fn epoch(&self) -> Epoch {
        Epoch(format!("epoch-{:x}", self.base >> 64))
    }
}

#[must_use]
pub fn config(path: PathBuf, clock: Arc<FixedClock>) -> StoreConfig {
    StoreConfig {
        path,
        readers: 4,
        writer_queue: 256,
        busy_timeout: Duration::from_secs(5),
        clock,
        ids: CounterIds::new(),
    }
}

#[must_use]
pub fn soon() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

// ---- a temp store -------------------------------------------------------------------------

/// A store in its own temp directory, shut down before the directory is removed (Windows
/// cannot delete open files).
pub struct TestStore {
    pub store: Option<Store>,
    pub clock: Arc<FixedClock>,
    pub dir: TempDir,
}

impl TestStore {
    #[must_use]
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = FixedClock::new(T0);
        let store = Store::open(config(dir.path().join("chaldea.db"), clock.clone())).unwrap();
        TestStore {
            store: Some(store),
            clock,
            dir,
        }
    }

    #[must_use]
    pub fn store(&self) -> &Store {
        self.store.as_ref().unwrap()
    }

    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.dir.path().join("chaldea.db")
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Runs one job and waits for it.
    pub fn run<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut WriteTx<'_>) -> Result<R, JobError> + Send + 'static,
    ) -> Result<R, WriteError> {
        self.store().submit(soon(), f).unwrap().blocking_wait()
    }

    pub fn append(&self, events: Vec<NewEvent>) -> Result<Vec<Appended>, WriteError> {
        self.run(move |tx| Ok(tx.append(events)?))
    }

    pub fn read<R>(&self, f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>) -> R {
        self.store().read(soon(), f).unwrap()
    }

    pub fn shutdown(&mut self) -> ShutdownReport {
        let report = self
            .store
            .take()
            .unwrap()
            .shutdown(Instant::now() + Duration::from_secs(20));
        assert!(report.writer_exited, "{report:?}");
        report
    }

    pub fn reopen(&mut self) {
        if self.store.is_some() {
            self.shutdown();
        }
        self.store = Some(Store::open(config(self.path(), self.clock.clone())).unwrap());
    }
}

impl Default for TestStore {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TestStore {
    fn drop(&mut self) {
        if let Some(s) = self.store.take() {
            let _ = s.shutdown(Instant::now() + Duration::from_secs(20));
        }
    }
}

// ---- builders -----------------------------------------------------------------------------

#[must_use]
pub fn thread(n: u128) -> ThreadId {
    ThreadId::from_u128(0x7000_0000_0000_0000_0000_0000_0000_0000 | n)
}

#[must_use]
pub fn launch(n: u128) -> LaunchId {
    LaunchId::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0000 | n)
}

#[must_use]
pub fn input(n: u128) -> InputId {
    InputId::from_u128(0x2000_0000_0000_0000_0000_0000_0000_0000 | n)
}

#[must_use]
pub fn approval(n: u128) -> ApprovalId {
    ApprovalId::from_u128(0x3000_0000_0000_0000_0000_0000_0000_0000 | n)
}

#[must_use]
pub fn source(kind: SourceKind) -> Source {
    Source {
        kind: Open::Known(kind),
        agent: None,
        cli_version: None,
        transcript: None,
    }
}

/// A new event whose envelope ids come from the stream and the payload's `thread_id` and
/// `launch_id`, with a daemon source and no source time.
#[must_use]
pub fn event(stream: StreamId, key: &str, ev: TypedEvent) -> NewEvent {
    let payload: serde_json::Value = serde_json::from_str(ev.to_payload().unwrap().get()).unwrap();
    let id = |k: &str| {
        payload
            .get(k)
            .and_then(serde_json::Value::as_str)
            .map(|s| uuid::Uuid::parse_str(s).unwrap())
    };
    let thread_id = match stream {
        StreamId::Thread(t) => Some(t),
        StreamId::Host => id("thread_id").map(ThreadId),
    };
    NewEvent {
        stream_id: stream,
        idempotency_key: IdempotencyKey(key.to_owned()),
        event: ev,
        thread_id,
        launch_id: id("launch_id").map(LaunchId),
        command_id: None,
        source: source(SourceKind::Daemon),
        occurred_at: None,
        annex: None,
        blobs: Vec::new(),
    }
}

#[must_use]
pub fn registered(t: ThreadId, task: bool) -> NewEvent {
    event(
        StreamId::Host,
        &format!("register:{t}"),
        TypedEvent::ThreadRegistered(ThreadRegistered {
            thread_id: t,
            agent: Agent::Claude,
            cwd: "/work/repo".into(),
            workspace_id: WorkspaceId("ws".into()),
            parent_thread_id: None,
            role: ThreadRole::Standard,
            task: task.then(|| "the task".to_owned()),
        }),
    )
}

#[must_use]
pub fn reserved(t: ThreadId, l: LaunchId) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("launch:{l}:reserved"),
        TypedEvent::LaunchReserved(LaunchReserved {
            launch_id: l,
            expected_previous: None,
            agent: Agent::Claude,
            argv_digest: "0".repeat(64),
            manual: false,
            worktree_id: None,
        }),
    )
}

#[must_use]
pub fn started(t: ThreadId, l: LaunchId) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("launch:{l}:started"),
        TypedEvent::LaunchStarted(LaunchStarted {
            launch_id: l,
            pid: 4242,
            start_time: "start".into(),
            pgid_or_job: "4242".into(),
            cli_version: Some("1.0.0".into()),
        }),
    )
}

#[must_use]
pub fn bound(t: ThreadId, l: LaunchId, session: &str, path: Option<&str>) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("bound:{l}:{session}"),
        TypedEvent::SessionBound(SessionBound {
            launch_id: l,
            session_id: session.into(),
            transcript_path: path.map(str::to_owned),
        }),
    )
}

#[must_use]
pub fn exited(t: ThreadId, l: LaunchId) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("launch:{l}:exited"),
        TypedEvent::LaunchExited(LaunchExited {
            launch_id: l,
            code: Some(0),
            signal: None,
            reason: ExitReason::Exit,
            death_confirmed: true,
        }),
    )
}

/// A `raw_unknown` on a stream with the given content.
#[must_use]
pub fn raw_unknown(stream: StreamId, key: &str, raw: Content) -> NewEvent {
    event(
        stream,
        key,
        TypedEvent::RawUnknown(RawUnknown {
            source: SourceKind::Hook,
            reason: RawUnknownReason::Unparseable,
            agent: None,
            cli_version: None,
            content_type: "application/json".into(),
            raw,
        }),
    )
}

/// A thread registered, reserved, started, and bound to `session` and `path`.
pub fn bound_thread(s: &TestStore, t: ThreadId, l: LaunchId, path: Option<&str>) {
    s.append(vec![registered(t, false)]).unwrap();
    s.append(vec![reserved(t, l), started(t, l), bound(t, l, "s1", path)])
        .unwrap();
}

#[must_use]
pub fn cont(s: &str) -> Continuity {
    Continuity(s.to_owned())
}

#[must_use]
pub fn assistant(text: &str) -> TypedEvent {
    TypedEvent::AssistantMessage(AssistantMessage {
        text: Content::text(text),
        turn_ref: None,
    })
}

/// A transcript line of `assistant_message` text.
#[must_use]
pub fn line(start: u64, end: u64, text: &str) -> TranscriptLine {
    TranscriptLine {
        offset_start: start,
        offset_end: end,
        event: assistant(text),
        occurred_at: None,
        continuity: cont(&format!("c{end}")),
        blobs: Vec::new(),
    }
}

/// A batch for `t` bound to `l` and session `s1` on `path`.
#[must_use]
pub fn batch(t: ThreadId, l: LaunchId, path: &str, expect: Option<CursorPos>) -> TranscriptBatch {
    TranscriptBatch {
        thread_id: t,
        launch_id: l,
        session_id: "s1".into(),
        path: path.into(),
        agent: Agent::Claude,
        cli_version: Some("1.0.0".into()),
        expect,
        new_generation: None,
        lines: Vec::new(),
        advance_to: ReadPos {
            read_offset: 0,
            continuity: cont("c0"),
        },
    }
}

#[must_use]
pub fn pos(generation: u64, path: &str, file_id: &str, read_offset: u64) -> CursorPos {
    CursorPos {
        generation,
        path: path.into(),
        file_id: file_id.into(),
        read_offset,
    }
}

#[must_use]
pub fn new_generation(file_id: &str) -> NewGeneration {
    NewGeneration {
        file_id: file_id.into(),
        continuity: cont(&format!("g-{file_id}")),
    }
}

// ---- the history model --------------------------------------------------------------------

/// One operation of a job.
#[expect(clippy::large_enum_variant, reason = "test operations, moved once")]
#[derive(Debug, Clone)]
pub enum Op {
    Append(Vec<NewEvent>),
    Transcript(TranscriptBatch),
    /// An event of an unknown kind on a thread stream, inserted with raw SQL (unit tests).
    RawUnknownKind {
        thread: ThreadId,
        key: String,
    },
    Rebuild,
}

/// A job and whether the model expects it to commit.
#[derive(Debug, Clone)]
pub struct Job {
    pub ops: Vec<Op>,
    pub commits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MCursor {
    pub generation: u64,
    pub path: String,
    pub file_id: String,
    pub event_offset: u64,
    pub event_continuity: String,
    pub read_offset: u64,
    pub read_continuity: String,
}

#[derive(Debug, Clone)]
pub struct MThread {
    pub id: ThreadId,
    pub state: ThreadState,
    pub current_launch: Option<LaunchId>,
    pub session: Option<String>,
    pub path: Option<String>,
    pub seq: u64,
    pub cursor: Option<MCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MLaunch {
    pub thread: ThreadId,
    pub session: Option<String>,
    pub readiness: Option<(u64, ReadinessEvidence)>,
    pub basis: Option<WriteBasis>,
    pub last_write: Option<u64>,
    pub last_human: Option<u64>,
    pub last_turn_start: Option<u64>,
    pub exited: bool,
    pub death_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MInput {
    pub thread: ThreadId,
    pub launch: Option<LaunchId>,
    pub state: InputState,
}

/// Per thread, what the current job did to its transcript.
#[derive(Debug, Clone, Copy, Default)]
struct JobTranscript {
    started: bool,
    continued: bool,
}

/// The model: the expected projection values, computed from the steps themselves.
#[derive(Debug, Clone, Default)]
pub struct Model {
    pub threads: BTreeMap<ThreadId, MThread>,
    pub launches: BTreeMap<LaunchId, MLaunch>,
    pub inputs: BTreeMap<InputId, MInput>,
    pub open_approvals: BTreeMap<ApprovalId, ThreadId>,
    pub host_seq: u64,
    pub events: usize,
    next: u128,
    ops: Vec<Op>,
    fails: bool,
    job_transcripts: BTreeMap<ThreadId, JobTranscript>,
    /// Committed events that a retry may resubmit.
    retryable: Vec<NewEvent>,
    pending_retryable: Vec<NewEvent>,
}

impl Model {
    fn id(&mut self) -> u128 {
        self.next += 1;
        self.next
    }

    fn key(&mut self, what: &str) -> String {
        let n = self.id();
        format!("command:{what}:{n}")
    }

    /// Takes the job built since the last call.
    pub fn take_job(&mut self) -> Job {
        self.job_transcripts.clear();
        let job = Job {
            ops: std::mem::take(&mut self.ops),
            commits: !std::mem::replace(&mut self.fails, false),
        };
        if job.commits {
            self.retryable.append(&mut self.pending_retryable);
        } else {
            self.pending_retryable.clear();
        }
        job
    }

    fn push(&mut self, e: NewEvent) {
        let seq = match e.stream_id {
            StreamId::Host => {
                self.host_seq += 1;
                self.host_seq
            }
            StreamId::Thread(t) => {
                let th = self.threads.get_mut(&t).expect("thread");
                th.seq += 1;
                th.seq
            }
        };
        let _ = seq;
        self.events += 1;
        self.pending_retryable.push(e.clone());
        match self.ops.last_mut() {
            Some(Op::Append(v)) => v.push(e),
            _ => self.ops.push(Op::Append(vec![e])),
        }
    }

    fn next_seq(&self, t: ThreadId) -> u64 {
        self.threads[&t].seq + 1
    }

    /// Applies a reducer input and pushes its `status_changed` events.
    fn reduce(&mut self, t: ThreadId, input: &StateInput) -> bool {
        let state = self.threads[&t].state.clone();
        let Ok((next, changes)) = reduce(&state, input) else {
            return false;
        };
        for c in changes {
            let k = self.key("status");
            self.push(event(StreamId::Thread(t), &k, TypedEvent::StatusChanged(c)));
        }
        self.threads.get_mut(&t).unwrap().state = next;
        true
    }

    pub fn register(&mut self, task: bool) -> ThreadId {
        let t = thread(self.id());
        self.threads.insert(
            t,
            MThread {
                id: t,
                state: ThreadState::registered(task),
                current_launch: None,
                session: None,
                path: None,
                seq: 0,
                cursor: None,
            },
        );
        self.push(registered(t, task));
        t
    }

    pub fn reserve(&mut self, t: ThreadId) -> Option<LaunchId> {
        let relaunch = self.threads[&t].current_launch.is_some();
        let state = self.threads[&t].state.clone();
        reduce(&state, &StateInput::Reserved { relaunch }).ok()?;
        let l = launch(self.id());
        self.push(reserved(t, l));
        self.reduce(t, &StateInput::Reserved { relaunch });
        self.threads.get_mut(&t).unwrap().current_launch = Some(l);
        self.launches.insert(
            l,
            MLaunch {
                thread: t,
                session: None,
                readiness: None,
                basis: None,
                last_write: None,
                last_human: None,
                last_turn_start: None,
                exited: false,
                death_confirmed: false,
            },
        );
        Some(l)
    }

    fn current(&self, t: ThreadId) -> Option<LaunchId> {
        self.threads[&t].current_launch
    }

    fn can(&self, t: ThreadId, input: &StateInput) -> bool {
        reduce(&self.threads[&t].state, input).is_ok()
    }

    pub fn start(&mut self, t: ThreadId) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.can(t, &StateInput::Spawned) {
            return false;
        }
        self.push(started(t, l));
        self.reduce(t, &StateInput::Spawned)
    }

    pub fn bind(&mut self, t: ThreadId, path: Option<String>) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.can(t, &StateInput::SessionBound) || self.launches[&l].session.is_some() {
            return false;
        }
        let session = format!("session-{}", self.id());
        let seq = self.next_seq(t);
        let k = self.key("bind");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::SessionBound(SessionBound {
                launch_id: l,
                session_id: session.clone(),
                transcript_path: path.clone(),
            }),
        ));
        let m = self.launches.get_mut(&l).unwrap();
        m.session = Some(session.clone());
        m.last_turn_start = Some(seq);
        let th = self.threads.get_mut(&t).unwrap();
        th.session = Some(session);
        th.path = path;
        self.reduce(t, &StateInput::SessionBound)
    }

    pub fn readiness(
        &mut self,
        t: ThreadId,
        evidence: ReadinessEvidence,
        position: u64,
        counted: u8,
    ) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.can(t, &StateInput::ReadinessObserved) {
            return false;
        }
        let classes: Vec<CountedClass> = [
            CountedClass::Input,
            CountedClass::Human,
            CountedClass::Reply,
        ]
        .into_iter()
        .enumerate()
        .filter(|(i, _)| counted & (1 << i) != 0)
        .map(|(_, c)| c)
        .collect();
        let basis = WriteBasis {
            launch_id: l,
            position,
            counted: CountedClasses::of(&classes),
        };
        let seq = self.next_seq(t);
        let k = self.key("ready");
        let mut e = event(
            StreamId::Thread(t),
            &k,
            TypedEvent::ReadinessObserved(ReadinessObserved {
                launch_id: l,
                evidence,
                by: Actor::Daemon,
            }),
        );
        e.annex = Some(EventAnnex::Readiness(basis.clone()));
        self.push(e);
        let m = self.launches.get_mut(&l).unwrap();
        m.readiness = Some((seq, evidence));
        m.basis = Some(basis);
        self.reduce(t, &StateInput::ReadinessObserved)
    }

    pub fn human_write(&mut self, t: ThreadId, origin: HumanWriteOrigin) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        let seq = self.next_seq(t);
        let k = self.key("human");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::HumanWrite(HumanWrite {
                launch_id: l,
                origin,
                approval_id: None,
            }),
        ));
        let m = self.launches.get_mut(&l).unwrap();
        m.last_write = Some(seq);
        if matches!(origin, HumanWriteOrigin::Attach | HumanWriteOrigin::Key) {
            m.last_human = Some(seq);
        }
        true
    }

    pub fn prompt(&mut self, t: ThreadId, big: bool) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.can(t, &StateInput::PromptSubmitted) {
            return false;
        }
        let n = self.id();
        let (prompt, blobs) = if big {
            let text = format!("{n}:{}", "あ".repeat(30_000));
            let p = prepare_content(text.into_bytes(), "text/plain; charset=utf-8").unwrap();
            (p.content, p.blob.into_iter().collect())
        } else {
            (Content::text(format!("prompt {n}")), Vec::new())
        };
        let seq = self.next_seq(t);
        let k = self.key("prompt");
        let mut e = event(
            StreamId::Thread(t),
            &k,
            TypedEvent::UserMessage(UserMessage {
                prompt,
                input_id: None,
            }),
        );
        e.launch_id = Some(l);
        e.blobs = blobs;
        self.push(e);
        self.launches.get_mut(&l).unwrap().last_turn_start = Some(seq);
        self.reduce(t, &StateInput::PromptSubmitted)
    }

    pub fn exit(&mut self, t: ThreadId, confirmed: bool) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        let input = StateInput::Exited {
            reason: ExitReason::Exit,
        };
        if !self.can(t, &input) {
            return false;
        }
        let k = self.key("exit");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::LaunchExited(LaunchExited {
                launch_id: l,
                code: Some(0),
                signal: None,
                reason: ExitReason::Exit,
                death_confirmed: confirmed,
            }),
        ));
        let m = self.launches.get_mut(&l).unwrap();
        m.exited = true;
        m.death_confirmed = confirmed;
        self.reduce(t, &input)
    }

    pub fn confirm_death(&mut self, t: ThreadId) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.launches[&l].exited || self.launches[&l].death_confirmed {
            return false;
        }
        let k = self.key("death");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::LaunchDeathConfirmed(LaunchDeathConfirmed {
                launch_id: l,
                method: "scan".into(),
            }),
        ));
        self.launches.get_mut(&l).unwrap().death_confirmed = true;
        self.reduce(t, &StateInput::DeathConfirmed)
    }

    pub fn task_set(&mut self, t: ThreadId, to: TaskState) -> bool {
        let k = self.key("task");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::TaskSet(TaskSet {
                to,
                by: Actor::Human,
                summary: None,
            }),
        ));
        self.reduce(
            t,
            &StateInput::TaskSet {
                to,
                by: Actor::Human,
            },
        )
    }

    pub fn attention(&mut self, t: ThreadId, cause: AttentionCause, raise: bool) -> bool {
        let input = if raise {
            StateInput::AttentionRaised(cause)
        } else {
            StateInput::AttentionCleared(cause)
        };
        self.can(t, &input) && self.reduce(t, &input)
    }

    pub fn create_input(&mut self, t: ThreadId, with_launch: bool) -> InputId {
        let i = input(self.id());
        let l = if with_launch { self.current(t) } else { None };
        let k = self.key("input");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::InputCreated(InputCreated {
                input_id: i,
                launch_id: l,
                initial_state: InputInitialState::Queued,
                text: Content::text("hello"),
                encoding: InputEncoding::BracketedPaste,
                origin: InputOrigin::Cli,
                created_by: Actor::Human,
            }),
        ));
        self.inputs.insert(
            i,
            MInput {
                thread: t,
                launch: l,
                state: InputState::Queued,
            },
        );
        i
    }

    /// Steps an input with `how`; to `sending` the envelope names the thread's current launch.
    pub fn step_input(&mut self, i: InputId, how: &InputInput) -> bool {
        let m = self.inputs[&i].clone();
        let Ok(to) = step(m.state, how) else {
            return false;
        };
        let t = m.thread;
        let launch = if to == InputState::Sending {
            match m.launch.or(self.current(t)) {
                Some(l) if self.launches[&l].thread == t => Some(l),
                _ => return false,
            }
        } else {
            None
        };
        let seq = self.next_seq(t);
        let k = self.key("input-step");
        let mut e = event(
            StreamId::Thread(t),
            &k,
            TypedEvent::InputStateChanged(InputStateChanged {
                input_id: i,
                from: m.state,
                to,
                reason: how.into(),
                sent_after_seq: matches!(how, InputInput::GateOpened { .. }).then_some(seq - 1),
                by: None,
            }),
        );
        if to == InputState::Sending {
            e.launch_id = self.current(t);
        }
        self.push(e);
        let mi = self.inputs.get_mut(&i).unwrap();
        mi.state = to;
        if let Some(l) = launch {
            mi.launch = Some(l);
            self.launches.get_mut(&l).unwrap().last_write = Some(seq);
        }
        true
    }

    pub fn request_approval(&mut self, t: ThreadId) -> bool {
        let Some(l) = self.current(t) else {
            return false;
        };
        if !self.can(t, &StateInput::ApprovalOpened) {
            return false;
        }
        let a = approval(self.id());
        let k = self.key("approval");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::ApprovalRequested(ApprovalRequested {
                approval_id: a,
                launch_id: l,
                turn_ref: None,
                tool_name: "Bash".into(),
                tool_input: Content::text("ls"),
                correlation: None,
            }),
        ));
        self.open_approvals.insert(a, t);
        self.reduce(t, &StateInput::ApprovalOpened)
    }

    pub fn resolve_approval(&mut self, pick: usize, outcome: ApprovalOutcome) -> bool {
        let Some((&a, &t)) = self
            .open_approvals
            .iter()
            .nth(pick % self.open_approvals.len().max(1))
        else {
            return false;
        };
        let k = self.key("resolve");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::ApprovalResolved(ApprovalResolved {
                approval_id: a,
                outcome,
            }),
        ));
        self.open_approvals.remove(&a);
        true
    }

    pub fn surface(&mut self, t: Option<ThreadId>, n: u8, empty: bool) {
        let (stream, scope) = match t {
            Some(t) => (StreamId::Thread(t), SurfaceScope::Thread { thread_id: t }),
            None => (StreamId::Host, SurfaceScope::Host),
        };
        let components = if empty {
            Vec::new()
        } else {
            vec![Component::Text {
                text: format!("panel {n}"),
            }]
        };
        let k = self.key("surface");
        self.push(event(
            stream,
            &k,
            TypedEvent::SurfaceUpdate(SurfaceUpdate {
                surface_id: SurfaceId(format!("s{}", n % 3)),
                scope,
                components,
            }),
        ));
    }

    pub fn worktree(&mut self, n: u8, state: WorktreeState) {
        let k = self.key("worktree");
        self.push(event(
            StreamId::Host,
            &k,
            TypedEvent::WorktreeChanged(WorktreeChanged {
                worktree_id: WorktreeId(format!("{:08x}", n % 3)),
                repo: "/work/repo".into(),
                workspace_id: WorkspaceId("ws".into()),
                path: "/work/wt".into(),
                branch: "chaldea/x".into(),
                base_ref: "main".into(),
                base_commit: "0".repeat(40),
                owner_thread_id: None,
                created_by: Actor::Human,
                state,
            }),
        ));
    }

    pub fn close(&mut self, t: ThreadId) {
        let k = self.key("close");
        self.push(event(
            StreamId::Host,
            &k,
            TypedEvent::ThreadClosed(ThreadClosed {
                thread_id: t,
                by: Actor::Human,
            }),
        ));
    }

    /// Whether the thread's current launch is bound with a transcript path.
    fn transcript_binding(&self, t: ThreadId) -> Option<(LaunchId, String, String)> {
        let th = &self.threads[&t];
        let l = th.current_launch?;
        let session = self.launches[&l].session.clone()?;
        Some((l, session, th.path.clone()?))
    }

    fn transcript_batch(
        &self,
        t: ThreadId,
        l: LaunchId,
        session: String,
        path: String,
    ) -> TranscriptBatch {
        let th = &self.threads[&t];
        TranscriptBatch {
            thread_id: t,
            launch_id: l,
            session_id: session,
            path,
            agent: Agent::Claude,
            cli_version: None,
            expect: th.cursor.as_ref().map(|c| CursorPos {
                generation: c.generation,
                path: c.path.clone(),
                file_id: c.file_id.clone(),
                read_offset: c.read_offset,
            }),
            new_generation: None,
            lines: Vec::new(),
            advance_to: ReadPos {
                read_offset: 0,
                continuity: cont("r0"),
            },
        }
    }

    /// Starts a new generation on the thread's bound path.
    pub fn generation(&mut self, t: ThreadId) -> bool {
        let Some((l, session, path)) = self.transcript_binding(t) else {
            return false;
        };
        let file_id = format!("f{}", self.id());
        let mut b = self.transcript_batch(t, l, session, path.clone());
        let g = NewGeneration {
            file_id: file_id.clone(),
            continuity: cont(&format!("g-{file_id}")),
        };
        b.new_generation = Some(g.clone());
        b.advance_to.continuity = g.continuity.clone();
        let jt = self.job_transcripts.entry(t).or_default();
        if jt.continued {
            self.fails = true;
        }
        jt.started = true;
        let generation = self.threads[&t]
            .cursor
            .as_ref()
            .map_or(1, |c| c.generation + 1);
        self.events += 1;
        self.threads.get_mut(&t).unwrap().seq += 1;
        self.threads.get_mut(&t).unwrap().cursor = Some(MCursor {
            generation,
            path,
            file_id,
            event_offset: 0,
            event_continuity: g.continuity.0.clone(),
            read_offset: 0,
            read_continuity: g.continuity.0,
        });
        self.ops.push(Op::Transcript(b));
        true
    }

    /// Appends `lines` lines (none: an advance without events) to the current generation.
    pub fn lines(&mut self, t: ThreadId, lines: u8, gap: u8, big: bool) -> bool {
        let Some((l, session, path)) = self.transcript_binding(t) else {
            return false;
        };
        let Some(c) = self.threads[&t].cursor.clone() else {
            return false;
        };
        if c.path != path {
            return false;
        }
        let mut b = self.transcript_batch(t, l, session, path);
        let mut at = c.read_offset + u64::from(gap % 3);
        let mut cursor = c.clone();
        for i in 0..lines {
            let n = self.id();
            let len = 10 + u64::from(i);
            let (text, blobs) = if big && i == 0 {
                let p = prepare_content(vec![b'x'; 70_000], "text/plain; charset=utf-8").unwrap();
                (p.content, p.blob.into_iter().collect())
            } else {
                (Content::text(format!("line {n}")), Vec::new())
            };
            b.lines.push(TranscriptLine {
                offset_start: at,
                offset_end: at + len,
                event: TypedEvent::AssistantMessage(AssistantMessage {
                    text,
                    turn_ref: None,
                }),
                occurred_at: None,
                continuity: cont(&format!("c{n}")),
                blobs,
            });
            at += len;
            cursor.event_offset = at;
            cursor.event_continuity = format!("c{n}");
            self.events += 1;
            self.threads.get_mut(&t).unwrap().seq += 1;
        }
        let advance = at + u64::from(gap % 2) * 7;
        let rc = format!("r{}", self.id());
        b.advance_to = ReadPos {
            read_offset: advance,
            continuity: cont(&rc),
        };
        cursor.read_offset = advance;
        cursor.read_continuity = rc;
        let jt = self.job_transcripts.entry(t).or_default();
        if jt.started {
            self.fails = true;
        }
        jt.continued = true;
        self.threads.get_mut(&t).unwrap().cursor = Some(cursor);
        self.ops.push(Op::Transcript(b));
        true
    }

    /// Resubmits a committed event: the store answers `Existing` and changes nothing.
    pub fn retry(&mut self, pick: usize) -> bool {
        if self.retryable.is_empty() {
            return false;
        }
        let e = self.retryable[pick % self.retryable.len()].clone();
        match self.ops.last_mut() {
            Some(Op::Append(v)) => v.push(e),
            _ => self.ops.push(Op::Append(vec![e])),
        }
        true
    }

    /// A `status_changed` whose `from` the projection does not hold: the job fails.
    pub fn fail(&mut self, t: ThreadId) {
        let wrong = match self.threads[&t].state.process {
            ProcessState::Exited => ProcessState::Running,
            _ => ProcessState::Exited,
        };
        let k = self.key("bad");
        self.push(event(
            StreamId::Thread(t),
            &k,
            TypedEvent::StatusChanged(StatusChanged::ProcessState {
                from: wrong,
                to: ProcessState::Starting,
                cause: chaldea_core::state::StatusCause::Reserved,
            }),
        ));
        self.fails = true;
    }

    pub fn unknown_kind(&mut self, t: ThreadId) {
        let k = self.key("unknown");
        self.events += 1;
        self.threads.get_mut(&t).unwrap().seq += 1;
        self.ops.push(Op::RawUnknownKind { thread: t, key: k });
    }

    pub fn rebuild(&mut self) {
        for th in self.threads.values_mut() {
            if let Some(c) = &mut th.cursor {
                c.read_offset = c.event_offset;
                c.read_continuity = c.event_continuity.clone();
            }
        }
        self.ops.push(Op::Rebuild);
    }

    fn pick_thread(&self, n: u8) -> Option<ThreadId> {
        let ids: Vec<ThreadId> = self.threads.keys().copied().collect();
        (!ids.is_empty()).then(|| ids[n as usize % ids.len()])
    }

    /// The next lifecycle step of the thread: reserve, start, or bind (mostly with a path).
    fn lifecycle(&mut self, t: ThreadId, b: u8) {
        let process = self.threads[&t].state.process;
        let bound = self
            .current(t)
            .is_some_and(|l| self.launches[&l].session.is_some());
        match process {
            ProcessState::Exited => {
                if self.reserve(t).is_none() {
                    // A blocked resume: confirm the old launch's death first.
                    self.confirm_death(t);
                }
            }
            ProcessState::Starting => {
                self.start(t);
            }
            ProcessState::Running | ProcessState::Stopping if !bound => {
                let path = match b % 4 {
                    0 => None,
                    1 | 2 => Some("/work/t/a.jsonl".to_owned()),
                    _ => Some(format!("/work/t/{b}.jsonl")),
                };
                self.bind(t, path);
            }
            _ => {
                self.exit(t, !b.is_multiple_of(3));
            }
        }
    }

    /// A transcript step: a generation when the cursor is missing or on another path (or,
    /// rarely, a swapped file), else lines or an advance without events.
    fn transcript(&mut self, t: ThreadId, b: u8) {
        let Some((_, _, path)) = self.transcript_binding(t) else {
            self.lifecycle(t, b);
            return;
        };
        let needs_generation = self.threads[&t]
            .cursor
            .as_ref()
            .is_none_or(|c| c.path != path);
        if needs_generation || b.is_multiple_of(13) {
            self.generation(t);
        } else {
            self.lines(t, b % 4, b / 4, b.is_multiple_of(17));
        }
    }

    /// Interprets one generated step; invalid ones are skipped.
    pub fn step(&mut self, s: Step, with_unknown: bool, with_rebuild: bool) {
        let Some(t) = self.pick_thread(s.a) else {
            self.register(s.b.is_multiple_of(2));
            return;
        };
        match s.op % 22 {
            0 => {
                if self.threads.len() < 4 {
                    self.register(s.b.is_multiple_of(2));
                } else {
                    self.lifecycle(t, s.b);
                }
            }
            1..=3 => self.lifecycle(t, s.b),
            4 => {
                let evidence = [
                    ReadinessEvidence::SessionStart,
                    ReadinessEvidence::StopSettled,
                    ReadinessEvidence::OwnerAsserted,
                ][s.b as usize % 3];
                self.readiness(t, evidence, u64::from(s.b) * 3, s.b % 8);
            }
            5 => {
                let origin = [
                    HumanWriteOrigin::Attach,
                    HumanWriteOrigin::Key,
                    HumanWriteOrigin::CardKey,
                ][s.b as usize % 3];
                self.human_write(t, origin);
            }
            6 => {
                self.prompt(t, s.b.is_multiple_of(7));
            }
            7 => {
                if !self.confirm_death(t) {
                    self.exit(t, s.b.is_multiple_of(2));
                }
            }
            8 => {
                let to = [
                    TaskState::None,
                    TaskState::Active,
                    TaskState::Blocked,
                    TaskState::Completed,
                    TaskState::Failed,
                    TaskState::Cancelled,
                ][s.b as usize % 6];
                self.task_set(t, to);
            }
            9 => {
                let causes: Vec<AttentionCause> =
                    self.threads[&t].state.attention.iter().copied().collect();
                if s.b.is_multiple_of(3)
                    && let Some(c) = causes.into_iter().find(|c| c.cleared_explicitly())
                {
                    self.attention(t, c, false);
                } else {
                    let ids: Vec<InputId> = self.inputs.keys().copied().collect();
                    let cause = match (s.b % 4, ids.first()) {
                        (0, _) => AttentionCause::SessionStartTimeout,
                        (1, _) => AttentionCause::ResumeBlocked,
                        (2, Some(&i)) => AttentionCause::InputGateTimeout { input_id: i },
                        _ => AttentionCause::StartupStall,
                    };
                    self.attention(t, cause, true);
                }
            }
            10 => {
                self.create_input(t, s.b.is_multiple_of(2));
            }
            11 => {
                let ids: Vec<InputId> = self
                    .inputs
                    .iter()
                    .filter(|(_, m)| m.thread == t)
                    .map(|(i, _)| *i)
                    .collect();
                if let Some(&i) = ids.get(s.b as usize % ids.len().max(1)) {
                    let how = [
                        InputInput::GateOpened { sent_after_seq: 0 },
                        InputInput::WriteCompleted,
                        InputInput::AckAttributed,
                        InputInput::WriteOutcomeUnknown,
                        InputInput::Cancelled,
                        InputInput::Resolved { delivered: false },
                    ][s.b as usize % 6];
                    self.step_input(i, &how);
                }
            }
            12 => {
                if s.b.is_multiple_of(2) {
                    self.request_approval(t);
                } else {
                    let outcome = [
                        ApprovalOutcome::Ran,
                        ApprovalOutcome::Expired,
                        ApprovalOutcome::Superseded,
                    ][s.b as usize % 3];
                    self.resolve_approval(s.b as usize, outcome);
                }
            }
            13 => match s.b % 8 {
                0..=2 => self.surface(Some(t), s.b, s.b.is_multiple_of(5)),
                3 | 4 => self.surface(None, s.b, s.b.is_multiple_of(5)),
                5 | 6 => {
                    let state = [
                        WorktreeState::Creating,
                        WorktreeState::Active,
                        WorktreeState::Removed,
                    ][s.b as usize % 3];
                    self.worktree(s.b, state);
                }
                _ => self.close(t),
            },
            14..=17 => self.transcript(t, s.b),
            18 => {
                self.retry(s.b as usize);
            }
            19 => {
                if s.b.is_multiple_of(3) {
                    self.fail(t);
                } else if with_unknown {
                    self.unknown_kind(t);
                }
            }
            20 => {
                if with_rebuild && s.b.is_multiple_of(2) {
                    self.rebuild();
                }
            }
            _ => {
                self.prompt(t, false);
            }
        }
    }
}

/// One generated step: an operation code and two parameters.
#[derive(Debug, Clone, Copy)]
pub struct Step {
    pub op: u8,
    pub a: u8,
    pub b: u8,
    /// Ends the current job after this step.
    pub cut: bool,
}

/// Histories of jobs: steps grouped into jobs at the `cut` marks.
pub fn steps() -> impl Strategy<Value = Vec<Step>> {
    prop::collection::vec(
        (
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            prop::bool::weighted(0.4),
        )
            .prop_map(|(op, a, b, cut)| Step { op, a, b, cut }),
        1..120,
    )
}

/// Turns steps into jobs, at most 200 events in all.
pub fn jobs(
    model: &mut Model,
    steps: &[Step],
    with_unknown: bool,
    with_rebuild: bool,
) -> Vec<(Job, Model)> {
    let mut out = Vec::new();
    let mut before = model.clone();
    for (i, s) in steps.iter().enumerate() {
        if model.events >= 200 {
            break;
        }
        model.step(*s, with_unknown, with_rebuild);
        if s.cut || i + 1 == steps.len() {
            let job = model.take_job();
            if job.ops.is_empty() {
                continue;
            }
            if !job.commits {
                // The job rolls back: the model stays as it was before it.
                *model = before.clone();
            }
            before = model.clone();
            out.push((job, model.clone()));
        }
    }
    out
}

/// Runs a job's ops in one `WriteTx`.
pub fn run_ops(
    tx: &mut WriteTx<'_>,
    ops: Vec<Op>,
    raw_unknown: &dyn Fn(&mut WriteTx<'_>, ThreadId, &str) -> Result<(), StoreError>,
) -> Result<(), JobError> {
    for op in ops {
        match op {
            Op::Append(events) => {
                tx.append(events)?;
            }
            Op::Transcript(b) => {
                tx.append_transcript(b)?;
            }
            Op::RawUnknownKind { thread, key } => raw_unknown(tx, thread, &key)?,
            Op::Rebuild => {
                tx.rebuild_projections()?;
            }
        }
    }
    Ok(())
}

/// Compares the store with the model's expected values: axes, current launch, session and
/// path, gate evidence and write basis, input state and launch, and the cursor.
pub fn check_model(r: &ReadTx<'_>, m: &Model) -> Result<(), String> {
    for th in m.threads.values() {
        let row = r
            .thread(th.id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("thread {} missing", th.id))?;
        if row.state != th.state {
            return Err(format!(
                "thread {} state {:?} != model {:?}",
                th.id, row.state, th.state
            ));
        }
        if row.current_launch_id != th.current_launch
            || row.session_id != th.session
            || row.transcript_path != th.path
        {
            return Err(format!(
                "thread {} binding differs: {row:?} vs {th:?}",
                th.id
            ));
        }
        let cursor = r.transcript_cursor(th.id).map_err(|e| e.to_string())?;
        let got = cursor.map(|c| MCursor {
            generation: c.generation,
            path: c.path,
            file_id: c.file_id,
            event_offset: c.event_offset,
            event_continuity: c.event_continuity.0,
            read_offset: c.read_offset,
            read_continuity: c.read_continuity.0,
        });
        if got != th.cursor {
            return Err(format!(
                "thread {} cursor {got:?} != model {:?}",
                th.id, th.cursor
            ));
        }
    }
    for (l, ml) in &m.launches {
        let row = r
            .launch(*l)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("launch {l} missing"))?;
        let got = MLaunch {
            thread: row.thread_id,
            session: row.session_id,
            readiness: row.readiness.map(|x| (x.seq, x.evidence)),
            basis: row.write_basis,
            last_write: row.last_write_seq,
            last_human: row.last_human_write_seq,
            last_turn_start: row.last_turn_start_seq,
            exited: row.exit_reason.is_some(),
            death_confirmed: row.death_confirmed,
        };
        if &got != ml {
            return Err(format!("launch {l}: {got:?} != model {ml:?}"));
        }
    }
    for (i, mi) in &m.inputs {
        let row = r
            .input(*i)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("input {i} missing"))?;
        if row.state != mi.state || row.launch_id != mi.launch || row.thread_id != mi.thread {
            return Err(format!("input {i}: {row:?} != model {mi:?}"));
        }
    }
    let open: BTreeSet<ApprovalId> = m
        .threads
        .keys()
        .map(|t| r.open_approvals(*t).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(|a| a.approval_id)
        .collect();
    let want: BTreeSet<ApprovalId> = m.open_approvals.keys().copied().collect();
    if open != want {
        return Err(format!("open approvals {open:?} != model {want:?}"));
    }
    Ok(())
}

/// The bytes of a blob input, for tests that build blob content by hand.
#[must_use]
pub fn blob_input(data: &[u8]) -> BlobInput {
    BlobInput {
        data: data.to_vec(),
    }
}
