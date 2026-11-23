//! Hook ingest end to end (`docs/design/hook-ingest.md` §7.4): the daemon's store and HTTP
//! server built from public APIs with `Mounts::mvp()` (the `/hook` mount over `MvpHooks`
//! and the OS walk), FakeCli `hooks` as the launch's process, and the real emitter. Each
//! script first waits for a gate file, created once `launch_started` records FakeCli's own
//! pid and start time, so every hook reaches a started launch.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chaldea_core::auth::TokenKind;
use chaldea_core::event::{EventEnvelope, Source, TypedEvent};
use chaldea_core::ids::{Agent, IdempotencyKey, LaunchId, Open, StreamId, ThreadId, WorkspaceId};
use chaldea_core::payload::{
    Content, LaunchReserved, LaunchStarted, SourceKind, ThreadRegistered, ThreadRole,
};
use chaldea_core::state::{StateInput, reduce};
use chaldea_daemon::http::{
    CommandRegistry, HttpConfig, HttpServer, LifecycleSink, Mounts, ServeHandle, StoreFatal,
    TokenService,
};
use chaldea_daemon::lifecycle::spawn_locked;
use chaldea_daemon::store::{
    NewEvent, RandomIds, ReadTx, Store, StoreConfig, StoreError, SystemClock,
};
use chaldea_pty_runtime::process::{self, Lookup};
use child::{collect, kill_and_reap, wait_until};
use serde_json::{Value, json};

mod child;

const EMITTER: &str = env!("CARGO_BIN_EXE_chaldea");
/// Every bounded wait of these tests.
const WAIT: Duration = Duration::from_secs(30);
const POLL: Duration = Duration::from_millis(10);

struct NoSink;

impl LifecycleSink for NoSink {
    fn store_fatal(&self, f: StoreFatal) {
        panic!("store fatal: {f:?}");
    }
}

/// The daemon's store and HTTP server on `127.0.0.1:0`, over a temp directory.
struct Daemon {
    rt: tokio::runtime::Runtime,
    store: Store,
    server: HttpServer,
    handle: Option<ServeHandle>,
    port: u16,
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Daemon {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(StoreConfig {
            path: dir.path().join("chaldea.db"),
            readers: 2,
            writer_queue: 64,
            busy_timeout: Duration::from_secs(5),
            clock: Arc::new(SystemClock),
            ids: Arc::new(RandomIds),
        })
        .unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = HttpServer::new(
            HttpConfig::new(port),
            store.clone(),
            CommandRegistry::mvp(),
            Arc::new(NoSink),
            Mounts::mvp(),
        );
        let handle = {
            let _rt = rt.enter();
            server.serve(listener)
        };
        Daemon {
            rt,
            store,
            server,
            handle: Some(handle),
            port,
            _dir: dir,
        }
    }

    fn append(&self, events: Vec<NewEvent>) {
        let t = self
            .store
            .submit(Instant::now() + WAIT, move |tx| Ok(tx.append(events)?))
            .unwrap();
        t.blocking_wait().unwrap();
    }

    fn read<R>(&self, f: impl FnOnce(&ReadTx<'_>) -> Result<R, StoreError>) -> R {
        self.store.read(Instant::now() + WAIT, f).unwrap()
    }

    fn hook_token(&self, t: ThreadId, l: LaunchId) -> String {
        let m = self.server.tokens().mint();
        let row = TokenService::new_token(&m, TokenKind::Hook, Some(t), Some(l));
        self.store
            .submit(Instant::now() + WAIT, move |tx| Ok(tx.token_insert(row)?))
            .unwrap()
            .blocking_wait()
            .unwrap();
        m.secret.expose().to_owned()
    }

    /// The thread stream's events from hooks: `(kind, key, raw body if a raw_unknown)`.
    fn hook_events(&self, t: ThreadId) -> Vec<(String, String, Option<Value>)> {
        self.read(move |r| r.events_after(StreamId::Thread(t), 0, 10_000))
            .events
            .into_iter()
            .filter(|e| e.source.kind == Open::Known(SourceKind::Hook))
            .map(|e| (e.kind.0.clone(), e.idempotency_key.0.clone(), raw_body(&e)))
            .collect()
    }

    fn session(&self, l: LaunchId) -> Option<String> {
        self.read(move |r| r.launch(l)).unwrap().session_id
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            self.rt
                .block_on(h.shutdown(Instant::now() + Duration::from_secs(10)));
        }
        let r = self
            .store
            .shutdown(Instant::now() + Duration::from_secs(20));
        assert!(r.writer_exited || std::thread::panicking(), "{r:?}");
    }
}

/// A `raw_unknown`'s body as JSON, with its reason as `"reason"`.
fn raw_body(e: &EventEnvelope) -> Option<Value> {
    let TypedEvent::RawUnknown(r) = TypedEvent::decode(e).ok()? else {
        return None;
    };
    let Content::Inline { inline, .. } = &r.raw else {
        return None;
    };
    let mut v: Value = serde_json::from_str(inline).ok()?;
    v["reason"] = serde_json::to_value(r.reason).unwrap();
    Some(v)
}

fn event(
    stream: StreamId,
    key: &str,
    ev: TypedEvent,
    t: ThreadId,
    l: Option<LaunchId>,
) -> NewEvent {
    NewEvent {
        stream_id: stream,
        idempotency_key: IdempotencyKey(key.to_owned()),
        event: ev,
        thread_id: Some(t),
        launch_id: l,
        command_id: None,
        source: Source {
            kind: Open::Known(SourceKind::Daemon),
            agent: None,
            cli_version: None,
            transcript: None,
        },
        occurred_at: None,
        annex: None,
        blobs: Vec::new(),
    }
}

/// The `status_changed` events of `input` for thread `t`, from its stored state.
fn status(d: &Daemon, t: ThreadId, input: &StateInput, key: &str) -> Vec<NewEvent> {
    let state = d.read(move |r| r.thread(t)).unwrap().state;
    let (_, changes) = reduce(&state, input).expect("a valid transition");
    changes
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            event(
                StreamId::Thread(t),
                &format!("{key}:{i}"),
                TypedEvent::StatusChanged(c),
                t,
                None,
            )
        })
        .collect()
}

/// One launch of `agent`: registered and reserved, its hook token minted.
struct Launch {
    thread: ThreadId,
    launch: LaunchId,
    agent: Agent,
    token: String,
}

fn reserve(d: &Daemon, agent: Agent) -> Launch {
    let (t, l) = (
        ThreadId(uuid::Uuid::new_v4()),
        LaunchId(uuid::Uuid::new_v4()),
    );
    d.append(vec![event(
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
        t,
        None,
    )]);
    let mut events = vec![event(
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
        t,
        Some(l),
    )];
    d.append(std::mem::take(&mut events));
    d.append(status(
        d,
        t,
        &StateInput::Reserved { relaunch: false },
        "reserved",
    ));
    let token = d.hook_token(t, l);
    Launch {
        thread: t,
        launch: l,
        agent,
        token,
    }
}

/// Records FakeCli (`pid`) as the launch's process, with its status.
fn started(d: &Daemon, l: &Launch, pid: u32) {
    let Lookup::Live(e) = process::lookup(pid) else {
        panic!("FakeCli {pid} is not live");
    };
    d.append(vec![event(
        StreamId::Thread(l.thread),
        &format!("launch:{}:started", l.launch),
        TypedEvent::LaunchStarted(LaunchStarted {
            launch_id: l.launch,
            pid,
            start_time: e.identity.start_time,
            pgid_or_job: pid.to_string(),
            cli_version: None,
        }),
        l.thread,
        Some(l.launch),
    )]);
    d.append(status(d, l.thread, &StateInput::Spawned, "spawned"));
}

/// FakeCli, built by `cargo test --workspace` (testkit's binary) next to the test binaries'
/// `deps/` directory.
fn fake_cli() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let p = dir.join(format!("chaldea-fake-cli{}", std::env::consts::EXE_SUFFIX));
    assert!(
        p.is_file(),
        "{} is missing: build with cargo test --workspace",
        p.display()
    );
    p
}

/// A FakeCli `hooks` run of `script` behind the gate `started` (its first step), killed
/// and reaped on drop.
struct Fake {
    child: Child,
    out: std::sync::mpsc::Receiver<Vec<u8>>,
    err: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl Drop for Fake {
    fn drop(&mut self) {
        kill_and_reap(&mut self.child, WAIT);
    }
}

fn spawn_fake(d: &Daemon, l: &Launch, started: &Path, steps: &[Value]) -> Fake {
    let mut script = vec![json!({ "wait_file": started.to_str().unwrap() })];
    script.extend_from_slice(steps);
    let agent = match l.agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    };
    let mut cmd = Command::new(fake_cli());
    cmd.args(["--mode", "hooks", "--agent", agent, "--emitter", EMITTER])
        .args(["--script", &Value::Array(script).to_string()])
        .args(["--max-life-secs", "60"])
        .env(
            "CHALDEA_HOOK_URL",
            format!("http://127.0.0.1:{}/hook", d.port),
        )
        .env("CHALDEA_HOOK_TOKEN", &l.token)
        .env("CHALDEA_THREAD_ID", l.thread.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = spawn_locked(&mut cmd).unwrap();
    let out = collect(child.stdout.take());
    let err = collect(child.stderr.take());
    Fake { child, out, err }
}

/// Waits for FakeCli's exit 0 with empty outputs.
fn finished(mut f: Fake) {
    let s = wait_until(&mut f.child, Instant::now() + WAIT, WAIT)
        .unwrap_or_else(|| panic!("FakeCli did not exit within {WAIT:?}"));
    let out = f.out.recv_timeout(WAIT).unwrap();
    let err = f.err.recv_timeout(WAIT).unwrap();
    assert_eq!(
        (s.code(), out.as_slice(), err.as_slice()),
        (Some(0), &b""[..], &b""[..]),
        "{}",
        String::from_utf8_lossy(&err)
    );
}

/// Polls `cond` until it holds, at most [`WAIT`].
fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < end, "{what} within {WAIT:?}");
        std::thread::sleep(POLL);
    }
}

fn session_start(session: &str) -> Value {
    json!({ "hook_event_name": "SessionStart", "session_id": session, "transcript_path": null })
}

/// The `raw_unknown` bodies of `events` with `event` and `session`.
fn raws<'a>(
    events: &'a [(String, String, Option<Value>)],
    event: &str,
    session: &str,
) -> Vec<&'a Value> {
    events
        .iter()
        .filter_map(|e| e.2.as_ref())
        .filter(|v| v["hook_event_name"] == event && v["session_id"] == session)
        .collect()
}

/// A Codex `UserPromptSubmit` before the binding is buffered; the `SessionStart` then binds
/// through the real walk (emitter → FakeCli = `L`) and promotes it after `session_bound`.
#[test]
fn codex_order_binds_and_promotes() {
    let d = Daemon::start();
    let l = reserve(&d, Agent::Codex);
    let dir = tempfile::tempdir().unwrap();
    let (gate0, gate1) = (dir.path().join("started"), dir.path().join("go"));
    let prompt =
        json!({ "hook_event_name": "UserPromptSubmit", "session_id": "own", "prompt": "p" });
    let f = spawn_fake(
        &d,
        &l,
        &gate0,
        &[
            json!({ "emit": prompt }),
            json!({ "wait_file": gate1.to_str().unwrap() }),
            json!({ "emit": session_start("own") }),
        ],
    );
    started(&d, &l, f.child.id());
    std::fs::write(&gate0, b"").unwrap();
    let lid = l.launch;
    eventually("a buffered delivery", || {
        d.read(move |r| r.buffered_summary(lid)).count == 1
    });
    assert_eq!(d.session(l.launch), None);
    std::fs::write(&gate1, b"").unwrap();
    finished(f);
    assert_eq!(d.session(l.launch).as_deref(), Some("own"));
    assert_eq!(d.read(move |r| r.buffered_summary(lid)).count, 0);
    let events = d.hook_events(l.thread);
    let bound = events
        .iter()
        .position(|e| e.0 == "session_bound")
        .unwrap_or_else(|| panic!("{events:?}"));
    let promoted = events
        .iter()
        .position(|e| {
            e.2.as_ref()
                .is_some_and(|v| v["hook_event_name"] == "UserPromptSubmit")
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    assert!(bound < promoted, "{events:?}");
    assert_eq!(events.iter().filter(|e| e.0 == "session_bound").count(), 1);
}

/// A nested FakeCli under the launch's process, running the unmodified emitter with the
/// inherited environment, does not bind (its own link lies between the emitter and `L`):
/// both nested `SessionStart`s are `foreign_session`, and the launch binds once, to its own.
#[test]
fn nested_fake_cli_does_not_bind() {
    let d = Daemon::start();
    let l = reserve(&d, Agent::Codex);
    let dir = tempfile::tempdir().unwrap();
    let (gate0, gate1) = (dir.path().join("started"), dir.path().join("go"));
    let f = spawn_fake(
        &d,
        &l,
        &gate0,
        &[
            json!({ "nested": [{ "emit": session_start("other") }] }),
            json!({ "wait_file": gate1.to_str().unwrap() }),
            json!({ "emit": session_start("own") }),
            json!({ "nested": [{ "emit": session_start("other2") }] }),
        ],
    );
    started(&d, &l, f.child.id());
    std::fs::write(&gate0, b"").unwrap();
    let t = l.thread;
    eventually("the nested hook", || {
        !raws(&d.hook_events(t), "SessionStart", "other").is_empty()
    });
    let events = d.hook_events(t);
    let nested = raws(&events, "SessionStart", "other");
    assert_eq!(nested[0]["reason"], "foreign_session", "{events:?}");
    assert_eq!(d.session(l.launch), None);
    std::fs::write(&gate1, b"").unwrap();
    finished(f);
    assert_eq!(d.session(l.launch).as_deref(), Some("own"));
    let events = d.hook_events(t);
    for s in ["other", "other2"] {
        let r = raws(&events, "SessionStart", s);
        assert_eq!(r.len(), 1, "{s}: {events:?}");
        assert_eq!(r[0]["reason"], "foreign_session", "{s}: {events:?}");
    }
    assert_eq!(events.iter().filter(|e| e.0 == "session_bound").count(), 1);
}

/// A Claude launch binds by its expected session (the launch id of a fresh launch): a
/// `SessionStart` of another session is `foreign_session`, then its own binds.
#[test]
fn claude_session_id_binds() {
    let d = Daemon::start();
    let l = reserve(&d, Agent::Claude);
    let dir = tempfile::tempdir().unwrap();
    let gate0 = dir.path().join("started");
    let own = l.launch.to_string();
    let f = spawn_fake(
        &d,
        &l,
        &gate0,
        &[
            json!({ "emit": session_start("other") }),
            json!({ "emit": session_start(&own) }),
        ],
    );
    started(&d, &l, f.child.id());
    std::fs::write(&gate0, b"").unwrap();
    finished(f);
    assert_eq!(d.session(l.launch).as_deref(), Some(own.as_str()));
    let events = d.hook_events(l.thread);
    let other = raws(&events, "SessionStart", "other");
    assert_eq!(other.len(), 1, "{events:?}");
    assert_eq!(other[0]["reason"], "foreign_session");
    assert_eq!(events.iter().filter(|e| e.0 == "session_bound").count(), 1);
}
