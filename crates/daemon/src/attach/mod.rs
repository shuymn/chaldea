//! `GET /api/attach/{thread}` (`docs/design/t1-attach.md`): the web terminal view's socket
//! to one launch. The socket renders the PTY and forwards deliberate human input; it never
//! feeds thread state (AGENTS.md invariant 1).

mod input;
mod output;
mod registry;
mod socket;

#[cfg(test)]
mod tests;

use std::sync::{Arc, Weak};
use std::time::Duration;

use axum::http::Method;
use chaldea_core::attach::{AttachReason, PasteReason};
use chaldea_pty_runtime::{Refusal, SubmitError};
use futures_util::future::BoxFuture;

use crate::http::{Mount, UpgradeHandler, UpgradedSocket, spec};
use crate::supervisor::Supervisor;

pub(crate) use registry::Registry;

/// The route's path.
pub(crate) const ATTACH_PATH: &str = "/api/attach/{thread}";

/// The socket's bounds (`docs/design/t1-attach.md` §2 "Limits").
#[derive(Debug, Clone)]
pub(crate) struct AttachConfig {
    /// Output buffered for the socket; over it the socket lags (4002).
    pub out_max: usize,
    /// One binary output frame.
    pub frame_out: usize,
    /// One raw binary input message; larger answers `input_dropped { too_large }`.
    pub raw_max: usize,
    /// Pending input at which the socket stops reading frames.
    pub watermark: usize,
    /// The whole `attached` frame.
    pub attached_max: usize,
    /// From the terminal event to the last send.
    pub drain: Duration,
    /// A unit's dispatch deadline.
    pub unit_wait: Duration,
    /// How long a partial bracketed-paste marker is held.
    pub hold: Duration,
    /// How often the socket checks that its launch is still live.
    pub gone_poll: Duration,
    /// The startup subscription's wait for the op lock and the VT lock.
    pub subscribe_wait: Duration,
    /// The verification and ping tick in place of the server's `heads_interval` (tests
    /// tick explicitly through [`AttachHooks::tick`]).
    #[cfg(test)]
    pub tick_every: Option<Duration>,
    /// Test points.
    #[cfg(test)]
    pub hooks: Arc<AttachHooks>,
}

/// Test points of the attach sockets.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct AttachHooks {
    /// Wakes every socket loop to recompute its timers (after the test clock moved).
    pub poke: tokio::sync::Notify,
    /// Run, last first, inside verification reads after their snapshot was taken.
    pub verify: std::sync::Mutex<Vec<Box<dyn FnOnce() + Send>>>,
    /// Terminal events latched, by every socket.
    pub latched: std::sync::atomic::AtomicU64,
    /// Runs at the next unit's submission, before `with_session`, on the socket's task.
    pub submit: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Ticks requested by the test ([`AttachHooks::tick`]); every socket runs one tick per
    /// request made after it started.
    pub ticks: std::sync::atomic::AtomicU64,
    /// Notified with each tick request.
    pub tick_now: tokio::sync::Notify,
    /// Ticks every socket has taken (its verification started), by every socket.
    pub ticked: std::sync::atomic::AtomicU64,
    /// Nudges or ticks that found both verification slots taken, by every socket.
    pub saturated: std::sync::atomic::AtomicU64,
    /// Runs on the socket's task right after its loop sampled the terminal latch (none
    /// latched yet).
    pub sampled: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The most bytes of pending input and queued responses retained while a socket was
    /// reading frames, over every socket.
    pub retained_max: std::sync::atomic::AtomicU64,
    /// Loop turns that did not read frames (the watermark reached), by every socket.
    pub paused: std::sync::atomic::AtomicU64,
    /// The registry's entries after its last change by a socket.
    pub registry_len: std::sync::atomic::AtomicUsize,
    /// Units whose report wait began (`Waiting`), by every socket.
    pub waiting: std::sync::atomic::AtomicU64,
    /// Gone polls run, by every socket.
    pub gone_polls: std::sync::atomic::AtomicU64,
    /// Runs on the blocking pool after the next startup subscription returned.
    pub subscribed: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

#[cfg(test)]
impl AttachHooks {
    /// Asks every socket for one tick.
    pub fn tick(&self) {
        self.ticks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.tick_now.notify_waiters();
    }
}

#[cfg(test)]
impl std::fmt::Debug for AttachHooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachHooks").finish_non_exhaustive()
    }
}

impl Default for AttachConfig {
    fn default() -> Self {
        AttachConfig {
            out_max: 1024 * 1024,
            frame_out: 64 * 1024,
            raw_max: 64 * 1024,
            watermark: 64 * 1024,
            attached_max: 192 * 1024,
            drain: Duration::from_secs(5),
            unit_wait: Duration::from_secs(30),
            hold: Duration::from_millis(50),
            gone_poll: Duration::from_secs(1),
            subscribe_wait: Duration::from_millis(250),
            #[cfg(test)]
            tick_every: Some(Duration::from_secs(24 * 3600)),
            #[cfg(test)]
            hooks: Arc::default(),
        }
    }
}

/// The attach mount's handler: the supervisor, the registry, and the bounds.
pub(crate) struct Attach {
    pub sup: Weak<Supervisor>,
    pub reg: Registry,
    pub cfg: AttachConfig,
}

struct Handler(Arc<Attach>);

impl UpgradeHandler for Handler {
    fn run(&self, s: UpgradedSocket) -> BoxFuture<'static, ()> {
        Box::pin(socket::run(self.0.clone(), s))
    }
}

/// The route's mount over `sup`, with `cfg`.
pub(crate) fn mount(sup: Weak<Supervisor>, cfg: AttachConfig) -> Mount {
    let route =
        spec(&Method::GET, ATTACH_PATH).unwrap_or_else(|| panic!("GET {ATTACH_PATH} is in ROUTES"));
    Mount::upgrade(
        route,
        Handler(Arc::new(Attach {
            sup,
            reg: Registry::default(),
            cfg,
        })),
    )
}

/// A writer refusal's reason (R-10).
pub(crate) fn refusal_reason(r: &Refusal) -> AttachReason {
    match r {
        Refusal::Expired => AttachReason::Expired,
        Refusal::BracketedPasteChanged => AttachReason::BracketedPasteOff,
        Refusal::Cancelled
        | Refusal::Closed
        | Refusal::ChildExited
        | Refusal::InputBroken
        | Refusal::FenceMoved
        | Refusal::WriteSinceBasis { .. }
        | Refusal::BasisUnknown { .. }
        | Refusal::Recheck(_)
        | Refusal::RecheckPanicked => AttachReason::NotWritten,
    }
}

/// A refused submission's reason (R-10).
pub(crate) fn submit_reason(e: SubmitError) -> AttachReason {
    match e {
        SubmitError::QueueFull => AttachReason::QueueFull,
        SubmitError::Closed
        | SubmitError::BadShape(_)
        | SubmitError::UnitTooLarge
        | SubmitError::ReplyOrigin => AttachReason::NotWritten,
    }
}

/// An attach reason as a paste's reason.
pub(crate) fn paste_reason(r: AttachReason) -> PasteReason {
    PasteReason::Attach(r)
}
