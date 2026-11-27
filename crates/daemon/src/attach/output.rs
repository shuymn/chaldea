//! The socket's output buffer (`docs/design/t1-attach.md` D8, I-8): the subscription's sink
//! appends to one shared buffer and wakes the socket; the socket takes frames from it. Over
//! its budget the sink records the lag and stops.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chaldea_core::attach::EndReason;
use chaldea_pty_runtime::{Flow, OutputEnd, OutputEvent, OutputSink};
use tokio::sync::Notify;

use crate::supervisor::lock;

/// A terminal event (R-8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ending {
    Ended(EndReason),
    Lagged,
}

#[derive(Default)]
struct State {
    data: VecDeque<u8>,
    /// The first terminal event, latched once: the sink's lag or end, or the session gone.
    /// Nothing is buffered after it (the prefix is frozen under the same lock).
    ending: Option<Ending>,
    /// When it latched (supervisor clock), from the latch hook.
    latched_at: Option<Instant>,
}

/// Runs at the latch, under the buffer's lock: revokes the socket's grant and reads the
/// clock the drain deadline counts from.
pub(crate) type LatchHook = Box<dyn Fn() -> Option<Instant> + Send + Sync>;

/// One socket's output.
pub(crate) struct OutBuf {
    st: Mutex<State>,
    pub wake: Notify,
    /// The sink accepts bytes while set.
    pub active: Arc<AtomicBool>,
    max: usize,
    on_latch: Mutex<Option<LatchHook>>,
}

/// What the buffer holds for the socket.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    Bytes(Vec<u8>),
    /// Nothing buffered.
    Empty,
}

impl OutBuf {
    pub(crate) fn new(max: usize) -> Arc<OutBuf> {
        Arc::new(OutBuf {
            st: Mutex::new(State::default()),
            wake: Notify::new(),
            active: Arc::new(AtomicBool::new(true)),
            max,
            on_latch: Mutex::new(None),
        })
    }

    /// The subscription's sink: never blocks (one short lock), never queues per piece.
    pub(crate) fn sink(self: &Arc<Self>) -> OutputSink {
        let me = self.clone();
        Box::new(move |ev: OutputEvent<'_>| {
            if !me.active.load(Ordering::Acquire) {
                return Flow::Stop;
            }
            let mut st = lock(&me.st);
            if st.ending.is_some() {
                return Flow::Stop;
            }
            let flow = match ev {
                OutputEvent::Bytes { bytes, .. } => {
                    if st.data.len() + bytes.len() > me.max {
                        me.latch_in(&mut st, Ending::Lagged);
                        Flow::Stop
                    } else {
                        st.data.extend(bytes);
                        Flow::Continue
                    }
                }
                OutputEvent::End(e) => {
                    let reason = match e {
                        OutputEnd::Eof => EndReason::Eof,
                        OutputEnd::Error(_) => EndReason::Error,
                    };
                    me.latch_in(&mut st, Ending::Ended(reason));
                    Flow::Stop
                }
            };
            drop(st);
            me.wake.notify_one();
            flow
        })
    }

    /// Up to `max` buffered bytes.
    pub(crate) fn take(&self, max: usize) -> Taken {
        let mut st = lock(&self.st);
        if st.data.is_empty() {
            return Taken::Empty;
        }
        let n = st.data.len().min(max);
        Taken::Bytes(st.data.drain(..n).collect())
    }

    /// The latched terminal event and when it latched (`None` before the latch hook was
    /// set).
    pub(crate) fn ending(&self) -> Option<(Ending, Option<Instant>)> {
        let st = lock(&self.st);
        st.ending.map(|e| (e, st.latched_at))
    }

    /// Sets the hook the latch runs (once the socket holds a grant).
    pub(crate) fn on_latch(&self, f: LatchHook) {
        *lock(&self.on_latch) = Some(f);
    }

    /// Latches `e` unless an event is latched already; the latched one.
    pub(crate) fn latch(&self, e: Ending) -> Ending {
        let mut st = lock(&self.st);
        self.latch_in(&mut st, e)
    }

    fn latch_in(&self, st: &mut State, e: Ending) -> Ending {
        if st.ending.is_none() {
            st.ending = Some(e);
            self.active.store(false, Ordering::Release);
            // The grant moves with the latch, before any unit can be permitted after it.
            st.latched_at = lock(&self.on_latch).as_ref().and_then(|f| f());
        }
        st.ending.unwrap_or(e)
    }

    /// Stops the sink; the buffered bytes stay (the frozen prefix).
    pub(crate) fn stop(&self) {
        self.active.store(false, Ordering::Release);
    }
}
