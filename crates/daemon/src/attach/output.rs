//! The socket's output buffer (`docs/design/t1-attach.md` D8, I-8): the subscription's sink
//! appends to one shared buffer and wakes the socket; the socket takes frames from it. Over
//! its budget the sink records the lag and stops.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

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
}

/// One socket's output.
pub(crate) struct OutBuf {
    st: Mutex<State>,
    pub wake: Notify,
    /// The sink accepts bytes while set.
    pub active: Arc<AtomicBool>,
    max: usize,
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

    /// The latched terminal event.
    pub(crate) fn ending(&self) -> Option<Ending> {
        lock(&self.st).ending
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
        }
        st.ending.unwrap_or(e)
    }

    /// Stops the sink; the buffered bytes stay (the frozen prefix).
    pub(crate) fn stop(&self) {
        self.active.store(false, Ordering::Release);
    }
}
