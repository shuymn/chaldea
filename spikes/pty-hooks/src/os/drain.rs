//! The raw ConPTY backend's output drain (§4.6), OS-agnostic so its rules
//! are tested on every host. The drain owns the host output read end and
//! keeps reading on every path until EOF (`ClosePseudoConsole` has
//! returned): output is held (bounded) before a consumer attaches,
//! forwarded to the consumer through EOF while there is one, and discarded
//! once the consumer is gone. The drain never depends on its consumer, and
//! output beyond the hold buffer is counted, never dropped silently.

use std::io::{self, Read};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bytes the drain holds while no consumer has attached yet.
pub const DRAIN_HOLD: usize = 1 << 20;

#[derive(Default)]
struct Consumer {
    tx: Option<Sender<Vec<u8>>>,
    held: Vec<u8>,
    /// The consumer went away (its channel closed); discard from here on.
    gone: bool,
    /// Bytes read before a consumer attached that did not fit the hold.
    dropped: u64,
}

#[derive(Default)]
struct DrainState {
    consumer: Mutex<Consumer>,
    finished: AtomicBool,
    bytes: AtomicU64,
    /// How the read loop ended: `eof`, or `error: …` (never taken for EOF).
    end: Mutex<Option<String>>,
}

pub struct Drain {
    state: Arc<DrainState>,
    join: std::thread::JoinHandle<()>,
}

impl Drain {
    /// Starts the drain. If the thread cannot start, `output` is dropped with
    /// the closure, so the host read end is closed before anything else.
    pub fn start(mut output: impl Read + Send + 'static) -> io::Result<Drain> {
        let state = Arc::new(DrainState::default());
        let st = state.clone();
        let join = std::thread::Builder::new()
            .name("conpty-drain".into())
            .spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                let end = loop {
                    let n = match output.read(&mut buf) {
                        Ok(0) => break "eof".to_string(),
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => break format!("error: {e}"),
                        Ok(n) => n,
                    };
                    st.bytes.fetch_add(n as u64, Ordering::SeqCst);
                    let mut c = st.consumer.lock().unwrap_or_else(|e| e.into_inner());
                    match &c.tx {
                        Some(t) => {
                            if t.send(buf[..n].to_vec()).is_err() {
                                c.tx = None;
                                c.gone = true;
                            }
                        }
                        None if !c.gone && c.held.len() + n <= DRAIN_HOLD => {
                            c.held.extend_from_slice(&buf[..n]);
                        }
                        None if !c.gone => c.dropped += n as u64,
                        None => {}
                    }
                };
                *st.end.lock().unwrap_or_else(|e| e.into_inner()) = Some(end);
                let mut c = st.consumer.lock().unwrap_or_else(|e| e.into_inner());
                st.finished.store(true, Ordering::SeqCst);
                // Dropping the sender ends the consumer's reads (EOF).
                c.tx = None;
            })?;
        Ok(Drain { state, join })
    }

    /// Output bytes read so far.
    pub fn bytes(&self) -> u64 {
        self.state.bytes.load(Ordering::SeqCst)
    }

    /// Attaches the session's reader: the held output first, then
    /// everything the drain reads until EOF. Refused when startup output
    /// overflowed the hold buffer (the screen would be incomplete).
    pub fn attach(&self) -> Result<ChannelReader, String> {
        let (tx, rx) = channel();
        let mut c = self
            .state
            .consumer
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if c.dropped > 0 {
            return Err(format!(
                "startup output exceeded the {DRAIN_HOLD}-byte hold buffer ({} bytes dropped)",
                c.dropped
            ));
        }
        let held = std::mem::take(&mut c.held);
        if !held.is_empty() {
            let _ = tx.send(held);
        }
        if !self.state.finished.load(Ordering::SeqCst) {
            c.tx = Some(tx);
        }
        Ok(ChannelReader {
            rx,
            pending: Vec::new(),
        })
    }

    /// How the drain ended (`eof` or `error: …`), once it has.
    pub fn end(&self) -> Option<String> {
        self.state
            .end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Waits for the drain's read loop to end, bounded; true when it did
    /// (at EOF or on a read error: see `end`).
    pub fn wait(&self, bound: Duration) -> bool {
        let end = Instant::now() + bound;
        while !self.join.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.join.is_finished()
    }

    /// Master teardown: waits (bounded) for EOF while an attached reader
    /// keeps receiving the final output (with no reader it is discarded);
    /// how the drain ended, or `None` if it did not within `bound`.
    pub fn finish(self, bound: Duration) -> Option<String> {
        self.wait(bound);
        self.end()
    }
}

/// The session reader: output forwarded by the drain; EOF when it ends.
pub struct ChannelReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(v) => self.pending = v,
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.pending.len());
        out[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

/// Runs a close that may block (`ClosePseudoConsole` on builds that wait
/// for its output to be read) on a helper thread: its duration in
/// milliseconds, or `None` when it did not return within `bound` (the
/// thread is abandoned and the close is recorded as hung).
pub fn close_bounded(close: impl FnOnce() + Send + 'static, bound: Duration) -> Option<u64> {
    let t = Instant::now();
    let (tx, rx) = channel();
    let spawned = std::thread::Builder::new()
        .name("conpty-close".into())
        .spawn(move || {
            close();
            let _ = tx.send(());
        });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(bound)
        .ok()
        .map(|()| t.elapsed().as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn read_all(mut r: ChannelReader) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut v = Vec::new();
            let _ = r.read_to_end(&mut v);
            v
        })
    }

    /// M4: after the master's teardown starts, output still pending (the
    /// child's last words) reaches the attached reader through EOF instead
    /// of being drained and discarded.
    #[test]
    fn final_output_reaches_the_attached_reader() {
        let (r, mut w) = io::pipe().unwrap();
        let d = Drain::start(r).unwrap();
        let reader = read_all(d.attach().unwrap());
        w.write_all(b"before ").unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            w.write_all("日本語".as_bytes()).unwrap();
        });
        assert_eq!(d.finish(Duration::from_secs(5)).as_deref(), Some("eof"));
        writer.join().unwrap();
        assert_eq!(
            String::from_utf8(reader.join().unwrap()).unwrap(),
            "before 日本語"
        );
    }

    /// M2: a read error ends the drain but is reported as such, never as
    /// EOF.
    #[test]
    fn read_error_is_not_eof() {
        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken pipe"))
            }
        }
        let d = Drain::start(Failing).unwrap();
        assert!(d.wait(Duration::from_secs(5)));
        assert!(
            d.end().is_some_and(|e| e.starts_with("error:")),
            "{:?}",
            d.end()
        );
        let (r, w) = io::pipe().unwrap();
        let d = Drain::start(r).unwrap();
        drop(w);
        assert!(d.wait(Duration::from_secs(5)));
        assert_eq!(d.end().as_deref(), Some("eof"));
    }

    /// M4: startup output beyond the hold buffer is never dropped silently:
    /// attaching reports it as incomplete evidence.
    #[test]
    fn hold_overflow_is_reported() {
        let (r, mut w) = io::pipe().unwrap();
        let d = Drain::start(r).unwrap();
        w.write_all(&vec![b'x'; DRAIN_HOLD + 4096]).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while d.bytes() < (DRAIN_HOLD + 4096) as u64 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        let e = d.attach().err();
        assert!(
            e.is_some_and(|e| e.contains("hold buffer")),
            "overflow reported"
        );
        drop(w);
        assert!(d.wait(Duration::from_secs(5)));
    }

    /// M4: a close that hangs is reported (no duration) within its bound,
    /// and a quick one reports its duration.
    #[test]
    fn hung_close_is_bounded() {
        let t = Instant::now();
        let r = close_bounded(
            || std::thread::sleep(Duration::from_millis(1500)),
            Duration::from_millis(100),
        );
        assert_eq!(r, None);
        assert!(
            t.elapsed() < Duration::from_millis(1000),
            "{:?}",
            t.elapsed()
        );
        assert!(close_bounded(|| {}, Duration::from_secs(1)).is_some());
    }
}
