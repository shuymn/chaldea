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

/// The route's mount over `sup`.
pub(crate) fn mount(sup: Weak<Supervisor>) -> Mount {
    mount_with(Arc::new(Attach {
        sup,
        reg: Registry::default(),
        cfg: AttachConfig::default(),
    }))
}

/// The route's mount over `a`.
pub(crate) fn mount_with(a: Arc<Attach>) -> Mount {
    let route =
        spec(&Method::GET, ATTACH_PATH).unwrap_or_else(|| panic!("GET {ATTACH_PATH} is in ROUTES"));
    Mount::upgrade(route, Handler(a))
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
