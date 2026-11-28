//! Supervisor unit tests (design §5.2, §6): `FakePty`, the in-process store and server, and
//! `cfg(test)` pause points.

mod adapters;
mod close;
mod commands;
mod evidence;
mod exit;
mod launch;
mod live;
mod ops;
mod prober;
mod race;
mod reserve;
pub(crate) mod restart;
mod screen;
mod shutdown;
mod stop;
mod tick;
mod transcript;
pub(crate) mod version;
mod workspace;

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chaldea_core::adapter::HostOs;
use chaldea_core::ids::{CommandId, LaunchId, StreamId, ThreadId};
use chaldea_pty_runtime::fake::FakePty;
use chaldea_pty_runtime::{CloseBounds, PtySize};
use futures_util::future::BoxFuture;
use tempfile::TempDir;
use uuid::Uuid;

pub(crate) use crate::store::tests::support;
use crate::store::{LaunchRow, ThreadRow, TokenKind};
use support::TestStore;
use support::http::{RecordingSink, Reply, SkewClock, TestServer};

use super::components::{mounts_with, new_supervisor, registry, seams};
use super::{Pause, Pauses, Supervisor, SupervisorConfig};

/// The program the fake runtime "runs".
pub(crate) const PROGRAM: &str = if cfg!(windows) {
    r"C:\work\agent.exe"
} else {
    "/work/agent"
};

/// A pause point armed once: the test sees it reached, then releases it.
pub(crate) struct Armed {
    pub reached: mpsc::Receiver<()>,
    pub release: mpsc::Sender<()>,
    /// Told when the panic of [`Pauser::arm_panic`] has ended its unwind (its payload,
    /// an [`Unwound`], dropped where the panic was caught).
    pub unwound: mpsc::Receiver<()>,
}

impl Armed {
    /// Waits until the point is reached (20 s at most).
    pub(crate) fn wait(&self) {
        self.reached
            .recv_timeout(Duration::from_secs(20))
            .expect("the pause point is reached");
    }

    pub(crate) fn go(self) {
        let _ = self.release.send(());
    }

    /// Releases the point and waits (20 s at most) until its panic has unwound.
    pub(crate) fn go_and_unwind(self) {
        let _ = self.release.send(());
        self.unwound
            .recv_timeout(Duration::from_secs(20))
            .expect("the pause point's panic unwound");
    }
}

/// A panic payload that says, when dropped where the panic was caught, that the unwind ended.
pub(crate) struct Unwound(pub mpsc::Sender<()>);

impl Drop for Unwound {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

struct Hold {
    reached: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    /// Panics once released instead of returning (a fault on that thread).
    panic: bool,
    unwound: mpsc::Sender<()>,
}

impl Hold {
    fn block(self) {
        let _ = self.reached.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(60));
        if self.panic {
            chaldea_pty_runtime::fake::inject_panic(Unwound(self.unwound));
        }
    }
}

/// The supervisor's pause points, each armed at most once at a time; every point reached
/// is logged, armed or not.
#[derive(Default)]
pub(crate) struct Pauser {
    holds: Mutex<HashMap<Pause, Hold>>,
    passed: Mutex<Vec<Pause>>,
}

impl Pauser {
    pub(crate) fn arm(&self, p: Pause) -> Armed {
        self.arm_with(p, false)
    }

    /// Like [`Pauser::arm`], but the thread panics at `p` once released.
    pub(crate) fn arm_panic(&self, p: Pause) -> Armed {
        self.arm_with(p, true)
    }

    fn arm_with(&self, p: Pause, panic: bool) -> Armed {
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let (unwound_tx, unwound) = mpsc::channel();
        self.holds.lock().unwrap().insert(
            p,
            Hold {
                reached: reached_tx,
                release: release_rx,
                panic,
                unwound: unwound_tx,
            },
        );
        Armed {
            reached,
            release,
            unwound,
        }
    }

    /// Whether `p` was reached.
    pub(crate) fn passed(&self, p: Pause) -> bool {
        self.passed.lock().unwrap().contains(&p)
    }

    fn take(&self, p: Pause) -> Option<Hold> {
        self.passed.lock().unwrap().push(p);
        self.holds.lock().unwrap().remove(&p)
    }
}

impl Pauses for Pauser {
    fn at(&self, p: Pause) -> BoxFuture<'static, ()> {
        let hold = self.take(p);
        Box::pin(async move {
            if let Some(h) = hold
                && let Err(e) = tokio::task::spawn_blocking(move || h.block()).await
                && e.is_panic()
            {
                std::panic::resume_unwind(e.into_panic());
            }
        })
    }

    fn blocking(&self, p: Pause) {
        if let Some(h) = self.take(p) {
            h.block();
        }
    }
}

/// A supervisor with `FakePty`, served by an in-process server over a temp store.
pub(crate) struct Rig {
    pub srv: TestServer,
    pub sup: Arc<Supervisor>,
    pub pty: Arc<FakePty>,
    pub pauses: Arc<Pauser>,
    pub clock: Arc<SkewClock>,
    pub sink: Arc<RecordingSink>,
    pub owner: String,
    pub cwd: TempDir,
}

/// The test configuration: `FakePty`, the given clock and pauses, no tick task, and no send
/// worker or driver: a worker's pass takes the live map, which an intake or `observe` meeting
/// it answers with no observation. The send tests' rig turns them on.
pub(crate) fn config(
    pty: &Arc<FakePty>,
    clock: &Arc<SkewClock>,
    pauses: &Arc<Pauser>,
) -> SupervisorConfig {
    SupervisorConfig {
        runtime: pty.clone(),
        clock: clock.clone(),
        program: Some(Arc::new(|_| Some(vec![OsString::from(PROGRAM)]))),
        env: Some(vec![(OsString::from("HOME_LIKE"), OsString::from("/work"))]),
        git: vec![OsString::from("git")],
        git_env: None,
        kill_budget: Duration::from_secs(5),
        close_bounds: CloseBounds::default(),
        pty_size: PtySize::new(80, 24).unwrap(),
        pauses: pauses.clone(),
        hook: crate::hook::HookConfig::default(),
        exit_steps: Vec::new(),
        tick_steps: Vec::new(),
        tick_task: false,
        decoders: crate::supervisor::send::Decoders::all(Arc::new(
            crate::supervisor::send::fake_decoder::FakeDecoder,
        )),
        readiness_mode: None,
        send_workers: false,
        // No OS work: every identity is reported not read. The prober tests script it.
        kill_recorded: Arc::new(|ids, _| chaldea_pty_runtime::orphan::OrphanReport {
            fates: ids
                .iter()
                .map(|id| (id.clone(), chaldea_pty_runtime::orphan::Fate::NotRead))
                .collect(),
        }),
        claude_settings: "/work/state/claude-settings.json".to_owned(),
        tail_poll: crate::transcript::TAIL_POLL,
        attach: crate::attach::AttachConfig::default(),
        pwa: &[],
    }
}

impl Rig {
    pub(crate) async fn new() -> Rig {
        Rig::with(HostOs::Macos, |_| {}).await
    }

    pub(crate) async fn with(host: HostOs, tune: impl FnOnce(&mut SupervisorConfig)) -> Rig {
        Rig::on(TestStore::new(), host, tune).await
    }

    pub(crate) async fn on(
        store: TestStore,
        host: HostOs,
        tune: impl FnOnce(&mut SupervisorConfig),
    ) -> Rig {
        Rig::on_mounts(
            store,
            host,
            tune,
            |_| {},
            |sup| mounts_with(sup, seams(sup)),
        )
        .await
    }

    /// A rig whose `/hook` walks the test's process chains instead of the OS's; also
    /// returns that `/hook`'s ingest, for its sync points.
    pub(crate) async fn with_chains(
        tune: impl FnOnce(&mut SupervisorConfig),
    ) -> (
        Rig,
        Arc<support::hook::Chains>,
        Arc<crate::hook::HookIngest>,
    ) {
        Rig::with_chains_on(HostOs::Macos, tune, crate::hook::HookConfig::default()).await
    }

    /// [`Rig::with_chains`] mapped to `host`, its `/hook` on `hook`.
    pub(crate) async fn with_chains_on(
        host: HostOs,
        tune: impl FnOnce(&mut SupervisorConfig),
        hook: crate::hook::HookConfig,
    ) -> (
        Rig,
        Arc<support::hook::Chains>,
        Arc<crate::hook::HookIngest>,
    ) {
        Rig::with_chains_http(host, tune, hook, |_| {}).await
    }

    /// [`Rig::with_chains_on`] whose server configuration `http` tunes.
    pub(crate) async fn with_chains_http(
        host: HostOs,
        tune: impl FnOnce(&mut SupervisorConfig),
        hook: crate::hook::HookConfig,
        http: impl FnOnce(&mut crate::http::HttpConfig),
    ) -> (
        Rig,
        Arc<support::hook::Chains>,
        Arc<crate::hook::HookIngest>,
    ) {
        let chains = Arc::new(support::hook::Chains::default());
        let made = Arc::new(Mutex::new(None));
        let (c, m) = (chains.clone(), made.clone());
        let rig = Rig::on_mounts(TestStore::new(), host, tune, http, move |sup| {
            let chained = crate::hook::HookSeams {
                procs: c,
                ..seams(sup)
            };
            let ingest = crate::hook::HookIngest::new(hook, chained);
            *m.lock().unwrap() = Some(ingest.clone());
            mounts_with(sup, seams(sup)).with(ingest.mount())
        })
        .await;
        let ingest = made.lock().unwrap().take().unwrap();
        (rig, chains, ingest)
    }

    async fn on_mounts(
        store: TestStore,
        host: HostOs,
        tune: impl FnOnce(&mut SupervisorConfig),
        http: impl FnOnce(&mut crate::http::HttpConfig),
        mounts: impl FnOnce(&Arc<Supervisor>) -> crate::http::Mounts,
    ) -> Rig {
        // Stopped: no deadline taken from it passes however slowly the host runs. A test
        // whose subject needs time to pass resumes it (`rig.clock.resume()`) or advances it.
        let clock = Arc::new(SkewClock::default());
        clock.stop();
        let sink = Arc::new(RecordingSink::default());
        let pty = Arc::new(FakePty::new());
        let pauses = Arc::new(Pauser::default());
        let mut cfg = config(&pty, &clock, &pauses);
        tune(&mut cfg);
        let side = crate::supervisor::send::install(&mut cfg);
        let sup = new_supervisor(
            host,
            cfg,
            side,
            store.store().clone(),
            sink.clone(),
            0,
            Uuid::new_v4().to_string(),
        );
        sup.start_send_driver();
        let srv = TestServer::start_parts(
            store,
            mounts(&sup),
            registry(&sup),
            (clock.clone(), sink.clone()),
            http,
        )
        .await;
        let _ = sup.nudge.set(srv.server.revocation_nudge());
        let owner = srv.token(TokenKind::Owner, None, None).await;
        Rig {
            srv,
            sup,
            pty,
            pauses,
            clock,
            sink,
            owner,
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    /// A command with a new `command_id`, as the owner.
    pub(crate) async fn cmd(&self, name: &str, args: &serde_json::Value) -> Reply {
        self.cmd_as(&self.owner, name, cid(), args).await
    }

    pub(crate) async fn cmd_as(
        &self,
        secret: &str,
        name: &str,
        id: CommandId,
        args: &serde_json::Value,
    ) -> Reply {
        self.srv
            .send(&self.srv.command(name, secret, id, &args.to_string()))
            .await
    }

    /// Spawns a Codex thread with `prompt` in the rig's cwd and returns its ids.
    pub(crate) async fn spawn_codex(&self, prompt: Option<&str>) -> (ThreadId, LaunchId) {
        let mut args = serde_json::json!({
            "agent": "codex",
            "cwd": self.cwd.path().to_string_lossy(),
        });
        if let Some(p) = prompt {
            args["prompt"] = p.into();
        }
        let r = self.cmd("spawn", &args).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let v = json(&r);
        (
            serde_json::from_value(v["thread_id"].clone()).unwrap(),
            serde_json::from_value(v["launch_id"].clone()).unwrap(),
        )
    }

    /// Binds launch `l` of thread `t` to `session`, as a `SessionStart` hook would.
    pub(crate) async fn bind(&self, t: ThreadId, l: LaunchId, session: &str) {
        self.srv
            .append(vec![support::bound(t, l, session, None)])
            .await;
    }

    /// Spawns a Codex thread, binds its session, and stops it (death confirmed).
    pub(crate) async fn stopped_bound(&self) -> (ThreadId, LaunchId) {
        let (t, l) = self.spawn_codex(None).await;
        self.bind(t, l, &format!("session-{l}")).await;
        let r = self
            .cmd("stop", &serde_json::json!({ "thread_id": t }))
            .await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        (t, l)
    }

    pub(crate) fn thread(&self, t: ThreadId) -> ThreadRow {
        self.srv.store.read(move |r| r.thread(t)).unwrap()
    }

    pub(crate) fn launch(&self, l: LaunchId) -> LaunchRow {
        self.srv.store.read(move |r| r.launch(l)).unwrap()
    }

    /// The thread stream's events as `(kind, payload)`.
    pub(crate) fn events(&self, t: ThreadId) -> Vec<(String, serde_json::Value)> {
        self.srv.store.read(move |r| {
            Ok(r.events_after(StreamId::Thread(t), 0, 10_000)?
                .events
                .into_iter()
                .map(|e| {
                    (
                        serde_json::to_value(&e.kind)
                            .unwrap()
                            .as_str()
                            .unwrap()
                            .to_owned(),
                        serde_json::from_str(e.payload.get()).unwrap(),
                    )
                })
                .collect())
        })
    }

    pub(crate) fn kinds(&self, t: ThreadId) -> Vec<String> {
        self.events(t).into_iter().map(|(k, _)| k).collect()
    }

    /// Waits (10 s at most) until `cond` holds.
    pub(crate) async fn until(&self, what: &str, cond: impl FnMut() -> bool) {
        support::http::eventually(Duration::from_secs(10), what, cond).await;
    }
}

/// The leader identity the launch row of `l` records.
pub(crate) fn prober_leader(rig: &Rig, l: LaunchId) -> chaldea_pty_runtime::ProcessIdentity {
    let row = rig.launch(l);
    chaldea_pty_runtime::ProcessIdentity {
        pid: row.pid.unwrap(),
        start_time: row.start_time.unwrap_or_default(),
        group: row.pgid_or_job.unwrap_or_default(),
    }
}

pub(crate) fn cid() -> CommandId {
    CommandId(Uuid::new_v4())
}

pub(crate) fn json(r: &Reply) -> serde_json::Value {
    serde_json::from_slice(&r.body).unwrap()
}
