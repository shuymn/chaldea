//! The output drain (`docs/design/pty-runtime.md` §2.4), lifted from the PTY spike's
//! `os/drain.rs` (`docs/spikes/pty-hooks-spike.md`) without its hold: the drain owns the
//! PTY's output read end, reads until EOF or a read error on every path, passes every chunk
//! to the session's sink from the first byte, and reports how it ended, never confusing the
//! two. The sink exists before the backend spawns, so no output is held for a late consumer.

use std::io::{self, Read};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::backend::DrainSink;
use crate::session::{OutputEnd, lock};

/// Bytes read per call.
const READ_BUFFER: usize = 16 * 1024;

#[derive(Clone, Copy)]
/// One delivery to the sink: a chunk with the offset of its first byte, or the end.
pub(crate) enum DrainEvent<'a> {
    Bytes { offset: u64, bytes: &'a [u8] },
    End(&'a OutputEnd),
}

#[derive(Default)]
struct Progress {
    /// Bytes read and passed to the sink; kept only for `FakePty`'s startup barrier.
    #[cfg(any(test, feature = "fake"))]
    delivered: u64,
    /// Set after the sink got the end.
    end: Option<OutputEnd>,
}

#[derive(Default)]
struct DrainState {
    progress: Mutex<Progress>,
    changed: Condvar,
}

/// The `pty-drain` thread.
pub(crate) struct Drain {
    state: Arc<DrainState>,
    join: JoinHandle<()>,
}

impl Drain {
    /// Starts the drain. If the thread cannot start, `output` (moved into the closure) is
    /// dropped first, closing the read end before anything else.
    pub(crate) fn start(
        mut output: impl Read + Send + 'static,
        mut sink: DrainSink,
    ) -> io::Result<Drain> {
        let state = Arc::new(DrainState::default());
        let st = state.clone();
        let join = std::thread::Builder::new()
            .name("pty-drain".into())
            .spawn(move || {
                let mut buf = vec![0u8; READ_BUFFER];
                let mut offset = 0u64;
                let end = loop {
                    let n = match output.read(&mut buf) {
                        Ok(0) => break OutputEnd::Eof,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => break OutputEnd::Error(e.to_string()),
                        Ok(n) => n,
                    };
                    sink.deliver(DrainEvent::Bytes {
                        offset,
                        bytes: &buf[..n],
                    });
                    offset += n as u64;
                    #[cfg(any(test, feature = "fake"))]
                    {
                        lock(&st.progress).delivered = offset;
                        st.changed.notify_all();
                    }
                };
                drop(output);
                sink.deliver(DrainEvent::End(&end));
                lock(&st.progress).end = Some(end);
                st.changed.notify_all();
            })?;
        Ok(Drain { state, join })
    }

    /// Waits until the sink has processed at least `n` bytes or the output ended; false
    /// past `deadline`.
    #[cfg(any(test, feature = "fake"))]
    pub(crate) fn wait_delivered(&self, n: u64, deadline: Instant) -> bool {
        let mut p = lock(&self.state.progress);
        loop {
            if p.delivered >= n || p.end.is_some() {
                return p.delivered >= n;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            p = self
                .state
                .changed
                .wait_timeout(p, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// Waits at most `bound` for the end, then joins the thread; `None` when the thread is
    /// abandoned (it did not end in time).
    pub(crate) fn finish(self, bound: Duration) -> Option<OutputEnd> {
        let deadline = Instant::now() + bound;
        let end = {
            let mut p = lock(&self.state.progress);
            loop {
                if let Some(end) = p.end.clone() {
                    break Some(end);
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break None;
                }
                p = self
                    .state
                    .changed
                    .wait_timeout(p, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
        }?;
        // The end is published as the thread's last action; it is finishing now.
        while !self.join.is_finished() {
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let _ = self.join.join();
        Some(end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    /// Bytes, chunk offsets, and the end a recording sink got.
    type Seen = Arc<Mutex<(Vec<u8>, Vec<u64>, Option<OutputEnd>)>>;

    /// A sink that records what it gets; the returned handle reads it.
    fn recording() -> (DrainSink, Seen) {
        let seen = Arc::new(Mutex::new((Vec::new(), Vec::new(), None)));
        let s = seen.clone();
        let sink = DrainSink::test(move |e| {
            let mut g = s.lock().unwrap();
            match e {
                DrainEvent::Bytes { offset, bytes } => {
                    assert_eq!(offset, g.0.len() as u64, "offsets are contiguous");
                    assert!(g.2.is_none(), "no bytes after the end");
                    g.0.extend_from_slice(bytes);
                    g.1.push(offset);
                }
                DrainEvent::End(end) => {
                    assert!(g.2.is_none(), "one end");
                    g.2 = Some(end.clone());
                }
            }
        });
        (sink, seen)
    }

    /// PR2-CL-16: every byte read reaches the sink once, in order, from the first byte,
    /// including output written after teardown started; then the end.
    #[test]
    fn final_output_reaches_the_sink() {
        let (r, mut w) = io::pipe().unwrap();
        let (sink, seen) = recording();
        w.write_all(b"first ").unwrap();
        let d = Drain::start(r, sink).unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            w.write_all("日本語".as_bytes()).unwrap();
        });
        writer.join().unwrap();
        assert_eq!(d.finish(Duration::from_secs(5)), Some(OutputEnd::Eof));
        let g = seen.lock().unwrap();
        assert_eq!(String::from_utf8(g.0.clone()).unwrap(), "first 日本語");
        assert_eq!(g.2, Some(OutputEnd::Eof));
    }

    /// PR2-CL-16: a read error ends the drain and is reported as an error, never as EOF.
    #[test]
    fn read_error_is_not_eof() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("broken pipe"));
                }
                self.0 = true;
                buf[..2].copy_from_slice(b"ok");
                Ok(2)
            }
        }
        let (sink, seen) = recording();
        let d = Drain::start(Failing(false), sink).unwrap();
        let end = d.finish(Duration::from_secs(5));
        assert!(
            matches!(&end, Some(OutputEnd::Error(e)) if e.contains("broken pipe")),
            "{end:?}"
        );
        let g = seen.lock().unwrap();
        assert_eq!(g.0, b"ok");
        assert!(matches!(g.2, Some(OutputEnd::Error(_))));

        let (r, w) = io::pipe().unwrap();
        let (sink, _) = recording();
        let d = Drain::start(r, sink).unwrap();
        drop(w);
        assert_eq!(d.finish(Duration::from_secs(5)), Some(OutputEnd::Eof));
    }

    /// PR2-CL-16: `finish` returns by its bound when the output never ends (the bounded
    /// pseudoconsole close is the Windows close worker's, `windows::control`).
    #[test]
    fn finish_is_bounded_without_an_end() {
        let (r, w) = io::pipe().unwrap();
        let (sink, _) = recording();
        let d = Drain::start(r, sink).unwrap();
        let t = Instant::now();
        assert_eq!(d.finish(Duration::from_millis(100)), None);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        drop(w);
    }

    /// The drain keeps reading when the sink is slow to return nothing: the sink is never
    /// waited on beyond its own call, and exactly what was delivered is counted.
    #[test]
    fn delivered_bytes_are_counted() {
        let (r, mut w) = io::pipe().unwrap();
        let (sink, _) = recording();
        let d = Drain::start(r, sink).unwrap();
        w.write_all(&[b'x'; 100_000]).unwrap();
        assert!(d.wait_delivered(100_000, Instant::now() + Duration::from_secs(5)));
        assert!(!d.wait_delivered(100_001, Instant::now() + Duration::from_millis(200)));
        drop(w);
        assert_eq!(d.finish(Duration::from_secs(5)), Some(OutputEnd::Eof));
    }
}
