//! Chaldea's PTY session runtime (`docs/design/pty-runtime.md`): the [`PtyRuntime`]
//! backend seam, and the [`Session`] every backend shares: one drain thread feeding a
//! headless `alacritty_terminal` VT, one writer thread that is the only code writing to
//! the PTY and records every write with its origin and byte span, and one waiter thread.
//!
//! The crate records facts (the origin, order, byte span, and start and finish of every
//! write; the terminal modes; the screen). Gate and ack policy stay with the caller.
//!
//! # Threads, locks, and callbacks
//!
//! The crate is synchronous: std threads and blocking I/O; it depends on no async runtime
//! (CI checks that `tokio` is not in its dependency tree). Callers bridge to their runtime
//! with callbacks that do not block.
//!
//! Lock order: control lock → VT lock → queue lock; no code takes them in another order.
//! No callback (`Completion`, `OutputSink`, `on_event`) runs under an internal lock. A
//! callback may call the non-blocking `Session` methods (`submit`, `cancel`,
//! `last_started`, `writes_after`, `modes`, `snapshot`, `subscribe`, `unsubscribe`,
//! `status`; `snapshot` and `subscribe` only with a short deadline, which bounds both the
//! lock attempt and the grid copy) but not `resize`, `kill`, or `close`: a sink runs on
//! the drain thread, and a resize can wait for it.
//!
//! # Features
//!
//! `fake` adds [`fake::FakePty`] and the test probes; no default-feature build has them.

// Off macOS, until the Windows backend (PR 4) builds a `SpawnedPty`, only `FakePty` does, so
// a default-feature build there has no caller of the drain and spawn internals.
#![cfg_attr(
    all(not(feature = "fake"), not(target_os = "macos")),
    allow(dead_code, reason = "the Windows backend arrives in PR 4")
)]

mod backend;
mod drain;
mod limits;
mod origin;
mod session;
mod snapshot;
mod spawn;
#[cfg(target_os = "macos")]
mod unix;
mod vt;
mod writer;

#[cfg(feature = "fake")]
pub mod fake;

/// Counts an occurrence of a [`fake::ProbePoint`]; nothing without the `fake` feature.
macro_rules! probe {
    ($sh:expr, $point:ident) => {
        #[cfg(feature = "fake")]
        $sh.probe.hit($crate::fake::ProbePoint::$point);
    };
}
pub(crate) use probe;

pub use backend::{
    ChildExit, Cleanup, CloseReport, DrainSink, KillReport, MAX_DIMENSION, NeverStartedKind,
    ProcessIdentity, PtyChild, PtyControl, PtyRuntime, PtySize, SizeError, SpawnError, SpawnSpec,
    SpawnedPty,
};
pub use limits::{CloseBounds, Limits};
pub use origin::{ClassSet, OriginClass, ReplyKind, WriteOrigin};
#[cfg(feature = "fake")]
pub use session::StartFault;
pub use session::{
    CloseOutcome, Flow, OutputEnd, OutputEvent, OutputReceiver, OutputSink, Received, Released,
    Resized, ResponderStatus, Session, SessionConfig, SessionEvent, SessionStatus, SubscribeError,
    SubscriptionId, VtUnavailable, channel_sink,
};
pub use snapshot::{Cell, CellWidth, Cursor, GridError, Line, Snapshot, TermModes, decode_grid};
pub use spawn::with_spawn_lock;
#[cfg(target_os = "macos")]
pub use unix::native;
pub use vt::ModeView;
pub use writer::{
    Basis, Completion, CompletionReceiver, Guard, LogTruncated, MAX_PARTS, Part, Recheck, Refusal,
    SubmitError, WriteContext, WriteOutcome, WriteRecord, WriteTicket, WriteUnit,
    completion_channel,
};
