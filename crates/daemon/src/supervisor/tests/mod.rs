//! Supervisor unit tests (design §5.2, §6): `FakePty`, the in-process store and server, and
//! `cfg(test)` pause points.

mod launch;
mod stop;

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

use super::components::{mounts, new_supervisor, registry};
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
}

struct Hold {
    reached: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Hold {
    fn block(self) {
        let _ = self.reached.send(());
        let _ = self.release.recv_timeout(Duration::from_secs(60));
    }
}

/// The supervisor's pause points, each armed at most once at a time.
#[derive(Default)]
pub(crate) struct Pauser {
    holds: Mutex<HashMap<Pause, Hold>>,
}

impl Pauser {
    pub(crate) fn arm(&self, p: Pause) -> Armed {
        let (reached_tx, reached) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        self.holds.lock().unwrap().insert(
            p,
            Hold {
                reached: reached_tx,
                release: release_rx,
            },
        );
        Armed { reached, release }
    }

    fn take(&self, p: Pause) -> Option<Hold> {
        self.holds.lock().unwrap().remove(&p)
    }
}

impl Pauses for Pauser {
    fn at(&self, p: Pause) -> BoxFuture<'static, ()> {
        let hold = self.take(p);
        Box::pin(async move {
            if let Some(h) = hold {
                let _ = tokio::task::spawn_blocking(move || h.block()).await;
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

/// The test configuration: `FakePty`, the given clock and pauses, no tick task.
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
        kill_budget: Duration::from_secs(5),
        close_bounds: CloseBounds::default(),
        pty_size: PtySize::new(80, 24).unwrap(),
        pauses: pauses.clone(),
        hook: crate::hook::HookConfig::default(),
        exit_steps: Vec::new(),
        tick_steps: Vec::new(),
        tick_task: false,
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
        let clock = Arc::new(SkewClock::default());
        let sink = Arc::new(RecordingSink::default());
        let pty = Arc::new(FakePty::new());
        let pauses = Arc::new(Pauser::default());
        let mut cfg = config(&pty, &clock, &pauses);
        tune(&mut cfg);
        let sup = new_supervisor(host, cfg, store.store().clone(), sink.clone(), 0);
        let srv = TestServer::start_parts(
            store,
            mounts(host),
            registry(&sup),
            (clock.clone(), sink.clone()),
            |_| {},
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

pub(crate) fn cid() -> CommandId {
    CommandId(Uuid::new_v4())
}

pub(crate) fn json(r: &Reply) -> serde_json::Value {
    serde_json::from_slice(&r.body).unwrap()
}
