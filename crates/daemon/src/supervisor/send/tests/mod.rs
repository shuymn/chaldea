//! Send-side unit tests (send §5.2, §6): `FakePty`, the in-process store and server,
//! `FakeDecoder` behind the real `/hook` route, and the send side's test points.

mod ack;
mod claude;
mod evidence;
mod flow;
mod gate;
mod human;
mod outcomes;
mod probe;
mod timers;
mod turn_order;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use chaldea_core::adapter::HostOs;
use chaldea_core::ids::{Agent, InputId, LaunchId, ThreadId};
use chaldea_core::input::InputState;
use chaldea_pty_runtime::fake::FakeChild;
use chaldea_pty_runtime::process::AncestryEnd;
use serde_json::{Value, json};

use super::SendLaunch;
use crate::hook::HookIngest;
use crate::store::InputRow;
use crate::supervisor::SupervisorConfig;
use crate::supervisor::tests::support::hook::{Chains, Hook, Launch, chain};
use crate::supervisor::tests::support::http::Reply;
use crate::supervisor::tests::{Rig, cid, json as body};

/// DECSET 2004 on.
pub(crate) const PASTE_ON: &[u8] = b"\x1b[?2004h";
/// DECSET 2004 off.
pub(crate) const PASTE_OFF: &[u8] = b"\x1b[?2004l";

/// A supervisor rig whose `/hook` reaches each launch's child through the test's chains.
pub(crate) struct SRig {
    pub rig: Rig,
    pub chains: Arc<Chains>,
    pub ingest: Arc<HookIngest>,
    next_pid: AtomicU32,
}

/// One spawned launch with its hook identity and its fake child.
pub(crate) struct Live {
    pub t: ThreadId,
    pub l: LaunchId,
    pub hook: Launch,
    pub child: FakeChild,
    /// The emitter pid whose chain reaches this child.
    pub pid: u32,
    pub session: String,
}

impl std::ops::Deref for SRig {
    type Target = Rig;
    fn deref(&self) -> &Rig {
        &self.rig
    }
}

impl SRig {
    /// Stops the server.
    pub(crate) async fn stop(self) {
        self.rig.srv.stop().await;
    }

    pub(crate) async fn new() -> SRig {
        SRig::with(|_| {}).await
    }

    pub(crate) async fn with(tune: impl FnOnce(&mut SupervisorConfig)) -> SRig {
        SRig::on(HostOs::Macos, tune).await
    }

    /// A rig with the send workers and driver running.
    pub(crate) async fn on(host: HostOs, tune: impl FnOnce(&mut SupervisorConfig)) -> SRig {
        let (rig, chains, ingest) = Rig::with_chains_on(host, |c| {
            c.send_workers = true;
            tune(c);
        })
        .await;
        SRig {
            rig,
            chains,
            ingest,
            next_pid: AtomicU32::new(9100),
        }
    }

    /// Spawns a thread of `agent` (with `prompt`) and returns the launch and the reply body.
    pub(crate) async fn spawn(&self, agent: Agent, prompt: Option<&str>) -> (Live, Value) {
        let mut args = json!({
            "agent": crate::supervisor::tests::support::hook::agent_name(agent),
            "cwd": self.cwd.path().to_string_lossy(),
        });
        if let Some(p) = prompt {
            args["prompt"] = p.into();
        }
        let r = self.cmd("spawn", &args).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        let v = body(&r);
        let t: ThreadId = serde_json::from_value(v["thread_id"].clone()).unwrap();
        let l: LaunchId = serde_json::from_value(v["launch_id"].clone()).unwrap();
        (self.adopt(t, l, agent), v)
    }

    /// The hook identity and the child of the launch the runtime spawned last.
    pub(crate) fn adopt(&self, t: ThreadId, l: LaunchId, agent: Agent) -> Live {
        let spec = self.pty.spawns().pop().unwrap();
        let token = spec
            .env
            .iter()
            .find(|(k, _)| {
                chaldea_pty_runtime::env_key_eq(k, std::ffi::OsStr::new("CHALDEA_HOOK_TOKEN"))
            })
            .map(|(_, v)| v.to_string_lossy().into_owned())
            .unwrap();
        let child = self.pty.take_child().unwrap();
        let id = child.identity();
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        let name = crate::supervisor::tests::support::hook::agent_name(agent);
        self.chains.set(
            pid,
            chain(
                &[
                    (pid, "3000", "chaldea"),
                    (id.pid, &id.start_time, name),
                    (1, "1", "launchd"),
                ],
                AncestryEnd::Root,
            ),
        );
        Live {
            t,
            l,
            hook: Launch {
                thread: t,
                launch: l,
                agent,
                token,
            },
            child,
            pid,
            // Claude's session id is its launch id (`--session-id`); Codex picks its own.
            session: match agent {
                Agent::Claude => l.to_string(),
                Agent::Codex => format!("session-{l}"),
            },
        }
    }

    /// A hook of `live` named `event` with `extra` fields merged into its body.
    pub(crate) fn hook_of(&self, live: &Live, event: &str, extra: &Value) -> Hook {
        let mut v = json!({ "hook_event_name": event, "session_id": live.session });
        if event == "SessionStart" {
            v["transcript_path"] = Value::Null;
        }
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, x) in e {
                o.insert(k.clone(), x.clone());
            }
        }
        Hook::new(&live.hook, &serde_json::to_vec(&v).unwrap()).pid(Some(live.pid))
    }

    pub(crate) async fn post(&self, h: &Hook) -> Reply {
        self.srv.send(&h.req(&self.srv)).await
    }

    /// Posts `h`, a hook of `live`, with `live`'s send worker parked ([`SRig::worker_parked`])
    /// unless one of its send side's test points is armed or running: a test that holds the
    /// worker, or is about to, keeps the order it set up.
    pub(crate) async fn post_for(&self, live: &Live, h: &Hook) -> Reply {
        if self.points_idle(live) {
            self.worker_parked(live, self.post(h)).await
        } else {
            self.post(h).await
        }
    }

    /// Whether no test point of `live`'s send side that runs on the worker or the writer is
    /// set or running (`observe` runs on the hook's own request).
    fn points_idle(&self, live: &Live) -> bool {
        let sl = self.sl(live);
        let h = &sl.hooks;
        [
            &h.completion,
            &h.before_submit,
            &h.after_submit,
            &h.record_job,
            &h.settle_job,
            &h.open_job,
            &h.open_after_flags,
            &h.after_written_read,
            &h.finished,
            &h.after_read,
            &h.after_record,
        ]
        .into_iter()
        .all(|slot| {
            crate::supervisor::send::lock(slot).is_none() && !Running::of(slot).is_running()
        })
    }

    /// Posts a hook of `live` ([`SRig::post_for`]) and requires 200.
    pub(crate) async fn hook(&self, live: &Live, event: &str, extra: &Value) -> Reply {
        let r = self.post_for(live, &self.hook_of(live, event, extra)).await;
        assert_eq!(r.status, 200, "{event}: {r:?}");
        r
    }

    /// Turns DECSET 2004 on in the child and waits until the session's VT shows it.
    pub(crate) async fn paste(&self, live: &Live, on: bool) {
        live.child.emit(if on { PASTE_ON } else { PASTE_OFF });
        self.until_async("the VT mode changed", || async {
            self.modes(live)
                .await
                .is_some_and(|m| m.modes.bracketed_paste == on)
        })
        .await;
    }

    /// A subscription to the launch's output, taken now. The drain publishes a piece's modes
    /// before it delivers the piece, so a delivered byte has its mode change applied; the
    /// wait needs no op lock, which a worker blocked on the writer may hold.
    pub(crate) async fn watch(&self, live: &Live) -> chaldea_pty_runtime::OutputReceiver {
        let (sink, rx) = chaldea_pty_runtime::channel_sink(64);
        let e = self.sup.entry(live.l).unwrap();
        e.with_session(&self.sup, self.sup.now() + Duration::from_secs(5), |s| {
            s.subscribe(sink, std::time::Instant::now() + Duration::from_secs(5))
        })
        .await
        .unwrap()
        .unwrap();
        rx
    }

    /// Waits until `rx` delivered `bytes`, so their mode changes are applied.
    pub(crate) async fn delivered(rx: &chaldea_pty_runtime::OutputReceiver, bytes: &[u8]) {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while !seen.windows(bytes.len()).any(|w| w == bytes) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            assert!(!left.is_zero(), "output {bytes:?} delivered");
            if let Ok(chaldea_pty_runtime::Received::Bytes { bytes: b, .. }) =
                tokio::task::block_in_place(|| rx.recv_timeout(left.min(Duration::from_millis(50))))
            {
                seen.extend_from_slice(&b);
            }
        }
    }

    /// The session's modes, or `None` when the entry is gone or busy.
    pub(crate) async fn modes(&self, live: &Live) -> Option<chaldea_pty_runtime::ModeView> {
        let e = self.sup.entry(live.l)?;
        e.with_session(&self.sup, self.sup.now() + Duration::from_secs(1), |s| {
            s.modes()
        })
        .await
        .ok()
    }

    /// Runs `f` with `live`'s send worker parked after a pass's read, outside the live map and
    /// out of the runtime's way (`block_in_place`): an intake meanwhile never meets the
    /// worker's map lock (which it would answer with no facts), and a test holding the map
    /// never has the worker block a runtime thread on it. The worker is released before the
    /// caller waits on anything it records.
    pub(crate) fn worker_parked<'a, T: 'a>(
        &'a self,
        live: &'a Live,
        f: impl std::future::Future<Output = T> + 'a,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>> {
        // Boxed: the setup helpers nest it, and an inline future would grow theirs.
        Box::pin(async move {
            let sl = self.sl(live);
            let (entered_tx, entered) = std::sync::mpsc::channel::<()>();
            let (go, wait) = std::sync::mpsc::channel::<()>();
            gate::set(&sl.hooks.after_read, move || {
                let _ = entered_tx.send(());
                tokio::task::block_in_place(|| {
                    let _ = wait.recv();
                });
            });
            sl.wake.notify_one();
            // Real time: a worker that never reaches its read fails the test instead of hanging it.
            tokio::task::block_in_place(|| entered.recv_timeout(Duration::from_secs(30)))
                .expect("the send worker parked after its read");
            let out = f.await;
            go.send(()).unwrap();
            out
        })
    }

    /// A Claude launch, DECSET 2004 on, bound by its `SessionStart` (its evidence recorded).
    pub(crate) async fn claude(&self) -> Live {
        let (live, _) = self.spawn(Agent::Claude, None).await;
        self.paste(&live, true).await;
        self.hook(&live, "SessionStart", &json!({})).await;
        assert!(
            self.launch(live.l).readiness.is_some(),
            "session_start: {:?}",
            self.events(live.t)
        );
        live
    }

    /// A launch of `agent` with its evidence: Claude bound by its `SessionStart`, Codex bound
    /// and asserted ready.
    pub(crate) async fn bound(&self, agent: Agent) -> Live {
        match agent {
            Agent::Claude => self.claude().await,
            Agent::Codex => {
                let (live, _) = self.spawn(Agent::Codex, None).await;
                self.paste(&live, true).await;
                self.hook(&live, "SessionStart", &json!({})).await;
                self.cmd_ok("ready", &json!({ "thread_id": live.t }), 200)
                    .await;
                live
            }
        }
    }

    /// `send` as the owner; returns the reply.
    pub(crate) async fn send_text(&self, live: &Live, text: &str) -> Reply {
        self.cmd("send", &json!({ "thread_id": live.t, "text": text }))
            .await
    }

    /// `send` as the owner, requiring 200; returns the input.
    pub(crate) async fn send(&self, live: &Live, text: &str) -> InputId {
        let r = self.send_text(live, text).await;
        assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
        serde_json::from_value(body(&r)["input_id"].clone()).unwrap()
    }

    pub(crate) fn input(&self, id: InputId) -> InputRow {
        self.srv.store.read(move |r| r.input(id)).unwrap()
    }

    /// Waits until input `id` is in `state`.
    pub(crate) async fn until_input(&self, id: InputId, state: InputState) {
        self.until(&format!("input {id} is {state:?}"), || {
            self.input(id).state == state
        })
        .await;
    }

    pub(crate) fn sl(&self, live: &Live) -> Arc<SendLaunch> {
        self.sup.send.launch(live.l).unwrap()
    }

    /// Wakes `live`'s worker and waits until two more passes ended, so one whole pass
    /// started after the call.
    pub(crate) async fn pass(&self, live: &Live) {
        let sl = self.sl(live);
        let n = sl.passes.load(Ordering::SeqCst);
        for k in 1..=2 {
            sl.wake.notify_one();
            self.until("a pass ended", || sl.passes.load(Ordering::SeqCst) >= n + k)
                .await;
        }
    }

    /// The session's test probe.
    pub(crate) async fn probe(&self, live: &Live) -> chaldea_pty_runtime::fake::Probe {
        let e = self.sup.entry(live.l).unwrap();
        e.with_session(&self.sup, self.sup.now() + Duration::from_secs(5), |s| {
            s.probe()
        })
        .await
        .unwrap()
    }

    /// Submits an `Attach` unit of `bytes` as PR 11's attach does (a human unit the worker
    /// records); returns its report.
    pub(crate) async fn attach(
        &self,
        live: &Live,
        bytes: &[u8],
    ) -> tokio::sync::oneshot::Receiver<super::KeyReport> {
        let sl = self.sl(live);
        let e = self.sup.entry(live.l).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let not_after = self.sup.real(self.sup.now() + Duration::from_secs(60));
        let unit = chaldea_pty_runtime::WriteUnit::bytes(
            chaldea_pty_runtime::WriteOrigin::Attach,
            bytes.to_vec(),
        );
        e.with_session(
            &self.sup,
            self.sup.now() + Duration::from_secs(5),
            move |s| {
                sl.submit_human(
                    s,
                    unit,
                    chaldea_pty_runtime::Guard::until(not_after),
                    Some(tx),
                )
            },
        )
        .await
        .unwrap()
        .unwrap();
        rx
    }

    /// The next hook's receipt ordinal for `live`.
    pub(crate) fn next_ordinal(&self, live: &Live) -> u64 {
        self.sl(live).latest_intake.load(Ordering::SeqCst) + 1
    }

    /// Ticks at the clock's now.
    pub(crate) async fn tick(&self) {
        self.sup.tick(self.sup.now()).await;
    }

    /// The thread stream's events of `kind`, as payloads.
    pub(crate) fn of_kind(&self, t: ThreadId, kind: &str) -> Vec<Value> {
        self.events(t)
            .into_iter()
            .filter(|(k, _)| k == kind)
            .map(|(_, v)| v)
            .collect()
    }

    /// Waits (10 s at most) until the async `cond` holds.
    pub(crate) async fn until_async<F: std::future::Future<Output = bool>>(
        &self,
        what: &str,
        mut cond: impl FnMut() -> F,
    ) {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        while !cond().await {
            assert!(std::time::Instant::now() < end, "not within 10 s: {what}");
            tokio::task::yield_now().await;
        }
    }

    /// A command as the owner with a fresh `command_id`, requiring `status`.
    pub(crate) async fn cmd_ok(&self, name: &str, args: &Value, status: u16) -> Value {
        let r = self.cmd_as(&self.owner, name, cid(), args).await;
        assert_eq!(
            r.status,
            status,
            "{name}: {}",
            String::from_utf8_lossy(&r.body)
        );
        serde_json::from_slice(&r.body).unwrap_or(Value::Null)
    }
}

/// The test points of the send sides now running, by slot address: a point taken from its
/// slot and running (a worker or writer parked in it) leaves the slot empty.
static RUNNING: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// One slot's entry in [`RUNNING`].
#[derive(Clone, Copy)]
pub(crate) struct Running(usize);

impl Running {
    pub(crate) fn of<T>(slot: &std::sync::Mutex<T>) -> Running {
        Running(std::ptr::from_ref(slot) as usize)
    }

    fn is_running(self) -> bool {
        crate::supervisor::send::lock(&RUNNING).contains(&self.0)
    }

    /// Marks the slot running until the guard drops (also while unwinding).
    pub(crate) fn enter(self) -> impl Drop {
        crate::supervisor::send::lock(&RUNNING).push(self.0);
        Left(self.0)
    }
}

struct Left(usize);

impl Drop for Left {
    fn drop(&mut self) {
        let mut r = crate::supervisor::send::lock(&RUNNING);
        if let Some(i) = r.iter().position(|k| *k == self.0) {
            r.swap_remove(i);
        }
    }
}
