//! The hook ingest harness (`docs/design/hook-ingest.md` §7.2): a test server with the
//! `/hook` mount over synthetic seams (chains as data with a gate to hold one walk, a
//! recording intake probe, a counting adapter), launches built from appended events, a
//! `/hook` request builder, and the no-side-effect snapshot of B-8.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chaldea_core::auth::TokenKind;
use chaldea_core::event::{EventEnvelope, TypedEvent};
use chaldea_core::ids::{Actor, Agent, IdempotencyKey, LaunchId, StreamId, ThreadId, WorkspaceId};
use chaldea_core::payload::{
    AssistantMessage, Content, Encoding, LaunchReserved, LaunchStarted, RawUnknown,
    RawUnknownReason, ReadinessEvidence, ReadinessObserved, SourceKind, ThreadRegistered,
    ThreadRole, UserMessage,
};
use chaldea_core::state::{StateInput, ThreadState, reduce};
use chaldea_daemon::hook::{
    AcceptedHook, BindingRule, HookAdapter, HookConfig, HookEvent, HookIngest, HookSeams,
    IntakeFacts, IntakeProbe, Interpretation, MvpHooks, ProcessProbe,
};
use chaldea_daemon::http::{HttpConfig, Mounts};
use chaldea_daemon::store::{
    CountedClasses, EventAnnex, HookDelivery, LaunchRow, NewEvent, ReadTx, Routing, StoreError,
    ThreadRow, WriteBasis,
};
use chaldea_pty_runtime::ProcessIdentity;
use chaldea_pty_runtime::process::{Ancestry, AncestryEnd, ProcessEntry};
use uuid::Uuid;

use super::event;
use super::http::{Req, TestServer};

/// The pid and start time every test launch records (`launch_started`).
pub const L_PID: u32 = 4242;
pub const L_START: &str = "1000";
/// The emitter pid of the standard chains.
pub const EMITTER_PID: u32 = 9000;

// ---- launches -------------------------------------------------------------------------------

#[must_use]
pub fn register(t: ThreadId, agent: Agent) -> NewEvent {
    event(
        StreamId::Host,
        &format!("register:{t}"),
        TypedEvent::ThreadRegistered(ThreadRegistered {
            thread_id: t,
            agent,
            cwd: "/work/repo".into(),
            workspace_id: WorkspaceId("ws".into()),
            parent_thread_id: None,
            role: ThreadRole::Standard,
            task: None,
        }),
    )
}

#[must_use]
pub fn reserve(t: ThreadId, l: LaunchId, agent: Agent) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("launch:{l}:reserved"),
        TypedEvent::LaunchReserved(LaunchReserved {
            launch_id: l,
            expected_previous: None,
            agent,
            argv_digest: "0".repeat(64),
            manual: false,
            worktree_id: None,
        }),
    )
}

#[must_use]
pub fn start(t: ThreadId, l: LaunchId) -> NewEvent {
    event(
        StreamId::Thread(t),
        &format!("launch:{l}:started"),
        TypedEvent::LaunchStarted(LaunchStarted {
            launch_id: l,
            pid: L_PID,
            start_time: L_START.into(),
            pgid_or_job: "4242".into(),
            cli_version: Some("1.0.0".into()),
        }),
    )
}

/// The `status_changed` events of `input` applied to `state`, keyed `<prefix>:<i>`, and the
/// state after it.
#[must_use]
pub fn status(
    t: ThreadId,
    state: &ThreadState,
    input: &StateInput,
    prefix: &str,
) -> (ThreadState, Vec<NewEvent>) {
    let (next, changes) = reduce(state, input).expect("a valid transition");
    let events = changes
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            event(
                StreamId::Thread(t),
                &format!("{prefix}:{i}"),
                TypedEvent::StatusChanged(c),
            )
        })
        .collect();
    (next, events)
}

/// `launch_reserved` (and `launch_started` when `started`) with their status events, for a
/// thread in `state`.
#[must_use]
pub fn reserve_and_start(
    t: ThreadId,
    l: LaunchId,
    agent: Agent,
    state: &ThreadState,
    relaunch: bool,
    started: bool,
) -> Vec<NewEvent> {
    let mut events = vec![reserve(t, l, agent)];
    let (state, s) = status(
        t,
        state,
        &StateInput::Reserved { relaunch },
        &format!("{l}:reserved"),
    );
    events.extend(s);
    if started {
        events.push(start(t, l));
        events.extend(status(t, &state, &StateInput::Spawned, &format!("{l}:spawned")).1);
    }
    events
}

/// `launch_exited` of `l` with its status events, for a thread in `state`.
#[must_use]
pub fn exit(t: ThreadId, l: LaunchId, state: &ThreadState) -> Vec<NewEvent> {
    let mut events = vec![super::exited(t, l)];
    events.extend(
        status(
            t,
            state,
            &StateInput::Exited {
                reason: chaldea_core::payload::ExitReason::Exit,
            },
            &format!("{l}:exited"),
        )
        .1,
    );
    events
}

// ---- seams --------------------------------------------------------------------------------

fn entry(pid: u32, start: &str, ppid: u32, name: &str) -> ProcessEntry {
    ProcessEntry {
        identity: ProcessIdentity {
            pid,
            start_time: start.into(),
            group: String::new(),
        },
        ppid,
        name: name.into(),
    }
}

/// A chain from `(pid, start, name)` links, `links[0]` first, ending at `end`.
#[must_use]
pub fn chain(links: &[(u32, &str, &str)], end: AncestryEnd) -> Ancestry {
    let mut out = Vec::new();
    for (i, (pid, start, name)) in links.iter().enumerate() {
        let ppid = links.get(i + 1).map_or(1, |n| n.0);
        out.push(entry(*pid, start, ppid, name));
    }
    Ancestry { links: out, end }
}

/// The emitter → `L` chain under `name` for `L`.
#[must_use]
pub fn direct(emitter_pid: u32, l_name: &str) -> Ancestry {
    chain(
        &[
            (emitter_pid, "3000", "chaldea"),
            (L_PID, L_START, l_name),
            (1, "1", "launchd"),
        ],
        AncestryEnd::Root,
    )
}

/// Releases a held walk when dropped or used.
pub type Release = std::sync::mpsc::Sender<()>;

/// A held call's gate: it reports reaching the hold, then waits for its release.
type Gate = (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

/// Synthetic chains by emitter pid; an unknown pid's walk is `Gone(pid)`. One pid's walk
/// can be held until released (at most 20 s).
#[derive(Default)]
pub struct Chains {
    chains: Mutex<HashMap<u32, Ancestry>>,
    holds: Mutex<HashMap<u32, Gate>>,
    /// Walks started, in order of their start.
    pub walked: Mutex<Vec<u32>>,
}

impl Chains {
    pub fn set(&self, pid: u32, a: Ancestry) {
        self.chains.lock().unwrap().insert(pid, a);
    }

    /// Holds the next walk of `pid`: the receiver hears when it starts, and it returns
    /// once the sender is used or dropped.
    pub fn hold(&self, pid: u32) -> (std::sync::mpsc::Receiver<()>, Release) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        self.holds
            .lock()
            .unwrap()
            .insert(pid, (reached_tx, release_rx));
        (reached_rx, release_tx)
    }
}

impl ProcessProbe for Chains {
    fn chain(&self, emitter_pid: u32, max: usize) -> Ancestry {
        self.walked.lock().unwrap().push(emitter_pid);
        let hold = self.holds.lock().unwrap().remove(&emitter_pid);
        if let Some((reached, release)) = hold {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(20));
        }
        let mut a = self
            .chains
            .lock()
            .unwrap()
            .get(&emitter_pid)
            .cloned()
            .unwrap_or(Ancestry {
                links: Vec::new(),
                end: AncestryEnd::Gone(emitter_pid),
            });
        if a.links.len() > max {
            a.links.truncate(max);
            a.end = AncestryEnd::Limit;
        }
        a
    }
}

/// Intake facts `{"n": k, "position": k}`, `k` counting calls from 1; records each call's
/// launch and `k` in call order.
#[derive(Default)]
pub struct Facts {
    n: AtomicU64,
    pub calls: Mutex<Vec<(LaunchId, u64)>>,
}

impl IntakeProbe for Facts {
    fn facts(&self, launch: LaunchId) -> Option<IntakeFacts> {
        let k = self.n.fetch_add(1, Ordering::SeqCst) + 1;
        self.calls.lock().unwrap().push((launch, k));
        Some(IntakeFacts(serde_json::json!({ "n": k, "position": k })))
    }
}

/// What one `interpret` call saw.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub emit_id: Uuid,
    pub event_name: String,
    pub receipt_ordinal: u64,
    pub attempt: Option<u8>,
    pub intake_reliable: bool,
    pub binding_ordinal: u64,
    pub promoted: bool,
    pub intake: Option<IntakeFacts>,
    /// The launch row's and the thread row's session as `interpret` read them.
    pub launch_session: Option<String>,
    pub thread_session: Option<String>,
}

/// How [`TestAdapter`] answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// `UserPromptSubmit` → `user_message`, `Stop` → `assistant_message`, others raw.
    #[default]
    Plain,
    /// As `Plain`, and `UserPromptSubmit` also returns `readiness_observed` with a write
    /// basis whose position is the intake facts' `position`.
    Readiness,
    /// Every selected hook returns two events with one name (an adapter bug).
    RepeatName,
    /// Every selected hook returns an event whose name starts with `bind`.
    ReservedName,
    /// `interpret` panics.
    Panic,
}

/// A counting adapter with §2.5's rules and `MvpHooks`' expected session.
#[derive(Default)]
pub struct TestAdapter {
    pub mode: Mutex<Mode>,
    pub calls: Mutex<Vec<Call>>,
    /// Taken by the next `interpret`, which holds there (on the writer thread) until
    /// released, at most 20 s.
    hold: Mutex<Option<Gate>>,
}

impl TestAdapter {
    /// Holds the next `interpret` inside its job: the receiver hears when it is reached,
    /// and the job goes on once the sender is used or dropped.
    pub fn hold(&self) -> (std::sync::mpsc::Receiver<()>, Release) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *self.hold.lock().unwrap() = Some((reached_tx, release_rx));
        (reached_rx, release_tx)
    }

    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    pub fn set_mode(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }
}

fn text(s: &str) -> Content {
    Content::Inline {
        inline: s.into(),
        encoding: Encoding::Utf8,
    }
}

fn hook_event(event: TypedEvent, name: &'static str) -> HookEvent {
    HookEvent {
        event,
        name,
        annex: None,
        blobs: Vec::new(),
    }
}

impl HookAdapter for TestAdapter {
    fn rule(&self, agent: Agent) -> BindingRule {
        MvpHooks.rule(agent)
    }

    fn expected_session(
        &self,
        tx: &ReadTx<'_>,
        launch: &LaunchRow,
    ) -> Result<Option<String>, StoreError> {
        MvpHooks.expected_session(tx, launch)
    }

    fn interpret(
        &self,
        tx: &ReadTx<'_>,
        hook: &AcceptedHook<'_>,
    ) -> Result<Interpretation, StoreError> {
        let hold = self.hold.lock().unwrap().take();
        if let Some((reached, release)) = hold {
            let _ = reached.send(());
            let _ = release.recv_timeout(Duration::from_secs(20));
        }
        let mode = *self.mode.lock().unwrap();
        self.calls.lock().unwrap().push(Call {
            emit_id: hook.emit_id,
            event_name: hook.event_name.to_owned(),
            receipt_ordinal: hook.receipt_ordinal,
            attempt: hook.attempt,
            intake_reliable: hook.intake_reliable,
            binding_ordinal: hook.binding_ordinal,
            promoted: hook.promoted,
            intake: hook.intake.cloned(),
            launch_session: tx.launch(hook.launch.launch_id)?.and_then(|l| l.session_id),
            thread_session: tx.thread(hook.thread)?.and_then(|t| t.session_id),
        });
        let note = || {
            TypedEvent::AssistantMessage(AssistantMessage {
                text: text("note"),
                turn_ref: None,
            })
        };
        match mode {
            Mode::Panic => panic!("the test adapter panics"),
            Mode::RepeatName => {
                return Ok(Interpretation::Events(vec![
                    hook_event(note(), "twice"),
                    hook_event(note(), "twice"),
                ]));
            }
            Mode::ReservedName => {
                return Ok(Interpretation::Events(vec![hook_event(note(), "bind_x")]));
            }
            Mode::Plain | Mode::Readiness => {}
        }
        Ok(match hook.event_name {
            "UserPromptSubmit" => {
                let mut events = vec![hook_event(
                    TypedEvent::UserMessage(UserMessage {
                        prompt: text("prompt"),
                        input_id: None,
                    }),
                    "prompt",
                )];
                if mode == Mode::Readiness {
                    let position = hook
                        .intake
                        .and_then(|f| f.0.get("position"))
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    events.push(HookEvent {
                        event: TypedEvent::ReadinessObserved(ReadinessObserved {
                            launch_id: hook.launch.launch_id,
                            evidence: ReadinessEvidence::StopSettled,
                            by: Actor::Daemon,
                        }),
                        name: "readiness",
                        annex: Some(EventAnnex::Readiness(WriteBasis {
                            launch_id: hook.launch.launch_id,
                            position,
                            counted: CountedClasses::ALL,
                        })),
                        blobs: Vec::new(),
                    });
                }
                Interpretation::Events(events)
            }
            "Stop" => Interpretation::Events(vec![hook_event(
                TypedEvent::AssistantMessage(AssistantMessage {
                    text: text("stop"),
                    turn_ref: None,
                }),
                "stop",
            )]),
            _ => Interpretation::Raw(RawUnknownReason::UnknownEvent),
        })
    }
}

// ---- the fixture --------------------------------------------------------------------------

/// A server with the `/hook` mount over the test seams.
pub struct HookServer {
    pub srv: TestServer,
    pub ingest: Arc<HookIngest>,
    pub chains: Arc<Chains>,
    pub facts: Arc<Facts>,
    pub adapter: Arc<TestAdapter>,
}

/// One registered, started launch and its hook token.
#[derive(Debug, Clone)]
pub struct Launch {
    pub thread: ThreadId,
    pub launch: LaunchId,
    pub agent: Agent,
    pub token: String,
}

impl HookServer {
    pub async fn start() -> HookServer {
        Self::start_with(HookConfig::default(), |_| {}).await
    }

    pub async fn start_with(cfg: HookConfig, tune: impl FnOnce(&mut HttpConfig)) -> HookServer {
        Self::start_on(super::TestStore::new(), cfg, tune).await
    }

    pub async fn start_on(
        store: super::TestStore,
        cfg: HookConfig,
        tune: impl FnOnce(&mut HttpConfig),
    ) -> HookServer {
        let chains = Arc::new(Chains::default());
        let facts = Arc::new(Facts::default());
        let adapter = Arc::new(TestAdapter::default());
        let ingest = HookIngest::new(
            cfg,
            HookSeams {
                adapter: adapter.clone(),
                intake: facts.clone(),
                procs: chains.clone(),
            },
        );
        let srv = TestServer::start_on(store, Mounts::mvp().with(ingest.mount()), tune).await;
        HookServer {
            srv,
            ingest,
            chains,
            facts,
            adapter,
        }
    }

    /// Registers thread `t` with launch `l` of `agent`, reserved (and started when
    /// `started`), and mints its hook token. The standard chain for emitter pid
    /// [`EMITTER_PID`] reaches `L` under the agent's name.
    pub async fn launch_with(
        &self,
        t: ThreadId,
        l: LaunchId,
        agent: Agent,
        started: bool,
    ) -> Launch {
        self.srv.append(vec![register(t, agent)]).await;
        let state = self.thread_row(t).state;
        self.srv
            .append(reserve_and_start(t, l, agent, &state, false, started))
            .await;
        let token = self.srv.token(TokenKind::Hook, Some(t), Some(l)).await;
        self.chains
            .set(EMITTER_PID, direct(EMITTER_PID, agent_name(agent)));
        Launch {
            thread: t,
            launch: l,
            agent,
            token,
        }
    }

    /// A started Codex launch on thread/launch `n`.
    pub async fn codex(&self, n: u128) -> Launch {
        self.launch_with(super::thread(n), super::launch(n), Agent::Codex, true)
            .await
    }

    /// A started Claude launch on thread/launch `n`.
    pub async fn claude(&self, n: u128) -> Launch {
        self.launch_with(super::thread(n), super::launch(n), Agent::Claude, true)
            .await
    }

    /// Runs one job on the store writer and awaits it.
    pub async fn run<R: Send + 'static>(
        &self,
        f: impl FnOnce(
            &mut chaldea_daemon::store::WriteTx<'_>,
        ) -> Result<R, chaldea_daemon::store::JobError>
        + Send
        + 'static,
    ) -> R {
        let t = self.srv.store.store().submit(super::soon(), f).unwrap();
        super::http::settled(t).await.unwrap()
    }

    pub async fn post(&self, h: &Hook) -> super::http::Reply {
        self.srv.send(&h.req(&self.srv)).await
    }

    /// Posts on its own task.
    pub fn spawn_post(&self, h: &Hook) -> tokio::task::JoinHandle<super::http::Reply> {
        super::http::spawn_send(&self.srv, &h.req(&self.srv))
    }

    /// The thread stream's events.
    #[must_use]
    pub fn stream(&self, t: ThreadId) -> Vec<EventEnvelope> {
        self.srv
            .store
            .read(move |tx| tx.events_after(StreamId::Thread(t), 0, 10_000))
            .events
    }

    /// `(kind, key)` of the thread stream's events after `launch_started`.
    #[must_use]
    pub fn hook_events(&self, t: ThreadId) -> Vec<(String, String)> {
        self.stream(t)
            .into_iter()
            .filter(|e| e.source.kind == chaldea_core::ids::Open::Known(SourceKind::Hook))
            .map(|e| (e.kind.0.clone(), e.idempotency_key.0))
            .collect()
    }

    #[must_use]
    pub fn delivery(&self, l: LaunchId, emit: Uuid) -> Option<HookDelivery> {
        self.srv.store.read(move |tx| tx.hook_delivery(l, emit))
    }

    #[must_use]
    pub fn launch_row(&self, l: LaunchId) -> LaunchRow {
        self.srv.store.read(move |tx| tx.launch(l)).unwrap()
    }

    #[must_use]
    pub fn thread_row(&self, t: ThreadId) -> ThreadRow {
        self.srv.store.read(move |tx| tx.thread(t)).unwrap()
    }

    /// The event a delivery's routing points at.
    #[must_use]
    pub fn routed_event(&self, d: &HookDelivery) -> EventEnvelope {
        let (stream, seq) = match &d.routing {
            Routing::Appended { stream_id, seq } | Routing::RawUnknown { stream_id, seq } => {
                (*stream_id, *seq)
            }
            Routing::Buffered => panic!("buffered: {d:?}"),
        };
        self.srv
            .store
            .read(move |tx| tx.events_after(stream, seq - 1, 1))
            .events
            .remove(0)
    }

    /// Asserts `emit`'s delivery is a `raw_unknown` with `reason`, in the token's thread
    /// stream with the token's launch and key `hook:<emit>` (PR9-CL-28), and returns it.
    pub fn assert_raw(&self, l: &Launch, emit: Uuid, reason: RawUnknownReason) -> RawUnknown {
        let d = self.delivery(l.launch, emit).expect("a delivery");
        assert!(
            matches!(d.routing, Routing::RawUnknown { .. }),
            "not raw_unknown: {d:?}"
        );
        let e = self.routed_event(&d);
        assert_eq!(e.stream_id, StreamId::Thread(l.thread));
        assert_eq!(e.thread_id, Some(l.thread));
        assert_eq!(e.launch_id, Some(l.launch));
        assert_eq!(e.idempotency_key, IdempotencyKey(format!("hook:{emit}")));
        let TypedEvent::RawUnknown(r) = TypedEvent::decode(&e).unwrap() else {
            panic!("not a raw_unknown event: {e:?}");
        };
        assert_eq!(r.reason, reason, "{r:?}");
        assert_eq!(r.source, SourceKind::Hook);
        r
    }

    /// The B-8 no-side-effect snapshot: the launch's deliveries, the host and thread
    /// heads, and every `proj_*` row.
    #[must_use]
    pub fn snapshot(&self, l: &Launch) -> Snapshot {
        let (t, lid) = (l.thread, l.launch);
        let heads = self.srv.store.read(move |tx| {
            Ok((
                tx.head(StreamId::Host, 0)?.head_seq,
                tx.head(StreamId::Thread(t), 0)?.head_seq,
            ))
        });
        let tables = super::http::dump(&self.srv.store.path())
            .into_iter()
            .filter(|(n, _)| n.starts_with("proj_"))
            .collect();
        let c = rusqlite::Connection::open_with_flags(
            self.srv.store.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let deliveries: i64 = c
            .query_row(
                "SELECT count(*) FROM hook_deliveries WHERE launch_id = ?1",
                [lid.to_string()],
                |r| r.get(0),
            )
            .unwrap();
        Snapshot {
            deliveries,
            heads,
            tables,
        }
    }

    pub async fn stop(self) {
        self.srv.stop().await;
    }
}

/// What a rejection must leave unchanged.
#[derive(Debug, PartialEq)]
pub struct Snapshot {
    pub deliveries: i64,
    pub heads: (u64, u64),
    pub tables: Vec<(String, Vec<Vec<rusqlite::types::Value>>)>,
}

#[must_use]
pub fn agent_name(a: Agent) -> &'static str {
    match a {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}

/// One `/hook` request.
#[derive(Debug, Clone)]
pub struct Hook {
    pub token: String,
    pub emit: Uuid,
    pub attempt: Option<String>,
    pub pid: Option<u32>,
    pub thread: Option<String>,
    pub agent: Option<String>,
    pub truncated: bool,
    pub body: Vec<u8>,
}

impl Hook {
    /// A hook of `l` with a fresh emit id, attempt 1, the standard emitter pid, and the
    /// thread header.
    #[must_use]
    pub fn new(l: &Launch, body: &[u8]) -> Hook {
        Hook {
            token: l.token.clone(),
            emit: Uuid::new_v4(),
            attempt: Some("1".into()),
            pid: Some(EMITTER_PID),
            thread: Some(l.thread.to_string()),
            agent: Some(agent_name(l.agent).into()),
            truncated: false,
            body: body.to_vec(),
        }
    }

    /// `{"hook_event_name": event, "session_id": session}` (plus `transcript_path: null`
    /// for a `SessionStart`).
    #[must_use]
    pub fn of(l: &Launch, event: &str, session: &str) -> Hook {
        Self::new(l, &payload(event, session))
    }

    #[must_use]
    pub fn emit(mut self, e: Uuid) -> Hook {
        self.emit = e;
        self
    }

    #[must_use]
    pub fn attempt(mut self, a: Option<&str>) -> Hook {
        self.attempt = a.map(str::to_owned);
        self
    }

    #[must_use]
    pub fn pid(mut self, p: Option<u32>) -> Hook {
        self.pid = p;
        self
    }

    #[must_use]
    pub fn thread(mut self, t: Option<&str>) -> Hook {
        self.thread = t.map(str::to_owned);
        self
    }

    #[must_use]
    pub fn truncated(mut self) -> Hook {
        self.truncated = true;
        self
    }

    #[must_use]
    pub fn req(&self, srv: &TestServer) -> Req {
        let mut r = srv
            .req("POST", "/hook")
            .bearer(&self.token)
            .header("Content-Type", "application/json")
            .header("X-Chaldea-Emit-Id", &self.emit.to_string())
            .header("X-Chaldea-Emitter-Version", "0.1.0");
        if let Some(a) = &self.attempt {
            r = r.header("X-Chaldea-Attempt", a);
        }
        if let Some(p) = self.pid {
            r = r.header("X-Chaldea-Emitter-Pid", &p.to_string());
        }
        if let Some(t) = &self.thread {
            r = r.header("X-Chaldea-Thread-Id", t);
        }
        if let Some(a) = &self.agent {
            r = r.header("X-Chaldea-Agent", a);
        }
        if self.truncated {
            r = r.header("X-Chaldea-Body-Truncated", "1");
        }
        r.body(&self.body)
    }
}

/// A hook payload; a `SessionStart` carries `transcript_path: null`.
#[must_use]
pub fn payload(event: &str, session: &str) -> Vec<u8> {
    let mut v = serde_json::json!({ "hook_event_name": event, "session_id": session });
    if event == "SessionStart" {
        v["transcript_path"] = serde_json::Value::Null;
    }
    serde_json::to_vec(&v).unwrap()
}
