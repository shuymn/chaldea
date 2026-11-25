//! PR8-CL-7: the gate's first request fixes the cause and the cutoff; later fatal outcomes
//! and panics raise the exit reason.

use std::sync::Arc;
use std::time::Duration;

use super::super::gate::{GateSink, ShutdownGate, SignalName, StopCause};
use super::super::log::{LogTarget, Logger, TestLog};
use super::super::{FailReason, LockDisposition};
use super::{TestComponents, foreground, start, state};
use crate::http::{LifecycleSink, StoreFatal};

fn gate() -> Arc<ShutdownGate> {
    Arc::new(ShutdownGate::new(
        Duration::from_secs(10),
        Logger::start(LogTarget::Test(TestLog::default())),
    ))
}

fn variants() -> [StoreFatal; 4] {
    [
        StoreFatal::CommitUnknown,
        StoreFatal::WriterGone,
        StoreFatal::Failed,
        StoreFatal::ReceiptUncompleted {
            command_id: chaldea_core::ids::CommandId(uuid::Uuid::from_u128(7)),
        },
    ]
}

/// Each variant passed to the sink of a serving daemon stops it with exit 1 `store_fatal`.
#[test]
fn each_fatal_variant_stops() {
    for f in variants() {
        let dir = state();
        let run = start(
            foreground(&dir.path, Some(super::hold_port())),
            TestComponents::default(),
            Arc::default(),
        );
        run.serving();
        GateSink(run.gate()).store_fatal(f.clone());
        let r = run.join();
        assert_eq!(r.code.code(), 1, "{f:?}");
        assert_eq!(r.reason, Some(FailReason::StoreFatal), "{f:?}");
        assert_eq!(r.lock, LockDisposition::Released, "{f:?}");
    }
}

/// A fatal outcome after a signal keeps the cause and the cutoff but sets the reason.
#[test]
fn late_fatal_sets_the_reason() {
    let g = gate();
    assert!(g.request(StopCause::Signal(SignalName::Term)));
    let first = g.stopping().unwrap().clone();
    std::thread::sleep(Duration::from_millis(20));
    assert!(!g.request(StopCause::StoreFatal(StoreFatal::WriterGone)));
    let after = g.stopping().unwrap();
    assert_eq!(after.cause, StopCause::Signal(SignalName::Term));
    assert_eq!(after.cutoff, first.cutoff);
    assert_eq!(g.worst(), 1);

    // End to end: the run's exit reason is `store_fatal`.
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(super::hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    assert!(run.stop());
    GateSink(run.gate()).store_fatal(StoreFatal::WriterGone);
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::StoreFatal)));
}

/// Signal, then `WriterGone`, then a panic: the reason is `panic`.
#[test]
fn reason_precedence() {
    let dir = state();
    let run = start(
        foreground(&dir.path, Some(super::hold_port())),
        TestComponents::default(),
        Arc::default(),
    );
    run.serving();
    let g = run.gate();
    assert!(g.request(StopCause::Signal(SignalName::Term)));
    assert!(!g.request(StopCause::StoreFatal(StoreFatal::WriterGone)));
    assert!(!g.request(StopCause::Panic));
    let r = run.join();
    assert_eq!((r.code.code(), r.reason), (1, Some(FailReason::Panic)));
}

/// Only the first request is first; the cutoff is first request + 10 s, whichever stop
/// request or signal comes first (the helper tests measure their bounds from that cutoff).
#[test]
fn first_request_fixes_the_cutoff() {
    let firsts = [
        StopCause::StopRequest,
        StopCause::Signal(SignalName::Term),
        StopCause::Signal(SignalName::Int),
        StopCause::Signal(SignalName::Hup),
        StopCause::Signal(SignalName::CtrlC),
        StopCause::Signal(SignalName::CtrlBreak),
    ];
    for first in firsts {
        let g = gate();
        let before = std::time::Instant::now();
        assert!(g.request(first.clone()), "{first:?}");
        let after = std::time::Instant::now();
        let c = g.stopping().unwrap().cutoff;
        assert!(
            c >= before + Duration::from_secs(10) && c <= after + Duration::from_secs(10),
            "{first:?}"
        );
        assert!(!g.request(StopCause::Panic), "{first:?}");
        assert_eq!(g.stopping().unwrap().cause, first);
        assert_eq!(g.stopping().unwrap().cutoff, c, "{first:?}");
    }
}
