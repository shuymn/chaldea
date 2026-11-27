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

#[derive(Default)]
struct State {
    data: VecDeque<u8>,
    end: Option<EndReason>,
    lagged: bool,
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
            let flow = match ev {
                OutputEvent::Bytes { bytes, .. } => {
                    if st.data.len() + bytes.len() > me.max {
                        st.lagged = true;
                        Flow::Stop
                    } else {
                        st.data.extend(bytes);
                        Flow::Continue
                    }
                }
                OutputEvent::End(e) => {
                    st.end = Some(match e {
                        OutputEnd::Eof => EndReason::Eof,
                        OutputEnd::Error(_) => EndReason::Error,
                    });
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

    /// The output's end, if the sink saw it.
    pub(crate) fn end(&self) -> Option<EndReason> {
        lock(&self.st).end
    }

    /// Whether the sink went over its budget.
    pub(crate) fn lagged(&self) -> bool {
        lock(&self.st).lagged
    }

    /// Stops the sink; the buffered bytes stay (the frozen prefix).
    pub(crate) fn stop(&self) {
        self.active.store(false, Ordering::Release);
    }
}
