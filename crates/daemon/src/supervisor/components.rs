//! `SupervisorComponents` (design §2.9 "Components"): the restart reconcilers, the
//! `Supervisor` with its four handlers, the shutdown participant, the mounts, the tick, and
//! the restart prober (`docs/design/supervisor-recovery.md` §2.3).

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chaldea_core::adapter::HostOs;
use futures_util::future::BoxFuture;

use crate::deadline::settle_before;
use crate::hook::HookSeams;
use crate::http::{CommandRegistry, HttpServer, Mounts};
use crate::lifecycle::{
    BuildCtx, Built, Components, ParticipantEnd, Reconciler, ShutdownParticipant,
    receipts_reconciler,
};
use crate::store::Store;

use super::commands::{CloseHandler, ResumeHandler, SpawnHandler, StopHandler};
use super::live::LiveMap;
use super::ops::close_and_record;
use super::send::{self, SendSide};
use super::tracker::Tracker;
use super::workspace::GitSlot;
use super::{Pause, Supervisor, SupervisorConfig, Window, adapters, restart, screen};

/// The components `chaldea daemon run` builds by default.
pub struct SupervisorComponents {
    host_os: HostOs,
    cfg: Option<SupervisorConfig>,
    side: Arc<SendSide>,
    sup: Option<Arc<Supervisor>>,
}

impl SupervisorComponents {
    #[must_use]
    pub fn new(host_os: HostOs, mut cfg: SupervisorConfig) -> SupervisorComponents {
        let side = send::install(&mut cfg);
        SupervisorComponents {
            host_os,
            cfg: Some(cfg),
            side,
            sup: None,
        }
    }
}

impl Components for SupervisorComponents {
    fn reconcilers(&mut self, _store: &Store) -> Vec<Reconciler> {
        let steps = self
            .cfg
            .as_ref()
            .map(|c| c.exit_steps.clone())
            .unwrap_or_default();
        vec![
            receipts_reconciler(),
            restart::launches(steps),
            restart::buffers(),
        ]
    }

    fn build(&mut self, ctx: &BuildCtx) -> Built {
        let cfg = self.cfg.take().unwrap_or_default();
        let sup = new_supervisor(
            self.host_os,
            cfg,
            self.side.clone(),
            ctx.store.clone(),
            ctx.sink.clone(),
            ctx.port,
            ctx.instance.clone(),
        );
        self.sup = Some(sup.clone());
        Built {
            mounts: mounts_with(&sup, seams(&sup)),
            commands: registry(&sup),
            participants: vec![Box::new(Participant(sup))],
        }
    }

    fn attach(&mut self, server: &HttpServer) {
        let Some(sup) = &self.sup else {
            return;
        };
        let _ = sup.nudge.set(server.revocation_nudge());
        if sup.cfg.tick_task {
            sup.rt.spawn(sup.clone().tick_loop());
        }
        sup.start_send_driver();
        let prober = super::prober::run(sup.clone());
        sup.rt.spawn(async move {
            drop(prober.await);
        });
    }
}

/// A supervisor over `store` on the current runtime.
pub(crate) fn new_supervisor(
    host_os: HostOs,
    cfg: SupervisorConfig,
    send: Arc<SendSide>,
    store: Store,
    sink: Arc<dyn crate::http::LifecycleSink>,
    port: u16,
    instance: String,
) -> Arc<Supervisor> {
    Arc::new(Supervisor {
        host_os,
        store,
        sink,
        port,
        cfg,
        live: LiveMap::default(),
        tracker: Arc::new(Tracker::default()),
        git: Arc::new(GitSlot::default()),
        nudge: OnceLock::new(),
        window: OnceLock::new(),
        send,
        rt: tokio::runtime::Handle::current(),
        instance,
        probe_slots: Arc::new(tokio::sync::Semaphore::new(super::PROBE_SLOTS)),
    })
}

/// The command registry with the four launch handlers and the five send handlers.
pub(crate) fn registry(sup: &Arc<Supervisor>) -> CommandRegistry {
    let mut commands = CommandRegistry::mvp();
    commands.register(Box::new(SpawnHandler(sup.clone())));
    commands.register(Box::new(ResumeHandler(sup.clone())));
    commands.register(Box::new(StopHandler(sup.clone())));
    commands.register(Box::new(CloseHandler(sup.clone())));
    send::register(&mut commands, sup);
    commands
}

/// The hook seams of `sup`: the send side's adapter and intake probe, the OS process walk.
pub(crate) fn seams(sup: &Arc<Supervisor>) -> HookSeams {
    HookSeams {
        adapter: Arc::new(send::SupervisorHooks {
            host_os: sup.host_os,
            side: sup.send.clone(),
        }),
        intake: Arc::new(send::SupervisorProbe {
            sup: Arc::downgrade(sup),
            side: sup.send.clone(),
        }),
        ..HookSeams::mvp()
    }
}

/// The mounts of `sup`'s reads over the given hook seams.
pub(crate) fn mounts_with(sup: &Arc<Supervisor>, seams: HookSeams) -> Mounts {
    Mounts::mvp_with(
        seams,
        vec![
            adapters::mount(sup.host_os),
            screen::mount(Arc::downgrade(sup)),
        ],
    )
}

/// The shutdown participant (design §2.9 "Shutdown").
pub(crate) struct Participant(pub(crate) Arc<Supervisor>);

impl ShutdownParticipant for Participant {
    fn name(&self) -> &'static str {
        "supervisor"
    }

    fn shutdown(&self, end: Instant) -> BoxFuture<'_, ParticipantEnd> {
        Box::pin(shutdown(self.0.clone(), end))
    }
}

/// The participant's steps; every instant on the supervisor's clock.
pub(crate) async fn shutdown(sup: Arc<Supervisor>, end: Instant) -> ParticipantEnd {
    let t0 = sup.now();
    let r = end.saturating_duration_since(t0);
    let j = (r / 4).min(Duration::from_millis(300));
    let lock_by = t0 + r / 4;
    let close_end = end.checked_sub(j).unwrap_or(end);
    let _ = sup.window.set(Window { close_end, end });
    // Step 1.
    sup.tracker.close_admission();
    // Step 2, in parallel with steps 3–5.
    let git = git_step(&sup, end);
    let ptys = async {
        // Step 3: the op lock of every published live entry.
        let entries: Vec<_> = sup
            .live
            .all()
            .into_iter()
            .filter(|e| e.published().is_some())
            .collect();
        let locks =
            futures_util::future::join_all(entries.iter().map(|e| e.lock_before(&sup, lock_by)))
                .await;
        // Steps 4–5: each locked slot that still holds a session.
        let closes = entries.iter().zip(locks).filter_map(|(e, g)| {
            let mut g = g?;
            let tracked = e.take_session(&mut g)?;
            let s = sup.clone();
            let e = e.clone();
            Some(async move {
                close_and_record(&s, &e, tracked, Pause::ShutdownCloseEnd).await;
                drop(g);
            })
        });
        futures_util::future::join_all(closes).await;
    };
    tokio::join!(git, ptys);
    // Step 6.
    if sup.tracker.empty_before(end).await {
        ParticipantEnd::Clean
    } else {
        ParticipantEnd::LeftRunning
    }
}

/// Step 2: rounds until the lease is free and `left` is empty, or until `end`.
async fn git_step(sup: &Arc<Supervisor>, end: Instant) {
    loop {
        let changed = sup.git.changed.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        let git = sup.git.clone();
        let s = sup.clone();
        let reap = sup.rt.spawn_blocking(move || {
            s.pauses().blocking(Pause::GitShutdownReap);
            git.reap_left();
            git.idle()
        });
        // A reap that answers after `end` is not waited for: the step ends with it.
        let Some(reaped) = settle_before(|| sup.now(), end, sup.timer(end), reap)
            .await
            .in_time()
        else {
            return;
        };
        let reaped = reaped.unwrap_or(false);
        sup.pauses().at(Pause::GitShutdownRead).await;
        if reaped {
            return;
        }
        let now = sup.now();
        if now >= end {
            return;
        }
        let wake = (now + Duration::from_millis(10)).min(end);
        let _ = settle_before(|| sup.now(), wake, sup.timer(wake), changed).await;
    }
}

impl Supervisor {
    /// The participant's `(close_end, end)`, or two seconds from now when it has not run.
    pub(crate) fn window_or_now(&self) -> (Instant, Instant) {
        self.window.get().map_or_else(
            || {
                let end = self.now() + Duration::from_secs(2);
                (end - Duration::from_millis(300), end)
            },
            |w| (w.close_end, w.end),
        )
    }

    /// The daemon environment a launch's environment is built from.
    pub(crate) fn daemon_env(&self) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        self.cfg
            .env
            .clone()
            .unwrap_or_else(|| std::env::vars_os().collect())
    }

    /// The program and its leading arguments for `agent`: the configured one, else
    /// `claude` or `codex` resolved on the daemon's own `PATH` to an absolute path.
    pub(crate) fn program(
        &self,
        agent: chaldea_core::ids::Agent,
    ) -> Option<Vec<std::ffi::OsString>> {
        if let Some(f) = &self.cfg.program {
            return f(agent);
        }
        let name = match agent {
            chaldea_core::ids::Agent::Claude => "claude",
            chaldea_core::ids::Agent::Codex => "codex",
        };
        let path = chaldea_pty_runtime::host::find_program(name, &std::env::var_os("PATH")?)?;
        tracing::info!(program = %path.display(), "resolved the agent program");
        Some(vec![path.into_os_string()])
    }
}
